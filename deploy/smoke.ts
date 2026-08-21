import { fetchWithTimeout, type Fetcher } from "./http";
import { reportCliError } from "./manifest";

const PUBLIC_ORIGINS = {
  production: "https://gridwork.sh",
  staging: "https://gridwork.gwstg.dev",
} as const;
const MODES = new Set(["pre-migration", "post-migration", "post-deploy", "rollback"]);
const CANARY_TAG = /^r[0-9]{1,20}$/;
const COMMIT = /^[0-9a-f]{40}$/;
const ORIGIN_SECRET = /^[A-Za-z0-9_-]{42}[AEIMQUYcgkosw048]$/;
const SERVICE = "gridwork-site";
const ORIGIN_SECRET_HEADER = "X-Gridwork-Origin-Secret";
const REQUEST_TIMEOUT_MS = 10_000;
const REQUIRED_HEADERS = [
  "strict-transport-security",
  "x-content-type-options",
  "x-frame-options",
  "referrer-policy",
  "content-security-policy",
] as const;

export type SmokeConfig = {
  environment: string;
  mode: string;
  canaryTag?: string | undefined;
  canaryUrls?: string | undefined;
  originSecrets?: string | undefined;
  expectedSha?: string | undefined;
  accessClientId?: string | undefined;
  accessClientSecret?: string | undefined;
};

type EnvironmentSource = Readonly<Record<string, string | undefined>>;

export function readSmokeConfig(environment: EnvironmentSource): SmokeConfig {
  const deploymentEnvironment = environment["ENVIRONMENT"]?.trim() ?? "";
  const configuredMode = environment["MODE"]?.trim();
  const mode = configuredMode || (deploymentEnvironment === "staging" ? "post-deploy" : "");
  return {
    environment: deploymentEnvironment,
    mode,
    canaryTag: environment["CANARY_TAG"]?.trim() || undefined,
    canaryUrls: environment["CANARY_URLS"]?.trim() || undefined,
    originSecrets: environment["ORIGIN_SECRETS"]?.trim() || undefined,
    expectedSha:
      mode === "post-deploy"
        ? environment["EXPECTED_SHA"]?.trim() || environment["GITHUB_SHA"]?.trim()
        : undefined,
    accessClientId: environment["CF_ACCESS_CLIENT_ID"],
    accessClientSecret: environment["CF_ACCESS_CLIENT_SECRET"],
  };
}

function validateCredential(value: string | undefined, label: string): string {
  const credential = value?.trim() ?? "";
  if (
    credential.length === 0 ||
    credential.length > 2_048 ||
    credential.includes("\r") ||
    credential.includes("\n")
  ) {
    throw new Error(`${label} is invalid`);
  }
  return credential;
}

function validateConfig(config: SmokeConfig): keyof typeof PUBLIC_ORIGINS {
  if (config.environment !== "staging" && config.environment !== "production") {
    throw new Error("ENVIRONMENT must be staging or production");
  }
  if (!MODES.has(config.mode)) throw new Error("MODE is invalid");
  if (config.mode === "pre-migration" || config.mode === "post-migration") {
    if (!config.canaryTag) throw new Error("CANARY_TAG is required");
    if (!CANARY_TAG.test(config.canaryTag)) throw new Error("CANARY_TAG is invalid");
  } else if (config.canaryUrls !== undefined || config.originSecrets !== undefined) {
    throw new Error(`${config.mode} smoke must not receive canary credentials`);
  }
  if (config.expectedSha !== undefined && !COMMIT.test(config.expectedSha)) {
    throw new Error("expected revision SHA is invalid");
  }
  if (config.environment === "staging") {
    if (!config.accessClientId || !config.accessClientSecret) {
      throw new Error("staging smoke requires Cloudflare Access service credentials");
    }
    validateCredential(config.accessClientId, "Cloudflare Access client ID");
    validateCredential(config.accessClientSecret, "Cloudflare Access client secret");
  }
  return config.environment;
}

function publicRequestHeaders(config: SmokeConfig): Headers {
  const headers = new Headers({ accept: "application/json" });
  if (config.environment === "staging") {
    headers.set(
      "CF-Access-Client-Id",
      validateCredential(config.accessClientId, "Cloudflare Access client ID"),
    );
    headers.set(
      "CF-Access-Client-Secret",
      validateCredential(config.accessClientSecret, "Cloudflare Access client secret"),
    );
  }
  return headers;
}

