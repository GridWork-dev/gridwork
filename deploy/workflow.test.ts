import { describe, expect, test } from "bun:test";

async function workflow(name: string): Promise<string> {
  return Bun.file(new URL(`../.github/workflows/${name}.yml`, import.meta.url)).text();
}

function runBodies(source: string): string[] {
  const lines = source.split("\n");
  const bodies: string[] = [];
  for (let index = 0; index < lines.length; index++) {
    const line = lines[index]!;
    const match = /^(\s*)run:\s*(.*)$/.exec(line);
    if (!match) continue;
    const indent = match[1]!.length;
    if (match[2] !== "|") {
      bodies.push(match[2]!);
      continue;
    }
    const body: string[] = [];
    for (index += 1; index < lines.length; index++) {
      const bodyLine = lines[index]!;
      if (bodyLine.length > 0 && bodyLine.search(/\S/) <= indent) {
        index -= 1;
        break;
      }
      body.push(bodyLine);
    }
    bodies.push(body.join("\n"));
  }
  return bodies;
}

function count(source: string, value: string): number {
  return source.split(value).length - 1;
}

describe("deployment workflow safety state transitions", () => {
  test("publication runs gates and the required host preparation before image build", async () => {
    const source = await workflow("publish-image");
    const gates = source.indexOf("run: bash deploy/gates.sh");
    const prepare = source.indexOf("run: bash deploy/prepare-build.sh");
    const build = source.indexOf("- name: build and push by digest");

    expect(gates).toBeGreaterThan(-1);
    expect(prepare).toBeGreaterThan(gates);
    expect(build).toBeGreaterThan(prepare);
    expect(source).toContain("bun-version-file: ${{ vars.BUN_VERSION_FILE }}");
    expect(source).toContain("${{ vars.GCP_REGISTRY }}/${{ matrix.service }}");
    expect(source).not.toContain("if [ -f deploy/prepare-build.sh ]");
    expect(source).toContain("jq -c --arg k \"$svc\" --arg v");
    expect(source).toContain('case "$REGISTRY" in');
    expect(source).toContain('"$REGISTRY_HOST"/*)');
  });

  test("gridwork explicitly declares that no host build preparation is needed", async () => {
    const source = await Bun.file(
      new URL("./prepare-build.sh", import.meta.url),
    ).text();
    expect(source).toBe(
      "# No host-side preparation is required before Docker receives the build context.\n",
    );
  });

  test("staging runs migration before deploy and always supplies Access credentials", async () => {
    const source = await workflow("deploy-staging");
    const update = source.indexOf("- name: point the migration Job at this digest");
    const execute = source.indexOf("- name: execute the migration Job");
    const deploy = source.indexOf("- name: deploy each service to its digest");

    expect(update).toBeGreaterThan(-1);
    expect(execute).toBeGreaterThan(update);
    expect(deploy).toBeGreaterThan(execute);
    expect(source).toContain("GCP_REGISTRY: ${{ vars.GCP_REGISTRY }}");
    expect(source).toContain("CF_ACCESS_CLIENT_ID: ${{ secrets.CF_ACCESS_CLIENT_ID }}");
    expect(source).toContain("CF_ACCESS_CLIENT_SECRET: ${{ secrets.CF_ACCESS_CLIENT_SECRET }}");
    expect(source).toContain("run: bun run deploy/smoke.ts");
    expect(source).toContain("MODE: staging");
    expect(source).toContain("name: staging-rollback-revisions");
    expect(source).toContain("rollback_revisions:$rollback");
  });

  test("production smokes the direct tagged revision before public traffic", async () => {
    const source = await workflow("deploy-production");
    const resolve = source.indexOf("- name: resolve the tagged revision URLs");
    const preMigration = source.indexOf("- name: pre-migration boot + origin checks");
    const postMigration = source.indexOf("- name: smoke the tagged revision");
    const shift = source.indexOf("- name: progressive traffic shift");
    const publicSmoke = source.indexOf("- name: post-deploy synthetic flows");

    expect(resolve).toBeGreaterThan(-1);
    expect(preMigration).toBeGreaterThan(resolve);
    expect(postMigration).toBeGreaterThan(preMigration);
    expect(shift).toBeGreaterThan(postMigration);
    expect(publicSmoke).toBeGreaterThan(shift);
    expect(source).toContain('echo "CANARY_TAG=$tag" >> "$GITHUB_ENV"');
    expect(source).toContain("printf 'CANARY_URLS=%s\\n' \"$urls\" >> \"$GITHUB_ENV\"");
    expect(source).toContain("ORIGIN_SECRETS: ${{ secrets.ORIGIN_SECRETS }}");
    expect(source).not.toContain("CANARY_TAG: ${{ env.CANARY_TAG }}");
    expect(source).toContain('CANARY_URLS: ""');
    expect(source).toContain('ORIGIN_SECRETS: ""');
    expect(source).toContain('--to-tags "${CANARY_TAG}=${pct}"');
    expect(source).toContain("staging_run_id:");
    expect(source).toContain("run-id: ${{ inputs.staging_run_id }}");
    expect(source).toContain('if [ "$HOLD" -lt 30 ] || [ "$HOLD" -gt 900 ]');
    expect(source).toContain("(failure() || cancelled())");
    expect(source).toContain("name: production-rollback-revisions");
    expect(source).toContain("staging_run_id:$staging_run_id");
  });

  test("rollback validates the target and sends staging Access credentials", async () => {
    const source = await workflow("rollback");
    expect(source).toContain("run: bun run deploy/validate-rollback.ts");
    expect(source).toContain('--to-revisions "${REVISION}=100"');
    expect(source).toContain("MODE: rollback");
    expect(source).toContain("CF_ACCESS_CLIENT_ID: ${{ secrets.CF_ACCESS_CLIENT_ID }}");
    expect(source).toContain("CF_ACCESS_CLIENT_SECRET: ${{ secrets.CF_ACCESS_CLIENT_SECRET }}");
    expect(source).toContain("evidence_run_id:");
    expect(source).toContain("run-id: ${{ inputs.evidence_run_id }}");
    expect(source).toContain("name: ${{ inputs.environment }}-rollback-revisions");
    expect(source).toContain("traffic_shift: env.SHIFT_OUTCOME");
  });

  test("routes runner expressions through env instead of shell source", async () => {
    for (const name of [
      "publish-image",
      "deploy-staging",
      "deploy-production",
      "rollback",
    ]) {
      for (const body of runBodies(await workflow(name))) {
        expect(body).not.toContain("${{");
      }
    }
  });

  test("keeps the no-root-lockfile dependency guard conditional and fail-closed", async () => {
    const sources = await Promise.all([
      workflow("publish-image"),
      workflow("deploy-staging"),
      workflow("deploy-production"),
      workflow("rollback"),
    ]);
    const combined = sources.join("\n");
    expect(count(combined, "\n            bun install --frozen-lockfile")).toBe(5);
    expect(count(combined, "\n          elif grep -hoE")).toBe(5);
    expect(count(combined, "this repo has no root bun.lock")).toBeGreaterThanOrEqual(5);
    expect(combined).not.toContain("run: bun install --frozen-lockfile");
  });
});
