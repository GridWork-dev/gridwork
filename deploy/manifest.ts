const SERVICE_KEY = /^[a-z][a-z0-9-]{0,62}$/;
const REPOSITORY = /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/;
const HOSTNAME = /^(?=.{1,253}$)(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;
const ENV_NAME = /^[A-Z][A-Z0-9_]{0,127}$/;

export type ServiceConfig = {
  dockerfile: string;
  build_context: string;
  watched_paths: string[];
  healthcheck: string;
  hostnames: string[];
  runtime: "cloud-run-service";
  origin_secret_mode?: string;
  origin_secret_current?: string;
  origin_secret_next?: string;
};

export type ServiceManifest = {
  _source: string;
  repository: string;
  services: Record<string, ServiceConfig>;
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function assertExactKeys(
  value: Record<string, unknown>,
  required: readonly string[],
  optional: readonly string[],
  label: string,
): void {
  const allowed = new Set([...required, ...optional]);
  for (const key of Object.keys(value)) {
    if (!allowed.has(key)) throw new Error(`${label} contains unknown field ${key}`);
  }
  for (const key of required) {
    if (!(key in value)) throw new Error(`${label} is missing ${key}`);
  }
}

function boundedString(value: unknown, label: string, maximum = 512): string {
  if (typeof value !== "string") throw new Error(`${label} must be a string`);
  const trimmed = value.trim();
  if (
    trimmed.length === 0 ||
    trimmed.length > maximum ||
    trimmed.includes("\0") ||
    value.includes("\r") ||
    value.includes("\n")
  ) {
    throw new Error(`${label} is invalid`);
  }
  return trimmed;
}

function relativePath(value: unknown, label: string, allowGlob = false): string {
  const path = boundedString(value, label);
  if (path.startsWith("/") || path.split("/").includes("..")) {
    throw new Error(`${label} must stay inside the repository`);
  }
  if (!allowGlob && ["*", "?", "[", "]", "{", "}"].some((token) => path.includes(token))) {
    throw new Error(`${label} must not be a glob`);
  }
  return path;
}

function stringArray(value: unknown, label: string): string[] {
  if (!Array.isArray(value) || value.length === 0) throw new Error(`${label} must be non-empty`);
  return value.map((entry, index) => boundedString(entry, `${label}[${index}]`));
}

function optionalEnvironmentName(value: unknown, label: string): string | undefined {
  if (value === undefined) return undefined;
  const name = boundedString(value, label, 128);
  if (!ENV_NAME.test(name)) throw new Error(`${label} must be an environment variable name`);
  return name;
}

function parseService(value: unknown, key: string): ServiceConfig {
  if (!isRecord(value)) throw new Error(`service ${key} must be an object`);
  assertExactKeys(
    value,
    ["dockerfile", "build_context", "watched_paths", "healthcheck", "hostnames", "runtime"],
    ["origin_secret_mode", "origin_secret_current", "origin_secret_next"],
    `service ${key}`,
  );

  const healthcheck = boundedString(value["healthcheck"], `service ${key}.healthcheck`, 256);
  if (!healthcheck.startsWith("/") || healthcheck.startsWith("//")) {
    throw new Error(`service ${key}.healthcheck must be an absolute URL path`);
  }

  const hostnames = stringArray(value["hostnames"], `service ${key}.hostnames`);
  for (const hostname of hostnames) {
    if (!HOSTNAME.test(hostname)) throw new Error(`service ${key} has invalid hostname ${hostname}`);
  }

  if (value["runtime"] !== "cloud-run-service") {
    throw new Error(`service ${key}.runtime must be cloud-run-service`);
  }

  return {
    dockerfile: relativePath(value["dockerfile"], `service ${key}.dockerfile`),
    build_context: relativePath(value["build_context"], `service ${key}.build_context`),
    watched_paths: stringArray(value["watched_paths"], `service ${key}.watched_paths`).map(
      (path, index) => relativePath(path, `service ${key}.watched_paths[${index}]`, true),
    ),
    healthcheck,
    hostnames,
    runtime: "cloud-run-service",
    ...optionalField(
      "origin_secret_mode",
      optionalEnvironmentName(value["origin_secret_mode"], `service ${key}.origin_secret_mode`),
    ),
    ...optionalField(
      "origin_secret_current",
      optionalEnvironmentName(value["origin_secret_current"], `service ${key}.origin_secret_current`),
    ),
    ...optionalField(
      "origin_secret_next",
      optionalEnvironmentName(value["origin_secret_next"], `service ${key}.origin_secret_next`),
    ),
  };
}

function optionalField<Key extends string>(key: Key, value: string | undefined): Partial<Record<Key, string>> {
  return value === undefined ? {} : { [key]: value } as Record<Key, string>;
}

export function parseManifest(value: unknown): ServiceManifest {
  if (!isRecord(value)) throw new Error("deploy/services.json must contain an object");
  assertExactKeys(value, ["_source", "repository", "services"], [], "deploy/services.json");

  const source = boundedString(value["_source"], "deploy/services.json._source");
  const repository = boundedString(value["repository"], "deploy/services.json.repository", 256);
  if (!REPOSITORY.test(repository)) throw new Error("deploy/services.json.repository is invalid");
  if (!isRecord(value["services"]) || Object.keys(value["services"]).length === 0) {
    throw new Error("deploy/services.json.services must be a non-empty object");
  }

  const services: Record<string, ServiceConfig> = {};
  for (const [key, service] of Object.entries(value["services"])) {
    if (!SERVICE_KEY.test(key)) throw new Error(`invalid service key ${key}`);
    services[key] = parseService(service, key);
  }
  return { _source: source, repository, services };
}

export async function readManifest(): Promise<ServiceManifest> {
  const text = await Bun.file(new URL("./services.json", import.meta.url)).text();
  let value: unknown;
  try {
    value = JSON.parse(text) as unknown;
  } catch {
    throw new Error("deploy/services.json is not valid JSON");
  }
  return parseManifest(value);
}

export function assertManifestRepository(
  manifest: ServiceManifest,
  expectedRepository: string | undefined,
): void {
  if (!expectedRepository) throw new Error("GITHUB_REPOSITORY is required");
  if (!REPOSITORY.test(expectedRepository)) throw new Error("GITHUB_REPOSITORY is invalid");
  if (manifest.repository !== expectedRepository) {
    throw new Error("deploy/services.json.repository does not match GITHUB_REPOSITORY");
  }
}

export async function readManifestForRepository(
  expectedRepository: string | undefined,
): Promise<ServiceManifest> {
  const manifest = await readManifest();
  assertManifestRepository(manifest, expectedRepository);
  return manifest;
}

export function requireService(manifest: ServiceManifest, key: string): ServiceConfig {
  if (!Object.hasOwn(manifest.services, key)) throw new Error(`unknown service ${key}`);
  const service = manifest.services[key];
  if (!service) throw new Error(`unknown service ${key}`);
  return service;
}

export function output(name: string, value: string): void {
  if (value.includes("\r") || value.includes("\n")) {
    throw new Error(`GitHub output ${name} must be a single line`);
  }
  process.stdout.write(`${name}=${value}\n`);
}

export function reportCliError(error: unknown): void {
  const message = error instanceof Error ? error.message : "unknown failure";
  process.stderr.write(`error: ${message}\n`);
  process.exitCode = 1;
}
