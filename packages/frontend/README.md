# frontend — the parsec dashboard

The account/savings dashboard for the platform service (`packages/platform`):
GitHub sign-in via Supabase Auth, the savings-ledger summary, brain-API key
minting, and hosted-Stripe billing links. The public landing page is NOT here —
this is the app you land in after signing in.

Next.js 16 (App Router) + Supabase SSR auth. All platform API calls happen
**server-side** (BFF pattern): the browser only ever talks to this app, so the
platform service needs no CORS and tokens stay out of client JS.

## Run

```bash
bun install
cp .env.example .env.local   # fill in Supabase project values
bun run dev                  # platform API expected at PLATFORM_URL
```

Without env configured, pages render a setup notice instead of crashing —
`bun run build` needs no secrets.

## Map

- `src/proxy.ts` — Supabase session refresh (Next 16's middleware convention)
- `src/lib/supabase/` — server (cookie) + browser clients
- `src/lib/platform.ts` — server-side platform API calls with the session JWT
- `src/app/login` · `src/app/auth/*` — OAuth flow (GitHub → code exchange → cookie)
- `src/app/page.tsx` — `/` is the dashboard: ledger summary tiles (§8.4
  counterfactual numbers), or a redirect to `/login` when signed out. There is
  no separate landing page.
- `src/app/account` — key minting (shown once), Stripe checkout/portal links;
  the checkout link carries `client_reference_id=<account id>` — the join the
  platform Stripe webhook depends on
- `src/app/api/keys` — BFF proxy for minting (browser → same-origin → platform)

## Brand

parsec is retro-terminal: phosphor green (`#4AF626`) on the void (`#0A0E0C`),
JetBrains Mono everywhere, one glowing star. Dark is the only theme here.

- `src/app/globals.css` — the tokens as a Tailwind v4 `@theme` block
  (`bg-void`, `text-phosphor`, `border-line`, `text-muted`, …). Mirrors
  `brand/parsec-brand-kit/tokens/tokens.json`, which is the source of truth —
  change it there first.
- `src/components/brand.tsx` — the mark (inlined SVG, `currentColor`), the
  lockup, and the shared nav/title bits.
- `src/app/icon.svg` · `src/app/apple-icon.png` — favicon / app icon
  (Next's metadata file convention; no `<link>` tags needed).
- `public/parsec-mark-{flat,glow}.svg` — the mark for non-React consumers.

Use the tokens, not raw hexes. The glow is an accent, not a default: `.glow`
(text) and `.glow-mark` (SVG) exist for the one number or mark that carries the
page.
