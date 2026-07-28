<!-- BEGIN:nextjs-agent-rules -->
# This is NOT the Next.js you know

This version has breaking changes — APIs, conventions, and file structure may all differ from your training data. Read the relevant guide in `node_modules/next/dist/docs/` before writing any code. Heed deprecation notices.
<!-- END:nextjs-agent-rules -->

# www — getparsec.ai marketing site

Pure static export (`output: "export"` → `out/`). No SSR, no route handlers
that read requests, no server actions, no redirects/headers/cookies — if a
change needs any of those, it does not belong in this package.

Theming: dark is the brand default; light mode works by redefining the same
`--color-*` variables under `[data-theme="light"]` in `src/app/globals.css`.
Never use `dark:` variants or raw hex in components — use the token utilities
(`bg-void`, `text-ink`, `text-phosphor`, …) and both themes come for free.
Glow only via the `.glow` / `.glow-mark` classes (they self-disable on light).
The mark/logo uses `text-logo`, never `text-phosphor`: on light, interactive
green darkens to emerald (#17833A) while the logo keeps the bright brand green
(#2FA317) — BRANDING.md §2 "logo green".

Color roles (use these pairings, don't invent new ones):

| Role | Tokens |
|---|---|
| body copy | `text-muted`; inline emphasis `text-ink` |
| kicker / table header / chip label | `text-xs tracking-caps text-faint uppercase` |
| fine print & illustrative captions | `text-xs text-faint` |
| attribution lines (testimonials) | `text-xs text-muted` |
| accent number / glyph | `text-phosphor` (aria-hidden if decorative); the hero owns the page's only `.glow` |
| prose links | `text-info underline underline-offset-2 hover:text-phosphor` |
| chrome links (nav/footer) | `text-muted hover:text-phosphor` |
| green action button | `border-line-strong text-phosphor hover:bg-phosphor hover:text-on-phosphor` |
| quiet action button | `border-line text-ink hover:border-line-strong hover:text-phosphor` |
| filled chip | `bg-phosphor text-on-phosphor` |
| card / frame | `bg-surface border-line`; inner pill `bg-elevated` |
| terminal role glyphs | `❯` phosphor · `✓` success · `•` muted · dots `dim` (BRANDING.md §5) |
| status colors | only for their meaning (warn/error/success/info), never decoration |

Copy rules: savings numbers only if they come from a real `count_tokens`
measurement — never invented (CLAUDE.md "measurement honesty"). The one
sanctioned stat line is the brand tagline "2× the context. ½ the cost."
(brand/parsecbrandkit/BRANDING.md §4).

`src/components/brand.tsx` is a deliberate copy of
`packages/frontend/src/components/brand.tsx` — change them together.
