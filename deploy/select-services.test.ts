import { describe, expect, test } from "bun:test";

import type { ServiceManifest } from "./manifest";
import { selectServiceKeys } from "./select-services";

const manifest: ServiceManifest = {
  _source: "test fixture",
  repository: "GridWork-dev/gridwork",
  services: {
    "gridwork-site": {
      dockerfile: "site/Dockerfile",
      build_context: ".",
      watched_paths: ["site/**", "site.config.ts"],
      healthcheck: "/health",
      hostnames: ["gridwork.sh"],
      runtime: "cloud-run-service",
    },
    "docs-site": {
      dockerfile: "docs/Dockerfile",
      build_context: ".",
      watched_paths: ["docs/**"],
      healthcheck: "/healthz",
      hostnames: ["docs.example.com"],
      runtime: "cloud-run-service",
    },
  },
};

describe("selectServiceKeys", () => {
  test("selects only services whose watched paths changed", () => {
    expect(selectServiceKeys(manifest, ["site/app/page.tsx", "README.md"])).toEqual([
      "gridwork-site",
    ]);
    expect(selectServiceKeys(manifest, ["site.config.ts"])).toEqual(["gridwork-site"]);
    expect(selectServiceKeys(manifest, ["README.md"])).toEqual([]);
  });

  test("an explicit service bypasses path selection but remains allowlisted", () => {
    expect(selectServiceKeys(manifest, [], "docs-site")).toEqual(["docs-site"]);
    expect(() => selectServiceKeys(manifest, [], "not-owned")).toThrow(
      "unknown service not-owned",
    );
  });

  test("an absent before revision selects every service for a manual publish", () => {
    expect(selectServiceKeys(manifest, null)).toEqual(["docs-site", "gridwork-site"]);
  });
});
