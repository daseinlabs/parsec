# Per-user savings reporting (proxy → platform → dashboard)

How a user's measured token savings get from the local proxy to their dashboard,
per user and per model, with cost.

This is the **control plane** (DIRECTION.md §7c): accounts, the brain-API key,
the savings ledger, and per-account reporting. The data plane (curation, the
count_tokens counterfactual) stays on the user's machine; only the *measured
result* — never message text — crosses to the platform, authenticated with the
user's own account key.

## The pipeline

```
 proxy (user's machine)                 platform (our cloud)          dashboard
 ─────────────────────                  ────────────────────          ─────────
 compute savings                        POST /ledger  (dsn_-key auth)  GET /ledger/summary
 (count_tokens counterfactual           → store row in Supabase        GET /ledger/usage
  vs actually-billed usage, §8.4)          (ledger table + extra JSONB) → per-model + per-day
 write_ledger →                         GET /ledger/summary|usage         cost & savings
   • ~/.dasein/ledger.jsonl (local)       aggregate + join model_pricing
   • ship row to platform (if keyed)      → cost at report time
```

- **Measurement honesty (§8.4).** Savings come only from the per-request
  `count_tokens` counterfactual (original body vs actually-billed usage), never a
  modeled baseline. A failed probe stores `counterfactual_input_tokens = NULL` —
  a hole, never an imputed zero. Summaries derive savings only over rows where the
  probe succeeded.
- **Local first, ship second.** Every row is written to `~/.dasein/ledger.jsonl`
  regardless (the source of `/dasein-savings` and the status line). Shipping to
  the platform is an *additional* fire-and-forget POST — it never blocks or fails
  a request. Ingest is idempotent on `request_id`, so retries/dupes are harmless.
- **What ships:** the savings-ledger row
  (`contracts/schemas/savings-ledger.schema.json`) — token counts, `model`,
  `conv_id`/`session_id` (opaque ids), and capture/governor telemetry. **No
  message text.**

## Configuring the account key (the user-facing part)

The proxy ships rows only when it has the user's per-account **`dsn_` API key**.
The user mints it in the dashboard (**Account → Brain API key**) and sets it from
inside Claude — no env vars, no restart:

```
dasein key set dsn_…                                   # release build (platform URL baked in)
dasein key set dsn_… --platform-url http://127.0.0.1:8080   # dev build (no baked URL)
dasein key show                                        # masked key + resolution source + shipping status
dasein key clear                                       # stop reporting
```

The `/dasein-key` plugin skill wraps the same commands conversationally.

- **Storage:** `~/.dasein/credentials.json`, mode `0600` (bearer secret).
  Written atomically by `dasein key set` (`credentials.rs`).
- **Takes effect on the next request, no restart.** `ledger_ship::resolve()`
  reads the key live per shipped row, so a mid-session `key set` ships the very
  next row.
- **Resolution / precedence** (`ledger_ship::resolve`) — shipping is active only
  when BOTH resolve:
  - **Platform URL:** `DASEIN_PLATFORM_URL` env → baked `DASEIN_DEFAULT_PLATFORM_URL`
    (stamped into release binaries by `release.yml`) → `credentials.json`
    `platform_url`.
  - **API key:** `DASEIN_API_KEY` env → `credentials.json` `api_key`.
- **Env overrides the file.** `DASEIN_API_KEY` / `DASEIN_PLATFORM_URL` in the
  environment win over the stored file — that's the CI / self-host path; the file
  is the end-user path.
- **Dev-build gotcha:** a locally-built proxy has **no baked platform URL**, so
  `dasein key show` reports *shipping: inactive* until you also pass
  `--platform-url` (or set `DASEIN_PLATFORM_URL`). Released builds bake it, so the
  key alone is enough.

Confirm shipping is live in `~/.dasein/proxy.log`:

```
per-account savings shipping active (API key configured) — rows post to the platform ledger …
```

## Platform APIs

FastAPI app (`packages/platform`, `dasein_platform.app:create_app`). Two
credentials (`auth.py`):

- **Supabase JWT** (`Authorization: Bearer …`, `aud=authenticated`) — identifies
  a human/account for dashboard-facing endpoints. Verified via
  `SUPABASE_JWKS_URL` (asymmetric, preferred) or `SUPABASE_JWT_SECRET` (legacy).
- **Brain-API key** (`dsn_…`, `X-Dasein-Key` header) — identifies the proxy for
  machine-to-machine calls (ledger ingest). Stored **hashed** (SHA-256); the raw
  key is shown once at mint and never persisted.

