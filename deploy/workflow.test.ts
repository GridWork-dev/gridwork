import { describe, expect, test } from "bun:test";

type Mapping = Record<string, unknown>;

function mapping(value: unknown, label: string): Mapping {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(`${label} must be a mapping`);
  }
  return value as Mapping;
}

async function workflowSteps(name: string, jobName: string): Promise<Mapping[]> {
  const source = await Bun.file(new URL(`../.github/workflows/${name}.yml`, import.meta.url)).text();
  const workflow = mapping(Bun.YAML.parse(source), `${name} workflow`);
  const jobs = mapping(workflow["jobs"], `${name} jobs`);
  const job = mapping(jobs[jobName], `${name}.${jobName}`);
  const steps = job["steps"];
  if (!Array.isArray(steps) || steps.length === 0) {
    throw new Error(`${name}.${jobName}.steps must be a non-empty sequence`);
  }
  return steps.map((step, index) => mapping(step, `${name}.${jobName}.steps[${index}]`));
}

function namedStep(steps: Mapping[], name: string): Mapping {
  const matches = steps.filter((step) => step["name"] === name);
  if (matches.length !== 1) throw new Error(`expected exactly one workflow step named ${name}`);
  return matches[0]!;
}

function stepIndex(steps: Mapping[], name: string): number {
  const index = steps.findIndex((step) => step["name"] === name);
  if (index === -1) throw new Error(`missing workflow step ${name}`);
  return index;
}

function usesIndex(steps: Mapping[], action: string): number {
  const index = steps.findIndex((step) => step["uses"] === action);
  if (index === -1) throw new Error(`missing workflow action ${action}`);
  return index;
}

function stepEnvironment(step: Mapping): Mapping {
  return mapping(step["env"], `${String(step["name"])}.env`);
}

function runBody(step: Mapping): string {
  const run = step["run"];
  if (typeof run !== "string") throw new Error(`${String(step["name"])}.run must be a string`);
  return run;
}

function artifactStep(steps: Mapping[], artifactName: string): Mapping {
  const matches = steps.filter((step) => {
    if (typeof step["uses"] !== "string" || !step["uses"].startsWith("actions/upload-artifact@")) {
      return false;
    }
    return mapping(step["with"], "upload-artifact.with")["name"] === artifactName;
  });
  if (matches.length !== 1) throw new Error(`expected exactly one upload for ${artifactName}`);
  return matches[0]!;
}

