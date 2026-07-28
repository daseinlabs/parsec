import fs from "node:fs";
import path from "node:path";
import matter from "gray-matter";

// Build-time-only reader of the blog content directory. The static export
// runs every page at build, so fs here never ships to (or runs in) a browser.
// The same YAML frontmatter is ALSO exported from each .mdx file by
// remark-mdx-frontmatter (next.config.ts) — that copy renders the post page;
// this one feeds the index, sitemap, and per-post metadata without importing
// every post's full component tree.

export type PostMeta = {
  slug: string;
  title: string;
  description: string;
  /** ISO date, e.g. "2026-07-27" */
  date: string;
  /** Optional byline, e.g. "Nicholas Swaminathan" */
  author?: string;
  /** Optional link for the byline, e.g. a LinkedIn profile */
  authorUrl?: string;
  tags: string[];
  draft: boolean;
};

const BLOG_DIR = path.join(process.cwd(), "src/content/blog");

export function allPosts(): PostMeta[] {
  if (!fs.existsSync(BLOG_DIR)) return [];
  return fs
    .readdirSync(BLOG_DIR)
    .filter((f) => f.endsWith(".mdx"))
    .map((f) => {
      const slug = f.replace(/\.mdx$/, "");
      const { data } = matter(fs.readFileSync(path.join(BLOG_DIR, f), "utf8"));
      // Fail the build loudly on a malformed post rather than emitting an
      // undefined-titled page a crawler would happily index.
      for (const key of ["title", "description", "date"]) {
        if (typeof data[key] !== "string" || data[key].length === 0) {
          throw new Error(`blog post ${f}: missing frontmatter field "${key}"`);
        }
      }
      return {
        slug,
        title: data.title as string,
        description: data.description as string,
        date: data.date as string,
        author: typeof data.author === "string" ? data.author : undefined,
        authorUrl:
          typeof data.authorUrl === "string" ? data.authorUrl : undefined,
        tags: (data.tags as string[]) ?? [],
        draft: data.draft === true,
      };
    })
    .filter((p) => !p.draft)
    .sort((a, b) => b.date.localeCompare(a.date));
}

export function postBySlug(slug: string): PostMeta | undefined {
  return allPosts().find((p) => p.slug === slug);
}