| Method & path | Auth | Purpose |
|---|---|---|
| `GET /health` | none | Liveness. |
| `POST /keys` | Supabase JWT | Mint a `dsn_` key for the account (returned once; only its hash is stored). |
| `GET /keys/validate/{key}` | none | "Is this key valid + entitled?" — the §7 seam the brain could ask. Unknown ⇒ `{valid:false}` (not 404). |
| `POST /ledger` | `dsn_` key | Ingest one savings-ledger row, attributed to the key's account. Idempotent on `request_id`. |
| `GET /ledger/summary` | Supabase JWT | Account totals + savings + **`by_model`** (per-model tokens & cost) + total **`cost_usd`**. |
| `GET /ledger/usage?days=N` | Supabase JWT | Per-day usage series (`days` 1–365, default 30): tokens, tokens_saved, cost per day. |
| `POST /webhooks/stripe` | Stripe sig | Entitlement flag from Stripe events. |

All reporting endpoints are **self-scoped** — `require_account` returns the
caller's own account from the JWT, so a user only ever sees their own usage.
(Admin cross-user viewing would need a role check; not implemented.)

## Storage & migrations

Store seam (`store.py`): **SQLite** by default (`DASEIN_PLATFORM_DB` path or
in-memory — zero-infra dev), **Postgres/Supabase** when `DASEIN_PLATFORM_DB_URL`
is set (`pgstore.py`). Schema lives in `packages/platform/migrations/*.sql`
(applied via the Supabase CLI — the service never runs DDL at startup). The
SQLite `_SCHEMA` mirrors it; keep them in sync.

Tables (after `0001` + `0002`):

- `entitlements`, `api_keys` (key_hash → account), `stripe_customers`.
- **`ledger`** — one row per request. Core typed columns (billing/aggregation):
  `request_id` (PK), `account_id`, `conv_id`, `ts`,
  `counterfactual_input_tokens` (nullable), `billed_{input,output,cache_read,cache_write}_tokens`,
  `cache_prefix_sha8`, `fail_open`, plus an **`extra JSONB`** column holding the
  evolving capture/governor seams (`model`, `session_id`, `brain_ms`, `tools_*`,
  `gov_*`, …). New seams need **no migration** — they land in `extra`. An
  expression index `ledger_model ON ((extra->>'model'))` makes per-model grouping
  fast.
- **`model_pricing`** — `(model, input_per_mtok, output_per_mtok,
  cache_read_per_mtok, cache_write_per_mtok, currency)`, seeded with current
  Anthropic list prices. Cost is computed at **report time** by joining ledger
  token sums to this table, so a price change is one `UPDATE` and never rewrites
  history.

Apply migrations to Supabase:

```sh
# via the Supabase CLI (tracked)
supabase db push
# or apply a file directly (idempotent: ADD COLUMN/CREATE TABLE IF NOT EXISTS,
# INSERT … ON CONFLICT DO NOTHING) — but this bypasses migration tracking:
psql "$DASEIN_PLATFORM_DB_URL" -f packages/platform/migrations/0002_ledger_extra_pricing.sql
```

⚠️ **If `0002` is not applied, `/ledger/summary` and `/ledger/usage` 500** (they
reference `ledger.extra` and `model_pricing`), which the dashboard shows as
"unreachable" — see Troubleshooting.

## Model & cost granularity

- **Model** is stored in `extra->>'model'` (queryable via the expression index) —
  no dedicated column needed.
- **Per-type token counts are already the right granularity for cost:** input,
  output, cache-read, and cache-write are priced differently, and the ledger
  splits them.
- **Cost = Σ(tokens × per-MTok price) / 1e6**, per model, joined to
  `model_pricing`. Unpriced models (stale/unknown ids) report `cost_usd = null` —
  a hole, never a fabricated zero.

Two caveats baked into the math:

- **Savings is input-side only.** The counterfactual is input tokens
  (`count_tokens` on the original body), so `tokens_saved =
  counterfactual_input − billed_input`. There is no counterfactual for
  output/cache — don't present a "total cost saved" that includes them.
- **Cache-write TTL.** The ledger stores one `billed_cache_write_tokens` and
  can't distinguish 5-minute vs 1-hour TTL, so `model_pricing` uses the **5m rate
  (1.25× input)**. Heavy 1h-cache use (2× input) undercounts write cost ~1.6×.

## Dashboard

