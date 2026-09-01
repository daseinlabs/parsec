# Install tracking — fleet accounting for parsec clients

Added 2026-08-31. Answers two product questions the savings ledger alone
could not: **how many machines have parsec installed**, and **which harnesses
each machine actually runs** (Claude Code / Claude Desktop / Codex /
opencode), at both the install level and the per-request level.

## What ships, and when

One `install-report/v0` ping (`contracts/schemas/install-report.schema.json`)
to `POST {platform}/installs`:

```json
{
  "contract_version": "install-report/v0",
  "install_id": "ins_<32 hex>",
  "ts": "2026-08-31T10:15:00Z",
  "version": "0.2.6",
  "os": "macos",
  "arch": "aarch64",
  "harnesses": ["claude-code", "claude-desktop"]
}
```

That is the **entire payload**. Every field is pattern-gated
(`additionalProperties: false` + regex per field), so this path cannot carry
raw text — the same contract-level guarantee as the ledger's `tool` and
`session_id` fields. No hostname, no username, no hardware identifiers.

Send moments (`packages/proxy/src/install.rs`):

- `parsec setup` reaching the `ready` phase — the "install completed" moment.
- `parsec key set` — the key flips the report from anonymous to
  account-linked (the platform COALESCEs, so a later keyless ping never
  unlinks).
- Proxy worker startup, then a re-check every 6h — this is what keeps
  `last_seen` honest for long-lived proxies and picks up harness/key changes
  made while the worker was already running.

All sends are deduped locally to **at most one ping per day** unless the
fingerprint (version | configured harnesses | key identity as a sha8)
changed. All sends are fail-open: blocking CLI sends have a 5s timeout, the
worker send is a detached task, and every failure is a debug log — a dead
platform never makes setup or serving look broken.

## The install id

`ins_<32 hex>`, minted **randomly** (time ‖ pid ‖ home-path hashed — not
derived from hardware) into `~/.parsec/install.json` on the first send
moment. Lifecycle:

- The serving path only **reads** it: `write_ledger` stamps `install_id` on
  every savings-ledger row (optional field), which joins per-request traffic
  to the fleet view. Rows written before the id exists simply lack the field.
- `parsec uninstall` / purge deletes `~/.parsec` including the id; a
  reinstall mints a fresh one and counts as a new install. That is the honest
  reading, and it doubles as the privacy story: purging really does sever the
  history.

## Harness attribution, both levels

- **Per request** (ledger `tool` field): the `x-parsec-tool` header set by
  each shim (`claude-desktop`, `codex` — also route-inferred on `/chatgpt/*`
  — and `opencode`). New in this change: untagged Anthropic-wire traffic is
  stamped **`claude-code` explicitly** instead of encoding Claude Code as
  absence. Absent now means "older row or untagged non-Anthropic wire";
  readers coalesce absent → claude-code.
- **Per install** (report `harnesses` array): what the machine is
  *configured* to intercept at report time — setup_state phase `ready`
  (claude-code), desktop state `enabled` (claude-desktop), the
  `parsec-managed codex` marker in `~/.codex/config.toml` (codex), and the
  opencode plugin file (opencode).

## Server side (packages/platform)

- `POST /installs` — upsert on `install_id` into the `installs` table
  (migration `0008_installs.sql`; `store.py::_SCHEMA` mirror). Keyless pings
  are accepted (`optional_key_account`): an install exists before onboarding.
  Idempotent and abuse-bounded — pattern-gated fields, one row per id.
- `GET /installs/summary` (Supabase-JWT-gated) — aggregate-only fold
  (`store.fold_installs`): `installs_total`, `active_7d`/`active_30d` (from
  `last_seen`), `linked_accounts`, `by_version`, `by_os`, `by_harness`. Note
  this is a product-wide metric: any signed-in account sees the same
  numbers. If the dashboard grows a public/admin split, move this behind it.

The pydantic mirror (`models.py::InstallReport`) is drift-guarded against the
schema and its committed example the same way the ledger is
(`test_install_schema_and_mirror_have_identical_field_sets`).

## What this deliberately is not

- Not telemetry (§6): no traces, no metrics, no content — the `parsec:share`
  consent pipeline remains unbuilt and unaffected.
- Not a modeled activity metric: "active" means the machine's proxy or CLI
  actually pinged inside the window; ledger rows carry the id for exact
  per-machine usage joins when needed.
