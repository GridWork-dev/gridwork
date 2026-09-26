import type { MetadataRoute } from "next";

import { absolute } from "@/lib/site-url";

// Rendered once at build: the site is a static export.
export const dynamic = "force-static";

export default function robots(): MetadataRoute.Robots {
  return {
    rules: [{ userAgent: "*", allow: "/" }],
    sitemap: absolute("/sitemap.xml"),
  };
}
