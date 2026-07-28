# www — getparsec.ai

The public marketing site: landing page, blog (MDX), privacy policy. This is
the front door; the signed-in dashboard at `app.getparsec.ai` lives in
`packages/frontend`.

## Build

```sh
bun install
bun run build     # → out/  (pure static export)
bun run dev       # local dev server
```

`out/` is a plain directory of HTML/CSS/JS — serve it from any static host
(GCS + CDN, Cloudflare Pages, S3, nginx). There is no server runtime. To
preview the built export exactly as a dumb host would serve it:

```sh
python3 -m http.server 8000 -d out
```

## Content

- **Blog posts**: drop an `.mdx` file in `src/content/blog/` with `title`,
  `description`, `date` (and optional `tags`, `draft: true`) frontmatter.
  The index, sitemap, and post pages pick it up at build time.
- **Testimonials**: `src/content/testimonials.ts` — currently clearly-marked
  placeholders; replace with real quotes before launch.
- **Pricing**: tier cards carry no dollar figures (pricing is an open question
  per `DIRECTION.md` §10). Add numbers in `src/components/sections/pricing.tsx`
  when they exist.

## Conventions

- Brand tokens come from `brand/parsecbrandkit/tokens/tokens.json` via
  `src/app/globals.css`. Light mode = the same variables redefined under
  `[data-theme="light"]` — components never use `dark:` variants.
- `src/components/brand.tsx` is a deliberate copy of
  `packages/frontend/src/components/brand.tsx` (no JS workspace root to share
  through). If you touch one, mirror the other.
- The privacy policy (`src/app/privacy/page.tsx`) is an engineering-accurate
  description of data flows, not counsel-reviewed legal copy — have it
  reviewed before launch.
