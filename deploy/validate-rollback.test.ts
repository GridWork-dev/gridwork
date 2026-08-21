import { describe, expect, test } from "bun:test";

import { parseManifest } from "./manifest";
import { validateRollback } from "./validate-rollback";

const manifest = parseManifest({
  _source: "test fixture",
  repository: "GridWork-dev/gridwork",
  services: {
    "gridwork-site": {
      dockerfile: "site/Dockerfile",
      build_context: ".",
      watched_paths: ["site/**"],
      healthcheck: "/health",
      hostnames: ["gridwork.sh"],
      runtime: "cloud-run-service",
    },
  },
});

const project = "example-prod-12345";
const region = "us-east4";
const revision = "gridwork-site-00042-abc";

describe("validateRollback", () => {
  test("accepts an allowlisted service only after the exact revision is described", () => {
    const calls: string[][] = [];
    const result = validateRollback(
      manifest,
      "gridwork-site",
      revision,
      "production",
      project,
      region,
      (arguments_) => {
        calls.push(arguments_);
        return revision;
      },
    );

    expect(result).toEqual({ project });
    expect(calls).toEqual([
      [
        "run",
        "revisions",
        "describe",
        revision,
        "--project",
        project,
        "--region",
        region,
        "--format=value(metadata.name)",
      ],
    ]);
  });

  test("rejects unowned services and malformed revisions before calling gcloud", () => {
    const never = () => {
      throw new Error("gcloud must not run");
    };
    expect(() =>
      validateRollback(manifest, "other-site", revision, "production", project, region, never),
    ).toThrow("unknown service other-site");
    expect(() =>
      validateRollback(
        manifest,
        "gridwork-site",
        "other-site-00042-abc",
        "production",
        project,
        region,
        never,
      ),
    ).toThrow("REVISION must belong to gridwork-site");
  });

  test("fails closed when the revision does not exist", () => {
    expect(() =>
      validateRollback(
        manifest,
        "gridwork-site",
        revision,
        "staging",
        project,
        region,
        () => {
          throw new Error("not found");
        },
      ),
    ).toThrow(`revision ${revision} does not exist`);
  });

  test("rejects invalid environment, project, and region inputs", () => {
    const exists = () => revision;
    expect(() =>
      validateRollback(manifest, "gridwork-site", revision, "preview", project, region, exists),
    ).toThrow("ENVIRONMENT must be staging or production");
    expect(() =>
      validateRollback(manifest, "gridwork-site", revision, "production", "bad", region, exists),
    ).toThrow("GCP_PROJECT is invalid");
    expect(() =>
      validateRollback(manifest, "gridwork-site", revision, "production", project, "bad", exists),
    ).toThrow("GCP_REGION is invalid");
  });
});
