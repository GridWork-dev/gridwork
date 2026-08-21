import {
  output,
  readManifest,
  reportCliError,
  requireService,
  type ServiceManifest,
} from "./manifest";
import { readDeploymentReceipt, rollbackImage } from "./receipt";
import { validateDeploymentConfig } from "./policy";

const PROJECT = /^[a-z][a-z0-9-]{4,28}[a-z0-9]$/;
const REGION = /^[a-z]+-[a-z]+[0-9]$/;
const REVISION = /^[a-z][a-z0-9-]{0,61}[a-z0-9]$/;

export function validateRollback(
  manifest: ServiceManifest,
  service: string,
  revision: string,
  environment: string,
  nonproductionProject: string,
  productionProject: string,
  region: string,
): { project: string } {
  requireService(manifest, service);
  if (environment !== "staging" && environment !== "production") {
    throw new Error("ENVIRONMENT must be staging or production");
  }
  if (!PROJECT.test(nonproductionProject)) throw new Error("GCP_NONPROD_PROJECT is invalid");
  if (!PROJECT.test(productionProject)) throw new Error("GCP_PROD_PROJECT is invalid");
  if (!REGION.test(region)) throw new Error("GCP_REGION is invalid");
  if (
    revision.length > 63 ||
    !REVISION.test(revision) ||
    !revision.startsWith(`${service}-`)
  ) {
    throw new Error(`REVISION must belong to ${service}`);
  }

  return { project: environment === "production" ? productionProject : nonproductionProject };
}

async function main(): Promise<void> {
  const service = process.env["SERVICE"]?.trim();
  const revision = process.env["REVISION"]?.trim();
  const environment = process.env["ENVIRONMENT"]?.trim();
  const nonproductionProject = process.env["GCP_NONPROD_PROJECT"]?.trim();
  const productionProject = process.env["GCP_PROD_PROJECT"]?.trim();
  const region = process.env["GCP_REGION"]?.trim();
  const receiptPath = process.env["DEPLOYMENT_RECEIPT"]?.trim();
  const repository = process.env["GITHUB_REPOSITORY"]?.trim();
  const deploymentRunId = process.env["DEPLOYMENT_RUN_ID"]?.trim();
  if (!service) throw new Error("SERVICE is required");
  if (!revision) throw new Error("REVISION is required");
  if (!environment) throw new Error("ENVIRONMENT is required");
  if (!nonproductionProject) throw new Error("GCP_NONPROD_PROJECT is required");
  if (!productionProject) throw new Error("GCP_PROD_PROJECT is required");
  if (!region) throw new Error("GCP_REGION is required");
  if (!receiptPath) throw new Error("DEPLOYMENT_RECEIPT is required");
  if (!repository) throw new Error("GITHUB_REPOSITORY is required");
  if (!deploymentRunId) throw new Error("DEPLOYMENT_RUN_ID is required");
  if (environment !== "staging" && environment !== "production") {
    throw new Error("ENVIRONMENT must be staging or production");
  }
  validateDeploymentConfig(environment, process.env);

  const manifest = await readManifest();
  if (manifest.repository !== repository) {
    throw new Error("deploy/services.json.repository does not match GITHUB_REPOSITORY");
  }
  const result = validateRollback(
    manifest,
    service,
    revision,
    environment,
    nonproductionProject,
    productionProject,
    region,
  );
  output("project", result.project);
  output(
    "image",
    rollbackImage(
      await readDeploymentReceipt(receiptPath),
      repository,
      environment,
      service,
      revision,
      deploymentRunId,
    ),
  );
}

if (import.meta.main) {
  main().catch(reportCliError);
}