function parseStringMap(value: string | undefined, label: string): Record<string, string> {
  if (!value) throw new Error(`${label} is required`);
  let parsed: unknown;
  try {
    parsed = JSON.parse(value) as unknown;
  } catch {
    throw new Error(`${label} must be valid JSON`);
  }
  if (!isRecord(parsed)) throw new Error(`${label} must be a JSON object`);
  for (const key of Object.keys(parsed)) {
    if (key !== SERVICE) throw new Error(`${label} contains unknown service ${key}`);
  }
  if (!Object.hasOwn(parsed, SERVICE)) throw new Error(`${label} is missing ${SERVICE}`);
  const entry = parsed[SERVICE];
  if (typeof entry !== "string" || entry.length === 0) {
    throw new Error(`${label}.${SERVICE} is invalid`);
  }
  return { [SERVICE]: entry };
}

function canaryTarget(config: SmokeConfig): { origin: string; headers: Headers } {
  const urls = parseStringMap(config.canaryUrls, "CANARY_URLS");
  const secrets = parseStringMap(config.originSecrets, "ORIGIN_SECRETS");
  const rawUrl = urls[SERVICE]!;
  let url: URL;
  try {
    url = new URL(rawUrl);
  } catch {
    throw new Error(`CANARY_URLS.${SERVICE} must be a Cloud Run tag URL`);
  }
  if (
    url.protocol !== "https:" ||
    !url.hostname.endsWith(".run.app") ||
    url.username !== "" ||
    url.password !== "" ||
    url.port !== "" ||
    (url.pathname !== "/" && url.pathname !== "") ||
    url.search !== "" ||
    url.hash !== ""
  ) {
    throw new Error(`CANARY_URLS.${SERVICE} must be a Cloud Run tag URL`);
  }
  const secret = secrets[SERVICE]!;
  if (!ORIGIN_SECRET.test(secret)) throw new Error(`ORIGIN_SECRETS.${SERVICE} is invalid`);
  return {
    origin: url.origin,
    headers: new Headers({ accept: "application/json", [ORIGIN_SECRET_HEADER]: secret }),
  };
}

function assertResponseHeaders(response: Response, path: string): void {
  for (const header of REQUIRED_HEADERS) {
    if (!response.headers.has(header)) throw new Error(`${path} is missing ${header}`);
  }
  if (response.headers.has("x-powered-by")) throw new Error(`${path} exposes x-powered-by`);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export async function runSmoke(config: SmokeConfig, fetcher: Fetcher = fetch): Promise<void> {
  const environment = validateConfig(config);
  const canaryMode = config.mode === "pre-migration" || config.mode === "post-migration";
  const target = canaryMode
    ? canaryTarget(config)
    : { origin: PUBLIC_ORIGINS[environment], headers: publicRequestHeaders(config) };
  const label = canaryMode ? "canary" : "public";

  // Pre/post-migration proves the zero-traffic revision at the tag URL resolved by gcloud,
  // authenticating with the same origin-secret header the Worker uses. Post-deploy and
  // rollback prove the user-visible Cloudflare path after traffic has moved.
  const health = await fetchWithTimeout(
    `${target.origin}/health`,
    { method: "GET", redirect: "manual", headers: target.headers },
    REQUEST_TIMEOUT_MS,
    fetcher,
  );
  if (health.status !== 200) throw new Error(`${label} health returned ${health.status}`);
  assertResponseHeaders(health, `${label} health`);
  if (!(health.headers.get("cache-control") ?? "").toLowerCase().includes("no-store")) {
    throw new Error(`${label} health is cacheable`);
  }

  let body: unknown;
  try {
    body = (await health.json()) as unknown;
  } catch {
    throw new Error(`${label} health returned invalid JSON`);
  }
  if (!isRecord(body) || body["status"] !== "ok" || typeof body["sha"] !== "string") {
    throw new Error(`${label} health returned an invalid payload`);
  }
  if (!COMMIT.test(body["sha"])) {
    throw new Error(`${label} health did not report an immutable revision`);
  }
  if (config.expectedSha !== undefined && body["sha"] !== config.expectedSha) {
    throw new Error(`${label} health reported an unexpected revision`);
  }

  const page = await fetchWithTimeout(
    `${target.origin}/`,
    { method: "GET", redirect: "manual", headers: target.headers },
    REQUEST_TIMEOUT_MS,
    fetcher,
  );
  if (page.status !== 200) throw new Error(`${label} page returned ${page.status}`);
  assertResponseHeaders(page, `${label} page`);
}

async function main(): Promise<void> {
  const config = readSmokeConfig(process.env);
  if (!config.environment) throw new Error("ENVIRONMENT is required");
  if (!config.mode) throw new Error("MODE is required");
  await runSmoke(config);
  process.stdout.write(`smoke environment=${config.environment} mode=${config.mode} passed\n`);
}

if (import.meta.main) {
  main().catch(reportCliError);
}
