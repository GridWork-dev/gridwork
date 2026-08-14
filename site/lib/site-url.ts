// The canonical public origin, in one place. It was spelled out separately in
// metadataBase and the openGraph URL, which is how a domain move turns into a
// scavenger hunt; sitemap, robots, canonicals, and the JSON-LD graph all read
// it from here now.
//
// A literal rather than an env var on purpose: this origin is a published fact
// about the project, baked into the crate manifests and the README, not a
// per-deploy knob.
//
// This comment used to end "gridwork.dev 301s here at the edge." It does not.
// Measured 2026-08-14: gridwork.dev answers 200 with a different application
// entirely — no redirect was ever configured. Nothing in this file depended on
// the claim, which is precisely why it survived: an unused justification is
// never tested by the code it justifies.
export const SITE_ORIGIN = "https://gridwork.sh";

export function absolute(path: string): string {
  return new URL(path, SITE_ORIGIN).toString();
}
