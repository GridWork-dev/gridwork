import { describe, expect, test } from "bun:test";

import { parseManifest } from "./manifest";
import { createPlan, parseDigests, validateHoldSeconds } from "./plan";

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

const digest = `sha256:${"a".repeat(64)}`;
const registry = "us-east4-docker.pkg.dev/example-project/apps";
const sourceSha = "b".repeat(40);

describe("deployment plan allowlist", () => {
  test("constructs immutable image references for reviewed services", () => {
    expect(createPlan(manifest, { "gridwork-site": digest }, "production", registry, sourceSha)).toEqual({
      services: [
        {
          service: "gridwork-site",
          image: `${registry}/gridwork-site@${digest}`,
        },
      ],
      migrationJob: "",
      migrationImage: "",
      sourceSha,
    });
  });

  test("rejects unknown service keys before constructing a plan", () => {
    expect(() => createPlan(manifest, { "other-site": digest }, "staging", registry, sourceSha)).toThrow(
      "unknown service other-site",
    );
    expect(() =>
      createPlan(manifest, { constructor: digest }, "staging", registry, sourceSha),
    ).toThrow("unknown service constructor");
  });

  test("rejects malformed and empty digest maps", () => {
    expect(() =>
      createPlan(
        manifest,
        { "gridwork-site": "sha256:not-a-digest" },
        "staging",
        registry,
        sourceSha,
      ),
    ).toThrow("invalid digest for gridwork-site");
    expect(() => createPlan(manifest, {}, "staging", registry, sourceSha)).toThrow(
      "DIGESTS must contain at least one service",
    );
  });

  test("rejects malformed JSON and non-object digest input", () => {
    expect(() => parseDigests("not json")).toThrow("DIGESTS must be valid JSON");
    expect(() => parseDigests("[]")).toThrow("DIGESTS must be a JSON object");
  });

  test("rejects unknown environments and registries", () => {
    expect(() =>
      createPlan(manifest, { "gridwork-site": digest }, "preview", registry, sourceSha),
    ).toThrow(
      "ENVIRONMENT must be staging or production",
    );
    expect(() =>
      createPlan(
        manifest,
        { "gridwork-site": digest },
        "production",
        "docker.io/example",
        sourceSha,
      ),
    ).toThrow("GCP_REGISTRY must name an Artifact Registry repository");
  });

  test("requires an immutable source SHA and a bounded production hold", () => {
    expect(() =>
      createPlan(manifest, { "gridwork-site": digest }, "production", registry, "main"),
    ).toThrow("SOURCE_SHA must be a full lowercase Git commit SHA");
    expect(validateHoldSeconds("300")).toBe(300);
    expect(() => validateHoldSeconds("0")).toThrow("HOLD_SECONDS must be between 60 and 1200");
    expect(() => validateHoldSeconds("1.5")).toThrow("HOLD_SECONDS must be a decimal integer");
  });
});
