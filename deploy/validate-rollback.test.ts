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
  test("selects the target project only after validating the service and revision", () => {
    const result = validateRollback(
      manifest,
      "gridwork-site",
      revision,
      "production",
      "example-staging-12345",
      project,
      region,
    );

    expect(result).toEqual({ project });
  });

  test("rejects unowned services and inherited object keys", () => {
    expect(() =>
      validateRollback(
        manifest,
        "other-site",
        revision,
        "production",
        "example-staging-12345",
        project,
        region,
      ),
    ).toThrow("unknown service other-site");
    expect(() =>
      validateRollback(
        manifest,
        "constructor",
        "constructor-00042-abc",
        "production",
        "example-staging-12345",
        project,
        region,
      ),
    ).toThrow("unknown service constructor");
  });

  test("rejects malformed revisions", () => {
    expect(() =>
      validateRollback(
        manifest,
        "gridwork-site",
        "other-site-00042-abc",
        "production",
        "example-staging-12345",
        project,
        region,
      ),
    ).toThrow("REVISION must belong to gridwork-site");
  });

  test("rejects invalid environment, project, and region inputs", () => {
    expect(() =>
      validateRollback(manifest, "gridwork-site", revision, "preview", project, project, region),
    ).toThrow("ENVIRONMENT must be staging or production");
    expect(() =>
      validateRollback(manifest, "gridwork-site", revision, "production", "bad", project, region),
    ).toThrow("GCP_NONPROD_PROJECT is invalid");
    expect(() =>
      validateRollback(manifest, "gridwork-site", revision, "production", project, "bad", region),
    ).toThrow("GCP_PROD_PROJECT is invalid");
    expect(() =>
      validateRollback(manifest, "gridwork-site", revision, "production", project, project, "bad"),
    ).toThrow("GCP_REGION is invalid");
  });
});
