const COMMIT = /^[0-9a-f]{40}$/;
const RUN_ID = /^[1-9][0-9]{0,19}$/;
const SERVICE = /^[a-z][a-z0-9-]{0,62}$/;
const REVISION = /^[a-z][a-z0-9-]{0,61}[a-z0-9]$/;
const IMAGE = /^([a-z0-9][a-z0-9.-]*\.pkg\.dev\/[a-z][a-z0-9-]{4,28}[a-z0-9]\/[a-z][a-z0-9._-]{0,62})\/([a-z][a-z0-9-]{0,62})@(sha256:[0-9a-f]{64})$/;
const REPOSITORY = /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/;

type ReceiptEnvironment = "staging" | "production";

export type ReceiptService = {
  service: string;
  image: string;
  revision: string;
};

export type DeploymentReceipt = {
  schema: 1;
  repository: string;
  environment: ReceiptEnvironment;
  source_sha: string;
  deployment_run_id: string;
  services: ReceiptService[];
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function exactKeys(
  value: Record<string, unknown>,
  required: readonly string[],
  label: string,
): void {
  const allowed = new Set(required);
  for (const key of Object.keys(value)) {
    if (!allowed.has(key)) throw new Error(`${label} contains unknown field ${key}`);
  }
  for (const key of required) {
    if (!Object.hasOwn(value, key)) throw new Error(`${label} is missing ${key}`);
  }
}

function parseService(value: unknown, index: number): ReceiptService {
  if (!isRecord(value)) throw new Error(`deployment receipt service ${index} must be an object`);
  exactKeys(value, ["service", "image", "revision"], `deployment receipt service ${index}`);
  const service = value["service"];
  const image = value["image"];
  const revision = value["revision"];
  if (typeof service !== "string" || !SERVICE.test(service)) {
    throw new Error(`deployment receipt service ${index} has invalid service`);
  }
  if (typeof image !== "string") {
    throw new Error(`deployment receipt service ${index} has invalid image`);
  }
  const imageMatch = IMAGE.exec(image);
  if (!imageMatch || imageMatch[2] !== service) {
    throw new Error(`deployment receipt service ${index} has invalid image`);
  }
  if (
    typeof revision !== "string" ||
    !REVISION.test(revision) ||
    !revision.startsWith(`${service}-`)
  ) {
    throw new Error(`deployment receipt service ${index} has invalid revision`);
  }
  return { service, image, revision };
}

export function parseDeploymentReceipt(value: unknown): DeploymentReceipt {
  if (!isRecord(value)) throw new Error("deployment receipt must contain an object");
  exactKeys(
    value,
    ["schema", "repository", "environment", "source_sha", "deployment_run_id", "services"],
    "deployment receipt",
  );
  if (value["schema"] !== 1) throw new Error("deployment receipt schema is invalid");
  const repository = value["repository"];
  const environment = value["environment"];
  const sourceSha = value["source_sha"];
  const runId = value["deployment_run_id"];
  if (typeof repository !== "string" || !REPOSITORY.test(repository)) {
    throw new Error("deployment receipt repository is invalid");
  }
  if (environment !== "staging" && environment !== "production") {
    throw new Error("deployment receipt environment is invalid");
  }
  if (typeof sourceSha !== "string" || !COMMIT.test(sourceSha)) {
    throw new Error("deployment receipt source SHA is invalid");
  }
  if (typeof runId !== "string" || !RUN_ID.test(runId)) {
    throw new Error("deployment receipt run ID is invalid");
  }
  const serviceValues = value["services"];
  if (!Array.isArray(serviceValues) || serviceValues.length === 0) {
    throw new Error("deployment receipt services must be non-empty");
  }
  const services = serviceValues.map(parseService);
  if (new Set(services.map((service) => service.service)).size !== services.length) {
    throw new Error("deployment receipt contains duplicate services");
  }
  return {
    schema: 1,
    repository,
    environment,
    source_sha: sourceSha,
    deployment_run_id: runId,
    services,
  };
}

export async function readDeploymentReceipt(path: string): Promise<DeploymentReceipt> {
  let value: unknown;
  try {
    value = JSON.parse(await Bun.file(path).text()) as unknown;
  } catch {
    throw new Error("deployment receipt is not valid JSON");
  }
  return parseDeploymentReceipt(value);
}

function assertRepository(receipt: DeploymentReceipt, expectedRepository: string): void {
  if (receipt.repository !== expectedRepository) {
    throw new Error("deployment receipt repository does not match this workflow");
  }
}

export function promotionInputs(
  receipt: DeploymentReceipt,
  expectedRepository: string,
  stagingRunId: string,
): { sourceSha: string; digests: Record<string, string> } {
  assertRepository(receipt, expectedRepository);
  if (receipt.environment !== "staging") throw new Error("production requires a staging receipt");
  if (!RUN_ID.test(stagingRunId) || receipt.deployment_run_id !== stagingRunId) {
    throw new Error("staging receipt does not match STAGING_RUN_ID");
  }
  const digests: Record<string, string> = {};
  for (const service of receipt.services) {
    const match = IMAGE.exec(service.image);
    if (!match) throw new Error(`deployment receipt has invalid image for ${service.service}`);
    digests[service.service] = match[3]!;
  }
  return { sourceSha: receipt.source_sha, digests };
}

export function rollbackImage(
  receipt: DeploymentReceipt,
  expectedRepository: string,
  environment: string,
  service: string,
  revision: string,
  deploymentRunId = receipt.deployment_run_id,
): string {
  assertRepository(receipt, expectedRepository);
  if (!RUN_ID.test(deploymentRunId) || receipt.deployment_run_id !== deploymentRunId) {
    throw new Error("deployment receipt does not match DEPLOYMENT_RUN_ID");
  }
  if (receipt.environment !== environment) {
    throw new Error("deployment receipt environment does not match rollback target");
  }
  const target = receipt.services.find(
    (entry) => entry.service === service && entry.revision === revision,
  );
  if (!target) throw new Error("revision is not present in the approved deployment receipt");
  return target.image;
}
