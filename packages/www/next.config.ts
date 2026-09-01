import type { NextConfig } from "next";
import createMDX from "@next/mdx";

// The marketing site is a pure static export: `next build` → `out/`, a plain
// directory of HTML/CSS/JS servable from any CDN or object store. No server
// runtime, so every page is fully-formed HTML in the initial response —
// crawlable by search and LLM bots without executing JS. Anything that needs a
// request at runtime (redirects, headers, cookies, server actions) is
// unavailable by design; see the static-exports guide in
// node_modules/next/dist/docs.
const nextConfig: NextConfig = {
  output: "export",
  // Emit /blog/ → /blog/index.html so nested routes are real files on hosts
  // with no rewrite rules (GCS, S3, nginx-with-defaults).
  trailingSlash: true,
  pageExtensions: ["ts", "tsx", "md", "mdx"],
  allowedDevOrigins: ["tagassistant.google.com"],
};

// Turbopack builds this package, and it cannot pass JS functions to Rust —
// remark/rehype plugins MUST be named by string with serializable options
// only (node_modules/next/dist/docs/01-app/02-guides/mdx.md).
const withMDX = createMDX({
  options: {
    remarkPlugins: [
      // GitHub-flavored markdown — tables, autolinks, strikethrough.
      "remark-gfm",
      // Parse YAML frontmatter out of the document…
      "remark-frontmatter",
      // …and expose it as an exported `frontmatter` object so pages can
      // import it. lib/posts.ts reads the same YAML with gray-matter at
      // build time for the index/sitemap — one source, two readers.
      "remark-mdx-frontmatter",
    ],
    rehypePlugins: [
      [
        "rehype-pretty-code",
        {
          // Dual theme: emits both palettes as CSS variables keyed off
          // data-theme, matching the site-wide theming (globals.css).
          themes: { light: "github-light", dark: "github-dark" },
          // Our own surface tokens supply the background.
          keepBackground: false,
        },
      ],
    ],
  },
});

export default withMDX(nextConfig);
