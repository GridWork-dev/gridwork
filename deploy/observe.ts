import { fetchWithTimeout, type Fetcher } from "./http";
import { reportCliError } from "./manifest";

const PUBLIC_ORIGINS = {
  production: "https://gridwork.sh",
  staging: "https://gridwork.gwstg.dev",
} as const;

export const OBSERVATION_SAMPLES = 20;
export const MAX_ERROR_RATE = 0.01;
export const MAX_P95_MS = 2_000;
const REQUEST_TIMEOUT_MS = 10_000;

export type ObservationSample = { status: number; durationMs: number };
export type ObservationSummary = {
  samples: number;
  errors: number;
  errorRate: number;
  p95Ms: number;
};

function assertEnvironment(value: string): asserts value is keyof typeof PUBLIC_ORIGINS {
  if (value !== "staging" && value !== "production") {
    throw new Error("ENVIRONMENT must be staging or production");
  }
}

function assertStep(value: string): asserts value is "5" | "25" | "100" {
  if (value !== "5" && value !== "25" && value !== "100") {
    throw new Error("STEP must be 5, 25, or 100");
  }
}

export function observationUrl(environment: string, step: string, sample: number): string {
  assertEnvironment(environment);
  assertStep(step);
  if (!Number.isSafeInteger(sample) || sample < 0) throw new Error("sample index is invalid");
  return `${PUBLIC_ORIGINS[environment]}/health?observe=${step}-${sample}`;
}

export function evaluateObservation(
  window: ObservationSample[],
  maximumErrorRate: number,
  maximumP95Ms: number,
): ObservationSummary {
  if (window.length === 0) throw new Error("observation produced no samples");
  if (maximumErrorRate < 0 || maximumErrorRate > 1) throw new Error("error threshold is invalid");
  if (maximumP95Ms < 0 || !Number.isFinite(maximumP95Ms)) {
    throw new Error("latency threshold is invalid");
  }

  const errors = window.filter((sample) => sample.status !== 200).length;
  const errorRate = errors / window.length;
  const durations = window.map((sample) => sample.durationMs).sort((a, b) => a - b);
  const p95Index = Math.max(0, Math.ceil(durations.length * 0.95) - 1);
  const p95Ms = Math.round(durations[p95Index]!);

  if (errorRate > maximumErrorRate) {
    throw new Error(
      `error rate ${errorRate.toFixed(4)} breached ${maximumErrorRate.toFixed(4)}`,
    );
  }
  if (p95Ms > maximumP95Ms) {
    throw new Error(`p95 ${p95Ms}ms breached ${maximumP95Ms}ms`);
  }
  return { samples: window.length, errors, errorRate, p95Ms };
}

export async function runObservation(
  environment: string,
  step: string,
  fetcher: Fetcher = fetch,
  sampleCount = OBSERVATION_SAMPLES,
  maximumErrorRate = MAX_ERROR_RATE,
  maximumP95Ms = MAX_P95_MS,
): Promise<ObservationSummary> {
  if (!Number.isSafeInteger(sampleCount) || sampleCount < 1 || sampleCount > 100) {
    throw new Error("sample count must be between 1 and 100");
  }

  const requests = Array.from({ length: sampleCount }, async (_, index) => {
    const started = performance.now();
    try {
      const response = await fetchWithTimeout(
        observationUrl(environment, step, index),
        { method: "GET", redirect: "manual", headers: { accept: "application/json" } },
        REQUEST_TIMEOUT_MS,
        fetcher,
      );
      return { status: response.status, durationMs: performance.now() - started };
    } catch {
      return { status: 0, durationMs: performance.now() - started };
    }
  });
  return evaluateObservation(await Promise.all(requests), maximumErrorRate, maximumP95Ms);
}

async function main(): Promise<void> {
  const environment = process.env["ENVIRONMENT"]?.trim();
  const step = process.env["STEP"]?.trim();
  if (!environment) throw new Error("ENVIRONMENT is required");
  if (!step) throw new Error("STEP is required");
  const summary = await runObservation(environment, step);
  process.stdout.write(
    `observe step=${step} samples=${summary.samples} errors=${summary.errors} p95_ms=${summary.p95Ms}\n`,
  );
}

if (import.meta.main) {
  main().catch(reportCliError);
}
