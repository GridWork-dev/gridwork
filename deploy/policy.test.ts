import { describe, expect, test } from "bun:test";

import { deploymentPolicy, validateDeploymentConfig } from "./policy";

const staging = {
  GITHUB_REPOSITORY: deploymentPolicy.repository,
  BUN_VERSION_FILE: deploymentPolicy.bun_version_file,
  GCP_WIF_PROVIDER: deploymentPolicy.wif_provider,
  GCP_REGISTRY: deploymentPolicy.registry,
  GCP_REGISTRY_HOST: deploymentPolicy.registry_host,
  GCP_REGION: deploymentPolicy.region,
  GCP_NONPROD_PROJECT: deploymentPolicy.projects.staging,
  GCP_PROD_PROJECT: deploymentPolicy.projects.production,
  GCP_STAGING_DEPLOYER_SA: deploymentPolicy.service_accounts.staging,
};

describe("committed deployment policy", () => {
  test("accepts the exact staging variable set", () => {
    expect(() => validateDeploymentConfig("staging", staging)).not.toThrow();
  });

  test("rejects mutable repository variables that drift from policy", () => {
    expect(() =>
      validateDeploymentConfig("staging", { ...staging, GCP_REGION: "us-west1" }),
    ).toThrow("GCP_REGION does not match deploy/policy.ts");
    expect(() =>
      validateDeploymentConfig("staging", { ...staging, GCP_WIF_PROVIDER: undefined }),
    ).toThrow("GCP_WIF_PROVIDER does not match deploy/policy.ts");
  });
});
