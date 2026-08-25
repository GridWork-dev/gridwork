import { describe, expect, test } from "bun:test";

import {
  evaluateObservation,
  observationUrl,
  verifyTrafficAllocation,
  runObservation,
  type ObservationSample,
} from "./observe";

function samples(count: number, durationMs: number, status = 200): ObservationSample[] {
  return Array.from({ length: count }, () => ({ status, durationMs }));
}

describe("observation gate", () => {
  test("accepts a healthy sample window", () => {
    expect(evaluateObservation(samples(20, 150), 0.01, 2_000)).toEqual({
      samples: 20,
      errors: 0,
      errorRate: 0,
      p95Ms: 150,
    });
  });

  test("fails on an error-rate or p95 breach", () => {
    const withError = [...samples(19, 100), ...samples(1, 100, 503)];
    expect(() => evaluateObservation(withError, 0.01, 2_000)).toThrow(
      "error rate 0.0500 breached 0.0100",
    );
    expect(() => evaluateObservation(samples(20, 2_001), 0.01, 2_000)).toThrow(
      "p95 2001ms breached 2000ms",
    );
  });

  test("rejects an empty sample window", () => {
    expect(() => evaluateObservation([], 0.01, 2_000)).toThrow(
      "observation produced no samples",
    );
  });

  test("accepts only the production Cloudflare hostname", () => {
    expect(observationUrl("production", "25", 3)).toBe(
      "https://gridwork.sh/health?observe=25-3",
    );
    expect(() => observationUrl("staging", "5", 0)).toThrow(
      "ENVIRONMENT must be production",
    );
    expect(() => observationUrl("preview", "5", 0)).toThrow(
      "ENVIRONMENT must be production",
    );
    expect(() => observationUrl("production", "10", 0)).toThrow(
      "STEP must be 5, 25, or 100",
    );
  });

  test("samples the public path and returns a gate summary", async () => {
    const seen: string[] = [];
    const fetcher = async (url: string | URL | Request) => {
      seen.push(String(url));
      return new Response(null, { status: 200 });
    };

    const result = await runObservation("production", "5", fetcher, 4, 0.01, 2_000);
    expect(result.samples).toBe(4);
    expect(result.errors).toBe(0);
    expect(seen).toEqual([
      "https://gridwork.sh/health?observe=5-0",
      "https://gridwork.sh/health?observe=5-1",
      "https://gridwork.sh/health?observe=5-2",
      "https://gridwork.sh/health?observe=5-3",
    ]);
  });
});

test("verifies the canary percentage in the requested project and region", () => {
  const calls: string[][] = [];
  verifyTrafficAllocation(
    ["gridwork-site"],
    "5",
    "r123",
    "gridwork-prod-99d2",
    "us-east4",
    (arguments_) => {
      calls.push(arguments_);
      return JSON.stringify({ status: { traffic: [{ tag: "r123", percent: 5 }] } });
    },
  );
  expect(calls).toEqual([
    [
      "run",
      "services",
      "describe",
      "gridwork-site",
      "--project",
      "gridwork-prod-99d2",
      "--region",
      "us-east4",
      "--format=json",
    ],
  ]);
  expect(() =>
    verifyTrafficAllocation(
      ["gridwork-site"],
      "25",
      "r123",
      "gridwork-prod-99d2",
      "us-east4",
      () => JSON.stringify({ status: { traffic: [{ tag: "r123", percent: 5 }] } }),
    ),
  ).toThrow("gridwork-site canary traffic is 5%, expected 25%");
});
