import { expect, test } from "bun:test";

import { parseManifest } from "./manifest";

test("rejects line breaks in manifest values that can reach GitHub outputs", () => {
  expect(() =>
    parseManifest({
      _source: "test fixture",
      repository: "GridWork-dev/gridwork",
      services: {
        "gridwork-site": {
          dockerfile: "site/Dockerfile\nother=value",
          build_context: ".",
          watched_paths: ["site/**"],
          healthcheck: "/health",
          hostnames: ["gridwork.sh"],
          runtime: "cloud-run-service",
        },
      },
    }),
  ).toThrow("service gridwork-site.dockerfile is invalid");
});
