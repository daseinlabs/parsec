# frontend — the Dasein dashboard

The account/savings dashboard for the platform service (`packages/platform`):
GitHub sign-in via Supabase Auth, the savings-ledger summary, brain-API key
minting, and hosted-Stripe billing links. The public landing page is NOT here
(that's dasein-frontend / daseinlabs.github.io) — this is the app you land in
after signing in.

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
- `src/app/dashboard` — ledger summary tiles (§8.4 counterfactual numbers)
- `src/app/account` — key minting (shown once), Stripe checkout/portal links;
  the checkout link carries `client_reference_id=<account id>` — the join the
  platform Stripe webhook depends on
- `src/app/api/keys` — BFF proxy for minting (browser → same-origin → platform)
