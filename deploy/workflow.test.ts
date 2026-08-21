import { describe, expect, test } from "bun:test";

async function workflow(name: string): Promise<string> {
  return Bun.file(new URL(`../.github/workflows/${name}.yml`, import.meta.url)).text();
}

describe("deployment workflow safety state transitions", () => {
  test("publication rebuilds through the per-repo gates without persisted checkout credentials", async () => {
    const source = await workflow("publish-image");
    expect(source).toContain("run: bash deploy/gates.sh");
    expect(source).toContain("persist-credentials: false");
    expect(source).not.toContain("<<JSON");
  });

  test("staging restores latest traffic and records a promotion receipt", async () => {
    const source = await workflow("deploy-staging");
    expect(source).toContain("--to-latest");
    expect(source).toContain("deployment-receipt.json");
    expect(source).toContain("SOURCE_SHA:");
  });

  test("production consumes staging evidence and recovers failed traffic shifts", async () => {
    const source = await workflow("deploy-production");
    expect(source).toContain("staging_run_id:");
    expect(source).not.toContain("      digests:");
    expect(source).toContain("if: failure()");
    expect(source).toContain("expected exactly one 100% traffic revision");
    expect(source).toContain('echo "CANARY_TAG=$tag" >> "$GITHUB_ENV"');
    expect(source).toContain("resolve the tagged revision URLs");
    expect(source).toContain("CANARY_URLS<<CANARY_URLS_EOF");
    expect(source).toContain("ORIGIN_SECRETS: ${{ secrets.ORIGIN_SECRETS }}");
    expect(source).not.toContain("CANARY_TAG: ${{ env.CANARY_TAG }}");
    expect(source).toContain('CANARY_URLS: ""');
    expect(source).toContain('ORIGIN_SECRETS: ""');
    expect(source).toContain('--to-tags "${CANARY_TAG}=${pct}"');
  });

  test("rollback requires an approved deployment receipt and staging Access credentials", async () => {
    const source = await workflow("rollback");
    expect(source).toContain("deployment_run_id:");
    expect(source).toContain("deployment-receipt.json");
    expect(source).toContain("CF_ACCESS_CLIENT_ID:");
    expect(source).toContain("CF_ACCESS_CLIENT_SECRET:");
  });
});
