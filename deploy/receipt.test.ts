import { describe, expect, test } from "bun:test";

import {
  parseDeploymentReceipt,
  promotionInputs,
  rollbackImage,
} from "./receipt";

const digest = `sha256:${"a".repeat(64)}`;
const sourceSha = "b".repeat(40);
const receiptValue = {
  schema: 1,
  repository: "GridWork-dev/gridwork",
  environment: "staging",
  source_sha: sourceSha,
  deployment_run_id: "12345",
  services: [
    {
      service: "gridwork-site",
      image: `us-east4-docker.pkg.dev/gridwork-shared-99d2/apps/gridwork-site@${digest}`,
      revision: "gridwork-site-00042-abc",
    },
  ],
};

describe("deployment receipts", () => {
  test("derives production inputs only from the named successful staging receipt", () => {
    const receipt = parseDeploymentReceipt(receiptValue);
    expect(promotionInputs(receipt, "GridWork-dev/gridwork", "12345")).toEqual({
      sourceSha,
      digests: { "gridwork-site": digest },
    });
  });

  test("authorizes rollback only to a revision recorded by the requested environment", () => {
    const receipt = parseDeploymentReceipt({ ...receiptValue, environment: "production" });
    expect(
      rollbackImage(
        receipt,
        "GridWork-dev/gridwork",
        "production",
        "gridwork-site",
        "gridwork-site-00042-abc",
      ),
    ).toEndWith(`@${digest}`);
    expect(() =>
      rollbackImage(
        receipt,
        "GridWork-dev/gridwork",
        "production",
        "gridwork-site",
        "gridwork-site-00043-bad",
      ),
    ).toThrow("revision is not present in the approved deployment receipt");
  });

  test("rejects malformed and cross-repository receipts", () => {
    expect(() => parseDeploymentReceipt({ ...receiptValue, extra: true })).toThrow(
      "deployment receipt contains unknown field extra",
    );
    expect(() =>
      promotionInputs(
        parseDeploymentReceipt(receiptValue),
        "GridWork-dev/other",
        "12345",
      ),
    ).toThrow("deployment receipt repository does not match this workflow");
  });
});
