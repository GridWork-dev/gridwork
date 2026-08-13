// The canonical public origin, in one place. It was spelled out separately in
// metadataBase and the openGraph URL, which is how a domain move turns into a
// scavenger hunt; sitemap, robots, canonicals, and the JSON-LD graph all read
// it from here now.
//
// A literal rather than an env var on purpose: this origin is a published fact
// about the project, baked into the crate manifests and the README, not a
// per-deploy knob. gridwork.dev 301s here at the edge.
export const SITE_ORIGIN = "https://gridwork.sh";

export function absolute(path: string): string {
  return new URL(path, SITE_ORIGIN).toString();
}
