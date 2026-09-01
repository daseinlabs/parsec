import type { MetadataRoute } from "next";
import { SITE } from "@/lib/site";
import { allPosts } from "@/lib/posts";
import { TOOL_SLUGS } from "@/content/tools";

// Required by `output: "export"` — see robots.ts.
export const dynamic = "force-static";

// Rendered once at build (static export) — no request-time inputs. Post
// entries carry lastModified from frontmatter; the static pages carry none
// rather than a fabricated wall-clock date.
export default function sitemap(): MetadataRoute.Sitemap {
  return [
    { url: `${SITE.url}/` },
    ...TOOL_SLUGS.map((slug) => ({ url: `${SITE.url}/${slug}/` })),
    { url: `${SITE.url}/privacy/` },
    { url: `${SITE.url}/blog/` },
    ...allPosts().map((post) => ({
      url: `${SITE.url}/blog/${post.slug}/`,
      lastModified: post.date,
    })),
  ];
}
