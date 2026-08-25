import { describe, expect, test } from "bun:test";

import { parseManifest } from "./manifest";

function runOutput(value: string) {
  return Bun.spawnSync(
    [
      process.execPath,
      "-e",
      'import { output } from "./deploy/manifest.ts"; output("probe", process.env.OUTPUT_VALUE ?? "");',
    ],
    {
      cwd: new URL("..", import.meta.url).pathname,
      env: { OUTPUT_VALUE: value },
    },
  );
}

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

describe("GitHub output encoding", () => {
  test("emits one key-value line for a single-line value", () => {
    const result = runOutput("value");
    expect(result.exitCode).toBe(0);
    expect(result.stdout.toString()).toBe("probe=value\n");
  });

  test("rejects carriage returns and newlines before writing runner control data", () => {
    for (const value of ["value\nforged=true", "value\rforged=true"]) {
      const result = runOutput(value);
      expect(result.exitCode).not.toBe(0);
      expect(result.stdout.toString()).toBe("");
    }
  });
});
