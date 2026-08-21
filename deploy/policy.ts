export const deploymentPolicy = {
  repository: "GridWork-dev/gridwork",
  bun_version_file: "site/package.json",
  wif_provider:
    "projects/495210962539/locations/global/workloadIdentityPools/github-deploy/providers/github",
  registry: "us-east4-docker.pkg.dev/gridwork-shared-99d2/apps",
  registry_host: "us-east4-docker.pkg.dev",
  region: "us-east4",
  projects: {
    staging: "gridwork-nonprod-99d2",
    production: "gridwork-prod-99d2",
  },
  service_accounts: {
    publish: "image-publisher@gridwork-shared-99d2.iam.gserviceaccount.com",
    staging: "staging-deployer-gridwork@gridwork-shared-99d2.iam.gserviceaccount.com",
    production: "prod-deployer-gridwork@gridwork-shared-99d2.iam.gserviceaccount.com",
  },
} as const;

export type DeploymentTarget = keyof typeof deploymentPolicy.service_accounts;
type EnvironmentSource = Readonly<Record<string, string | undefined>>;

function requireExact(environment: EnvironmentSource, name: string, expected: string): void {
  if (environment[name] !== expected) throw new Error(`${name} does not match deploy/policy.ts`);
}

export function validateDeploymentConfig(
  target: DeploymentTarget,
  environment: EnvironmentSource,
): void {
  requireExact(environment, "GITHUB_REPOSITORY", deploymentPolicy.repository);
  requireExact(environment, "BUN_VERSION_FILE", deploymentPolicy.bun_version_file);
  requireExact(environment, "GCP_WIF_PROVIDER", deploymentPolicy.wif_provider);
  requireExact(environment, "GCP_REGISTRY", deploymentPolicy.registry);
  requireExact(environment, "GCP_REGISTRY_HOST", deploymentPolicy.registry_host);
  requireExact(environment, "GCP_REGION", deploymentPolicy.region);
  requireExact(environment, "GCP_NONPROD_PROJECT", deploymentPolicy.projects.staging);
  requireExact(environment, "GCP_PROD_PROJECT", deploymentPolicy.projects.production);

  const serviceAccountVariable = {
    publish: "GCP_IMAGE_PUBLISHER_SA",
    staging: "GCP_STAGING_DEPLOYER_SA",
    production: "GCP_PROD_DEPLOYER_SA",
  } as const;
  requireExact(
    environment,
    serviceAccountVariable[target],
    deploymentPolicy.service_accounts[target],
  );
}
