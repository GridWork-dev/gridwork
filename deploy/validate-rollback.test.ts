import { describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

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

function runRollbackCli(environment: Record<string, string>) {
  return Bun.spawnSync([process.execPath, "run", "deploy/validate-rollback.ts"], {
    cwd: new URL("..", import.meta.url).pathname,
    env: environment,
  });
}

function runRollbackWithFakeGcloud(
  environment: Record<string, string>,
  exitCode = 0,
) {
  const fixtureDirectory = mkdtempSync(join(tmpdir(), "gridwork-gcloud-"));
  const executable = join(fixtureDirectory, "gcloud");
  const capture = join(fixtureDirectory, "argv.txt");
  writeFileSync(
    executable,
    [
      "#!/bin/sh",
      'printf \'%s\\n\' "$@" > "$GCLOUD_ARGS_CAPTURE"',
      'printf \'%s\\n\' "$EXPECTED_REVISION"',
      'exit "$GCLOUD_EXIT_CODE"',
      "",
    ].join("\n"),
    { mode: 0o700 },
  );

  try {
    const result = runRollbackCli({
      ...environment,
      PATH: fixtureDirectory,
      GCLOUD_ARGS_CAPTURE: capture,
      EXPECTED_REVISION: revision,
      GCLOUD_EXIT_CODE: String(exitCode),
    });
    const argv = existsSync(capture)
      ? readFileSync(capture, "utf8").trimEnd().split("\n")
      : [];
    return { result, argv };
  } finally {
    rmSync(fixtureDirectory, { recursive: true, force: true });
  }
}

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

  test("looks up the validated revision with an argv array before approving it", () => {
    const { result, argv } = runRollbackWithFakeGcloud({
      SERVICE: "gridwork-site",
      REVISION: revision,
      ENVIRONMENT: "production",
      GCP_NONPROD_PROJECT: "example-staging-12345",
      GCP_PROD_PROJECT: project,
      GCP_REGION: region,
    });
    expect(result.exitCode).toBe(0);
    expect(result.stdout.toString()).toBe(`project=${project}\n`);
    expect(argv).toEqual([
      "run",
      "revisions",
      "describe",
      revision,
      "--project",
      project,
      "--region",
      region,
      "--format=value(metadata.name)",
    ]);
  });

  test("rejects raw whitespace-bearing inputs before spawning gcloud", () => {
    for (const overrides of [
      { SERVICE: "gridwork-site " },
      { REVISION: `${revision}\n` },
    ]) {
      const { result, argv } = runRollbackWithFakeGcloud({
        SERVICE: "gridwork-site",
        REVISION: revision,
        ENVIRONMENT: "production",
        GCP_NONPROD_PROJECT: "example-staging-12345",
        GCP_PROD_PROJECT: project,
        GCP_REGION: region,
        ...overrides,
      });
      expect(result.exitCode).not.toBe(0);
      expect(argv).toEqual([]);
    }
  });

  test("fails when Cloud Run cannot resolve the validated revision", () => {
    const { result, argv } = runRollbackWithFakeGcloud(
      {
        SERVICE: "gridwork-site",
        REVISION: revision,
        ENVIRONMENT: "production",
        GCP_NONPROD_PROJECT: "example-staging-12345",
        GCP_PROD_PROJECT: project,
        GCP_REGION: region,
      },
      7,
    );
    expect(result.exitCode).not.toBe(0);
    expect(argv[0]).toBe("run");
  });
});
