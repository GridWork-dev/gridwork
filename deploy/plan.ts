import {
  output,
  readManifest,
  reportCliError,
  requireService,
  type ServiceManifest,
} from "./manifest";
import { promotionInputs, readDeploymentReceipt } from "./receipt";
import { validateDeploymentConfig } from "./policy";

const DIGEST = /^sha256:[0-9a-f]{64}$/;
const ARTIFACT_REGISTRY = /^[a-z0-9][a-z0-9.-]*\.pkg\.dev\/[a-z][a-z0-9-]{4,28}[a-z0-9]\/[a-z][a-z0-9._-]{0,62}$/;
const COMMIT = /^[0-9a-f]{40}$/;

type Environment = "staging" | "production";

export type DeploymentPlan = {
  services: Array<{ service: string; image: string }>;
  migrationJob: string;
  migrationImage: string;
  sourceSha: string;
};

export function validateHoldSeconds(value: string): number {
  if (!/^[0-9]+$/.test(value)) throw new Error("HOLD_SECONDS must be a decimal integer");
  const seconds = Number(value);
  if (!Number.isSafeInteger(seconds) || seconds < 60 || seconds > 1_200) {
    throw new Error("HOLD_SECONDS must be between 60 and 1200");
  }
  return seconds;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function parseDigests(value: string): Record<string, string> {
  let parsed: unknown;
  try {
    parsed = JSON.parse(value) as unknown;
  } catch {
    throw new Error("DIGESTS must be valid JSON");
  }
  if (!isRecord(parsed)) throw new Error("DIGESTS must be a JSON object");

  const digests: Record<string, string> = {};
  for (const [service, digest] of Object.entries(parsed)) {
    if (typeof digest !== "string") throw new Error(`invalid digest for ${service}`);
    digests[service] = digest;
  }
  return digests;
}

function parseEnvironment(value: string): Environment {
  if (value !== "staging" && value !== "production") {
    throw new Error("ENVIRONMENT must be staging or production");
  }
  return value;
}

export function createPlan(
  manifest: ServiceManifest,
  digests: Record<string, string>,
  environmentValue: string,
  registry: string,
  sourceSha: string,
): DeploymentPlan {
  parseEnvironment(environmentValue);
  if (!COMMIT.test(sourceSha)) {
    throw new Error("SOURCE_SHA must be a full lowercase Git commit SHA");
  }
  if (!ARTIFACT_REGISTRY.test(registry)) {
    throw new Error("GCP_REGISTRY must name an Artifact Registry repository");
  }

  const keys = Object.keys(digests).sort();
  if (keys.length === 0) throw new Error("DIGESTS must contain at least one service");

  const services = keys.map((service) => {
    requireService(manifest, service);
    const digest = digests[service]!;
    if (!DIGEST.test(digest)) throw new Error(`invalid digest for ${service}`);
    return { service, image: `${registry}/${service}@${digest}` };
  });

  // gridwork.sh has no database or migration Job. Keep both outputs because the shared
  // workflows and the three stateful sibling repos consume the same plan shape.
  return { services, migrationJob: "", migrationImage: "", sourceSha };
}

async function main(): Promise<void> {
  const environment = process.env["ENVIRONMENT"];
  const registry = process.env["GCP_REGISTRY"];
  if (!environment) throw new Error("ENVIRONMENT is required");
  if (!registry) throw new Error("GCP_REGISTRY is required");
  if (environment !== "staging" && environment !== "production") {
    throw new Error("ENVIRONMENT must be staging or production");
  }
  validateDeploymentConfig(environment, process.env);
  const manifest = await readManifest();
  const repository = process.env["GITHUB_REPOSITORY"]?.trim();
  if (!repository) throw new Error("GITHUB_REPOSITORY is required");
  if (manifest.repository !== repository) {
    throw new Error("deploy/services.json.repository does not match GITHUB_REPOSITORY");
  }

  let digests: Record<string, string>;
  let sourceSha: string;
  const receiptPath = process.env["PROMOTION_RECEIPT"]?.trim();
  if (receiptPath) {
    const stagingRunId = process.env["STAGING_RUN_ID"]?.trim();
    if (!stagingRunId) throw new Error("STAGING_RUN_ID is required");
    const promoted = promotionInputs(
      await readDeploymentReceipt(receiptPath),
      repository,
      stagingRunId,
    );
    digests = promoted.digests;
    sourceSha = promoted.sourceSha;
  } else {
    const digestInput = process.env["DIGESTS"];
    const sourceInput = process.env["SOURCE_SHA"]?.trim();
    if (!digestInput) throw new Error("DIGESTS is required");
    if (!sourceInput) throw new Error("SOURCE_SHA is required");
    digests = parseDigests(digestInput);
    sourceSha = sourceInput;
  }

  const plan = createPlan(
    manifest,
    digests,
    environment,
    registry,
    sourceSha,
  );
  output("services", JSON.stringify(plan.services));
  output("migration_job", plan.migrationJob);
  output("migration_image", plan.migrationImage);
  output("source_sha", plan.sourceSha);
  const holdSeconds = process.env["HOLD_SECONDS"]?.trim();
  if (holdSeconds !== undefined && holdSeconds !== "") {
    output("hold_seconds", String(validateHoldSeconds(holdSeconds)));
  }
}

if (import.meta.main) {
  main().catch(reportCliError);
}
