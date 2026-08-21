import { describe, expect, test } from "bun:test";

import { parseManifest } from "./manifest";
import { createPlan, parseDigests } from "./plan";

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

describe("deployment plan allowlist", () => {
  test("constructs immutable image references for reviewed services", () => {
    expect(createPlan(manifest, { "gridwork-site": digest }, "production", registry)).toEqual({
      services: [
        {
          service: "gridwork-site",
          image: `${registry}/gridwork-site@${digest}`,
        },
      ],
      migrationJob: "",
      migrationImage: "",
    });
  });

  test("rejects unknown service keys before constructing a plan", () => {
    expect(() => createPlan(manifest, { "other-site": digest }, "staging", registry)).toThrow(
      "unknown service other-site",
    );
  });

  test("rejects malformed and empty digest maps", () => {
    expect(() =>
      createPlan(manifest, { "gridwork-site": "sha256:not-a-digest" }, "staging", registry),
    ).toThrow("invalid digest for gridwork-site");
    expect(() => createPlan(manifest, {}, "staging", registry)).toThrow(
      "DIGESTS must contain at least one service",
    );
  });

  test("rejects malformed JSON and non-object digest input", () => {
    expect(() => parseDigests("not json")).toThrow("DIGESTS must be valid JSON");
    expect(() => parseDigests("[]")).toThrow("DIGESTS must be a JSON object");
  });

  test("rejects unknown environments and registries", () => {
    expect(() => createPlan(manifest, { "gridwork-site": digest }, "preview", registry)).toThrow(
      "ENVIRONMENT must be staging or production",
    );
    expect(() =>
      createPlan(manifest, { "gridwork-site": digest }, "production", "docker.io/example"),
    ).toThrow("GCP_REGISTRY must name an Artifact Registry repository");
  });
});