describe("deployment workflow safety state transitions", () => {
  test("publication runs gates and the required host preparation before image build", async () => {
    const steps = await workflowSteps("publish-image", "publish");
    const gates = stepIndex(steps, "gates");
    const prepare = stepIndex(steps, "prepare build context");
    const build = stepIndex(steps, "build and push by digest");

    expect(gates).toBeLessThan(prepare);
    expect(prepare).toBeLessThan(build);
    expect(runBody(namedStep(steps, "gates"))).toBe("bash deploy/gates.sh");
    expect(runBody(namedStep(steps, "prepare build context"))).toBe(
      "bash deploy/prepare-build.sh",
    );

    const setup = steps.find(
      (step) => typeof step["uses"] === "string" && step["uses"].startsWith("oven-sh/setup-bun@"),
    );
    expect(mapping(setup, "setup-bun step")["with"]).toMatchObject({
      "bun-version-file": "${{ vars.BUN_VERSION_FILE }}",
    });
    expect(mapping(namedStep(steps, "build and push by digest")["with"], "build.with")).toMatchObject({
      tags: "${{ vars.GCP_REGISTRY }}/${{ matrix.service }}:${{ github.sha }}",
    });
  });

  test("gridwork explicitly declares that no host build preparation is needed", async () => {
    const source = await Bun.file(new URL("./prepare-build.sh", import.meta.url)).text();
    expect(source).toBe(
      "# No host-side preparation is required before Docker receives the build context.\n",
    );
  });

  test("staging runs migration before deploy and wires Access credentials to its smoke", async () => {
    const steps = await workflowSteps("deploy-staging", "deploy");
    const update = stepIndex(steps, "point the migration Job at this digest");
    const execute = stepIndex(steps, "execute the migration Job");
    const deploy = stepIndex(steps, "deploy each service to its digest");
    expect(update).toBeLessThan(execute);
    expect(execute).toBeLessThan(deploy);

    const smoke = namedStep(steps, "origin + Access bypass suite");
    expect(runBody(smoke)).toBe("bun run deploy/smoke.ts");
    expect(stepEnvironment(smoke)).toMatchObject({
      ENVIRONMENT: "staging",
      MODE: "staging",
      CF_ACCESS_CLIENT_ID: "${{ secrets.CF_ACCESS_CLIENT_ID }}",
      CF_ACCESS_CLIENT_SECRET: "${{ secrets.CF_ACCESS_CLIENT_SECRET }}",
    });
    expect(mapping(artifactStep(steps, "staging-rollback-revisions")["with"], "artifact.with")).toMatchObject({
      path: "rollback-revisions.txt",
      "if-no-files-found": "error",
    });
  });

  test("production validates holds, smokes both direct canary phases, and restores failures", async () => {
    const steps = await workflowSteps("deploy-production", "rollout");
    const validate = namedStep(steps, "validate the rollout inputs");
    const resolve = stepIndex(steps, "resolve the tagged revision URLs");
    const preMigration = stepIndex(steps, "pre-migration boot + origin checks");
    const postMigration = stepIndex(steps, "smoke the tagged revision");
    const shift = stepIndex(steps, "progressive traffic shift");
    const publicSmoke = stepIndex(steps, "post-deploy synthetic flows");
    const restore = stepIndex(steps, "restore traffic if the rollout did not complete");

    expect(stepIndex(steps, "validate the rollout inputs")).toBeLessThan(
      usesIndex(steps, "google-github-actions/auth@7c6bc770dae815cd3e89ee6cdf493a5fab2cc093"),
    );
    expect(resolve).toBeLessThan(preMigration);
    expect(preMigration).toBeLessThan(postMigration);
    expect(postMigration).toBeLessThan(shift);
    expect(shift).toBeLessThan(publicSmoke);
    expect(publicSmoke).toBeLessThan(restore);
    expect(stepEnvironment(validate)["HOLD"]).toBe("${{ inputs.hold_seconds }}");
    expect(runBody(validate)).toContain('if [ "$HOLD" -lt 30 ] || [ "$HOLD" -gt 900 ]');

    for (const [name, mode] of [
      ["pre-migration boot + origin checks", "pre-migration"],
      ["smoke the tagged revision", "post-migration"],
    ] as const) {
      const smoke = namedStep(steps, name);
      expect(runBody(smoke)).toBe("bun run deploy/smoke.ts");
      expect(stepEnvironment(smoke)).toMatchObject({
        ENVIRONMENT: "production",
        MODE: mode,
        ORIGIN_SECRETS: "${{ secrets.ORIGIN_SECRETS }}",
      });
    }

    const restoreStep = namedStep(steps, "restore traffic if the rollout did not complete");
    expect(restoreStep["id"]).toBe("restore");
    expect(restoreStep["if"]).toBe(
      "(failure() || cancelled()) && hashFiles('rollback-revisions.txt') != ''",
    );
    expect(runBody(restoreStep)).toContain('--to-revisions "${rev}=100"');
    expect(runBody(restoreStep)).toContain("exit 1");

    const evidence = namedStep(steps, "deployment evidence");
    expect(evidence["if"]).toBe(
      "always() && steps.plan.outcome == 'success' && hashFiles('rollback-revisions.txt') != ''",
    );
    expect(stepEnvironment(evidence)).toMatchObject({
      ROLLOUT: "${{ steps.shift.outcome }}",
      RESTORE: "${{ steps.restore.outcome }}",
    });
    expect(runBody(evidence)).toContain("jq -n");
    expect(runBody(evidence)).toContain("workflow_commit:$workflow_commit");
    expect(runBody(evidence)).not.toContain("${{");
  });

  test("rollback validates, shifts, and then smokes the requested service", async () => {
    const steps = await workflowSteps("rollback", "rollback");
    const validate = stepIndex(steps, "validate service + revision against strict allowlists");
    const shift = stepIndex(steps, "shift traffic to the known-good revision");
    const smoke = stepIndex(steps, "verify the public flow through Cloudflare");
    expect(validate).toBeLessThan(shift);
    expect(shift).toBeLessThan(smoke);

    const validateStep = namedStep(steps, "validate service + revision against strict allowlists");
    expect(runBody(validateStep)).toBe('bun run deploy/validate-rollback.ts >> "$GITHUB_OUTPUT"');
    const smokeStep = namedStep(steps, "verify the public flow through Cloudflare");
    expect(runBody(smokeStep)).toBe("bun run deploy/smoke.ts");
    expect(stepEnvironment(smokeStep)).toMatchObject({
      MODE: "rollback",
      SERVICE: "${{ inputs.service }}",
      CF_ACCESS_CLIENT_ID: "${{ secrets.CF_ACCESS_CLIENT_ID }}",
      CF_ACCESS_CLIENT_SECRET: "${{ secrets.CF_ACCESS_CLIENT_SECRET }}",
    });
  });

  test("routes runner expressions through step fields instead of shell source", async () => {
    const jobs = [
      ["publish-image", "select"],
      ["publish-image", "publish"],
      ["publish-image", "collect"],
      ["deploy-staging", "deploy"],
      ["deploy-production", "rollout"],
      ["rollback", "rollback"],
    ] as const;
    for (const [workflow, job] of jobs) {
      for (const step of await workflowSteps(workflow, job)) {
        if (typeof step["run"] === "string") expect(step["run"]).not.toContain("${{");
      }
    }
  });

  test("keeps the no-root-lockfile dependency guard conditional and fail-closed", async () => {
    const jobs = [
      ["publish-image", "select"],
      ["publish-image", "publish"],
      ["publish-image", "collect"],
      ["deploy-staging", "deploy"],
      ["deploy-production", "rollout"],
      ["rollback", "rollback"],
    ] as const;
    const runBodies: string[] = [];
    for (const [workflow, job] of jobs) {
      for (const step of await workflowSteps(workflow, job)) {
        if (typeof step["run"] === "string") runBodies.push(step["run"]);
      }
    }
    const installBodies = runBodies.filter((run) => run.includes("bun install --frozen-lockfile"));
    expect(installBodies).toHaveLength(5);
    for (const run of installBodies) {
      expect(run).toContain("elif grep -hoE");
      expect(run).toContain("this repo has no root bun.lock");
    }
    expect(runBodies).not.toContain("bun install --frozen-lockfile");
  });
});
