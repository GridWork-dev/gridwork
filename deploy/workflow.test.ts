import { describe, expect, test } from "bun:test";

async function workflow(name: string): Promise<string> {
  return Bun.file(new URL(`../.github/workflows/${name}.yml`, import.meta.url)).text();
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
    expect(source).toContain("CANARY_URLS<<CANARY_URLS_EOF");
    expect(source).toContain("ORIGIN_SECRETS: ${{ secrets.ORIGIN_SECRETS }}");
    expect(source).not.toContain("CANARY_TAG: ${{ env.CANARY_TAG }}");
    expect(source).toContain('CANARY_URLS: ""');
    expect(source).toContain('ORIGIN_SECRETS: ""');
    expect(source).toContain('--to-tags "${CANARY_TAG}=${pct}"');
  });

  test("rollback validates the target and sends staging Access credentials", async () => {
    const source = await workflow("rollback");
    expect(source).toContain("run: bun run deploy/validate-rollback.ts");
    expect(source).toContain('--to-revisions "${{ inputs.revision }}=100"');
    expect(source).toContain("MODE: rollback");
    expect(source).toContain("CF_ACCESS_CLIENT_ID: ${{ secrets.CF_ACCESS_CLIENT_ID }}");
    expect(source).toContain("CF_ACCESS_CLIENT_SECRET: ${{ secrets.CF_ACCESS_CLIENT_SECRET }}");
  });
});
