import { expect, test } from "bun:test";

import { parseManifest } from "./manifest";
import { serviceInputs } from "./service-inputs";

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

test("serviceInputs returns only reviewed manifest build paths", () => {
  expect(serviceInputs(manifest, "gridwork-site")).toEqual({
    dockerfile: "site/Dockerfile",
    buildContext: ".",
  });
  expect(() => serviceInputs(manifest, "other-site")).toThrow("unknown service other-site");
});
