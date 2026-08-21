import {
  output,
  readManifest,
  reportCliError,
  requireService,
  type ServiceManifest,
} from "./manifest";

export function serviceInputs(
  manifest: ServiceManifest,
  serviceKey: string,
): { dockerfile: string; buildContext: string } {
  const service = requireService(manifest, serviceKey);
  return { dockerfile: service.dockerfile, buildContext: service.build_context };
}

async function main(): Promise<void> {
  const key = process.env["SERVICE"]?.trim();
  if (!key) throw new Error("SERVICE is required");
  const inputs = serviceInputs(await readManifest(), key);
  output("dockerfile", inputs.dockerfile);
  output("build_context", inputs.buildContext);
}

if (import.meta.main) {
  main().catch(reportCliError);
}
