import {
  output,
  readManifest,
  reportCliError,
  requireService,
  type ServiceManifest,
} from "./manifest";

const PROJECT = /^[a-z][a-z0-9-]{4,28}[a-z0-9]$/;
const REGION = /^[a-z]+-[a-z]+[0-9]$/;
const REVISION = /^[a-z][a-z0-9-]{0,61}[a-z0-9]$/;

type GcloudRunner = (arguments_: string[]) => string;

function runGcloud(arguments_: string[]): string {
  const result = Bun.spawnSync(["gcloud", ...arguments_]);
  if (result.exitCode !== 0) throw new Error("gcloud rejected the revision");
  return new TextDecoder().decode(result.stdout).trim();
}

export function validateRollback(
  manifest: ServiceManifest,
  service: string,
  revision: string,
  environment: string,
  project: string,
  region: string,
  gcloud: GcloudRunner = runGcloud,
): { project: string } {
  requireService(manifest, service);
  if (environment !== "staging" && environment !== "production") {
    throw new Error("ENVIRONMENT must be staging or production");
  }
  if (!PROJECT.test(project)) throw new Error("GCP_PROJECT is invalid");
  if (!REGION.test(region)) throw new Error("GCP_REGION is invalid");
  if (
    revision.length > 63 ||
    !REVISION.test(revision) ||
    !revision.startsWith(`${service}-`)
  ) {
    throw new Error(`REVISION must belong to ${service}`);
  }

  const arguments_ = [
    "run",
    "revisions",
    "describe",
    revision,
    "--project",
    project,
    "--region",
    region,
    "--format=value(metadata.name)",
  ];
  let described: string;
  try {
    described = gcloud(arguments_);
  } catch {
    throw new Error(`revision ${revision} does not exist`);
  }
  if (described !== revision) throw new Error(`revision ${revision} does not exist`);
  return { project };
}

async function main(): Promise<void> {
  const service = process.env["SERVICE"]?.trim();
  const revision = process.env["REVISION"]?.trim();
  const environment = process.env["ENVIRONMENT"]?.trim();
  const project = process.env["GCP_PROJECT"]?.trim();
  const region = process.env["GCP_REGION"]?.trim();
  if (!service) throw new Error("SERVICE is required");
  if (!revision) throw new Error("REVISION is required");
  if (!environment) throw new Error("ENVIRONMENT is required");
  if (!project) throw new Error("GCP_PROJECT is required");
  if (!region) throw new Error("GCP_REGION is required");

  const result = validateRollback(
    await readManifest(),
    service,
    revision,
    environment,
    project,
    region,
  );
  output("project", result.project);
}

if (import.meta.main) {
  main().catch(reportCliError);
}
