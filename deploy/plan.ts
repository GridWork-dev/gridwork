import {
  assertManifestRepository,
  output,
  readManifestForRepository,
  reportCliError,
  requireService,
  type ServiceManifest,
} from "./manifest";

const DIGEST = /^sha256:[0-9a-f]{64}$/;
const ARTIFACT_REGISTRY = /^[a-z0-9][a-z0-9.-]*\.pkg\.dev\/[a-z][a-z0-9-]{4,28}[a-z0-9]\/[a-z][a-z0-9._-]{0,62}$/;

type Environment = "staging" | "production";

export type DeploymentPlan = {
  services: Array<{ service: string; image: string }>;
  migrationJob: string;
  migrationImage: string;
};

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
  expectedRepository: string,
): DeploymentPlan {
  parseEnvironment(environmentValue);
  assertManifestRepository(manifest, expectedRepository);
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
  return { services, migrationJob: "", migrationImage: "" };
}

async function main(): Promise<void> {
  const environment = process.env["ENVIRONMENT"];
  const registry = process.env["GCP_REGISTRY"];
  if (!environment) throw new Error("ENVIRONMENT is required");
  if (!registry) throw new Error("GCP_REGISTRY is required");
  if (environment !== "staging" && environment !== "production") {
    throw new Error("ENVIRONMENT must be staging or production");
  }
  const repository = process.env["GITHUB_REPOSITORY"];
  const manifest = await readManifestForRepository(repository);
  const digestInput = process.env["DIGESTS"];
  if (!digestInput) throw new Error("DIGESTS is required");
  const plan = createPlan(manifest, parseDigests(digestInput), environment, registry, repository!);
  output("services", JSON.stringify(plan.services));
  output("migration_job", plan.migrationJob);
  output("migration_image", plan.migrationImage);
}

if (import.meta.main) {
  main().catch(reportCliError);
}
