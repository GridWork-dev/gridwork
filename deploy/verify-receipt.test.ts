import { describe, expect, test } from "bun:test";

function runReceiptGate(environment: Record<string, string>) {
  return Bun.spawnSync([process.execPath, "run", "deploy/verify-receipt.ts"], {
    cwd: new URL("..", import.meta.url).pathname,
    env: environment,
  });
}

describe("operator deploy receipt gate", () => {
  test("rejects an unset or shorter-than-32-character configured receipt", () => {
    const submitted = "r".repeat(32);
    expect(runReceiptGate({ RECEIPT: submitted }).exitCode).not.toBe(0);
    expect(
      runReceiptGate({ RECEIPT: "short", PROD_DEPLOY_RECEIPT: "short" }).exitCode,
    ).not.toBe(0);
  });

  test("does not trim either side before comparison", () => {
    const expected = "r".repeat(32);
    expect(
      runReceiptGate({
        RECEIPT: `${expected} `,
        PROD_DEPLOY_RECEIPT: expected,
      }).exitCode,
    ).not.toBe(0);
  });
});