`packages/frontend` (Next.js, App Router). Server-side BFF (`lib/platform.ts`) —
the browser never talks to the platform directly; the Supabase session JWT is
attached server-side, and `PLATFORM_URL` points at the platform service.

`dashboard/page.tsx` shows (when the account has rows):

- Tiles: tokens saved, **spend** (`cost_usd`), billed input, would-have-been
  input, cache reads, fail-open count.
- **By model** table: requests, tokens saved, billed input, cost per model.
- **Usage — last 30 days**: per-day cost bar chart with tokens-saved annotations
  (from `/ledger/usage`).

Empty account ⇒ "No ledger rows yet…". Fetch failure ⇒ "unreachable" (see below).

## Running / deploying the platform

Local dev:

```sh
cd packages/platform
set -a; . ./.env; set +a          # DASEIN_PLATFORM_DB_URL, SUPABASE_JWKS_URL, …
.venv/bin/python -m uvicorn --factory dasein_platform:create_app --port 8080
```

Container: `packages/platform/Dockerfile`
(`uvicorn --factory dasein_platform:create_app --host 0.0.0.0 --port ${PORT}`).
Point `DASEIN_PLATFORM_DB_URL` at Supabase's **transaction pooler** (port 6543) —
Cloud Run instances churn and would exhaust direct connections.

Relevant env:

| Var | Purpose |
|---|---|
| `DASEIN_PLATFORM_DB_URL` | Postgres/Supabase DSN. Unset ⇒ SQLite (`DASEIN_PLATFORM_DB`). |
| `SUPABASE_JWKS_URL` / `SUPABASE_JWT_SECRET` | Dashboard JWT verification (JWKS preferred). |
| `STRIPE_WEBHOOK_SECRET` | Stripe webhook signature check. |

Client side, so released proxies ship without any per-user config beyond the key:

| Var | Where |
|---|---|
| `DASEIN_DEFAULT_PLATFORM_URL` | Baked into release binaries by `.github/workflows/release.yml` (repo variable). |
| `DASEIN_PLATFORM_URL` | Runtime override (required on dev builds — no baked URL). |
| `DASEIN_API_KEY` | Runtime override of the stored key (CI/self-host). |

## Go-live checklist

1. **Apply migrations** `0001` + `0002` to the Supabase DB.
2. **Set `DASEIN_DEFAULT_PLATFORM_URL`** (GitHub Actions repo variable) and cut a
   release so binaries know where to POST.
3. Ensure the platform service is **running** and reachable at `PLATFORM_URL`.
4. User **mints a key** in the dashboard and runs **`dasein key set dsn_…`**
   (release) — or the `/dasein-key` skill.
5. Confirm `dasein key show` reports *shipping: active* and `proxy.log` shows the
   shipping line; run traffic; rows appear in the dashboard.

## Troubleshooting

**Dashboard says "The platform API is unreachable — check PLATFORM_URL."** This
message fires on *any* failure of the server-side fetch, not just a bad URL.
Likely causes, in order:

1. **Platform not running** — `curl -s -o /dev/null -w '%{http_code}'
   http://127.0.0.1:8080/health` returns `000`/connection-refused. Start it (see
   above).
2. **Migration `0002` not applied** — an authenticated request runs the per-model
   query against `ledger.extra` / `model_pricing`; if missing → 500 → "unreachable".
   Check: `information_schema.columns` for `ledger.extra`, and that `model_pricing`
   exists.
3. **Auth (401)** — platform's Supabase JWT verification misconfigured
   (`SUPABASE_JWKS_URL` / audience).
4. **Endpoint 404** — the running platform predates `/ledger/usage`; redeploy the
   current code.

**No rows in the dashboard, but curation is working locally.**

- `dasein key show` → *shipping: inactive*? Missing key or (dev build) platform
  URL — set both.
- Installed plugin binary predates the shipping code — rebuild the local binary
  (`scripts/refresh_plugin_bin.sh`) or cut a fresh release.
- `~/.dasein/proxy.log` — a `ledger ship: platform rejected row` / `platform
  unreachable` debug line points at ingest 4xx/5xx or a down platform (shipping is
  best-effort, so the row is still on disk and can be backfilled).

## Related

- `docs/environment-variables.md` — full client env surface.
- `docs/deploy-cloud-run.md` — brain deploy (separate service from the platform).
- `DIRECTION.md` §7c — the control-plane design this implements.
- `contracts/schemas/savings-ledger.schema.json` — the row contract (authoritative
  across proxy + platform).
