import type { MetadataRoute } from "next";

import { absolute } from "@/lib/site-url";
import { source } from "@/lib/source";

// Enumerated from the docs source rather than hand-listed: a curated page added
// to content/docs already has to pass the freshness gate, and a sitemap that
// needs a second manual edit is a sitemap that silently goes stale.
// Rendered once at build: the site is a static export.
export const dynamic = "force-static";

export default function sitemap(): MetadataRoute.Sitemap {
  const docs = source.getPages().map((page) => ({
    url: absolute(page.url),
    changeFrequency: "weekly" as const,
    priority: 0.8,
  }));

  return [
    { url: absolute("/"), changeFrequency: "weekly", priority: 1 },
    ...docs,
    { url: absolute("/privacy"), changeFrequency: "yearly", priority: 0.1 },
  ];
}
