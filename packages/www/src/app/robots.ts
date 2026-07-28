import type { MetadataRoute } from "next";
import { SITE } from "@/lib/site";

// Required by `output: "export"` — metadata routes compile to route handlers,
// and static export refuses any handler not explicitly marked static.
export const dynamic = "force-static";

export default function robots(): MetadataRoute.Robots {
  return {
    rules: {
      userAgent: "*",
      allow: "/",
    },
    sitemap: `${SITE.url}/sitemap.xml`,
  };
}
