import type { MetadataRoute } from "next";

import { absolute } from "@/lib/site-url";

export default function robots(): MetadataRoute.Robots {
  return {
    rules: [{ userAgent: "*", allow: "/" }],
    sitemap: absolute("/sitemap.xml"),
  };
}
