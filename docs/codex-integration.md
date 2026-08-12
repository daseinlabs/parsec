# Codex CLI integration — implementation notes

**Status:** Milestone A (pure passthrough + installer) and Milestone B (the
Responses curation adapter) implemented 2026-08-11. Research background:
`docs/codex-cli-integration-research.md` (assumed-true basis); phasing:
`docs/tool-integrations-survey.md` §3.6 Phase 2. Codex traffic now runs
through the SAME deterministic Freezer as Claude Code — trimming stale tool
outputs — with per-wire fold-back keeping served bytes stable for OpenAI's
automatic prefix cache. Savings measurement (Milestone C) is still open:
rows carry billed usage and `freeze_cut_tokens`, `counterfactual` stays null.

## What shipped

| Piece | Where | What it does |
|---|---|---|
| `/openai/*` + `/chatgpt/*` routes | `packages/proxy/src/openai.rs` + worker routes in `server.rs` | Two namespaces, one Responses wire. `/openai/*` → `https://api.openai.com` (BYOK custom provider; `PARSEC_OPENAI_UPSTREAM` overrides). `/chatgpt/*` → `https://chatgpt.com/backend-api/codex` (subscription mode, the default install; `PARSEC_CHATGPT_UPSTREAM` overrides): the profile's `openai_base_url` points Codex's built-in provider here, the OAuth Bearer + `chatgpt-account-id` make ONE localhost hop — the Claude Code subscription posture (§3). Non-model backend calls (login/connectors/apps, `chatgpt_base_url`) are untouched. Sibling endpoints (`responses/compact`, `models`, `search`, `session`, …) relay verbatim, all methods. WebSocket Upgrade attempts are answered 426 locally so Codex falls back to HTTP SSE — the subscription path NEEDS this (built-in provider tries WS first). Subscription rows attribute `tool=codex` from the route (a base-URL override can't add headers); BYOK keeps the `http_headers` tag. **Product decision 2026-08-12: subscription default-on for auth parity with Claude Code; OpenAI ToS review remains open before public release (survey §3.7.2).** |
| Ledger attribution | `openai.rs` (`ledger_usage`, `SseScan`) | POST `/openai/v1/responses` writes a savings-ledger row from actually-billed usage (JSON body, or the SSE `response.completed` event; Drop-finalized so client aborts still record what was observed). `counterfactual_input_tokens` is **null** — OpenAI has no count_tokens endpoint and §8.4 forbids estimating. OpenAI's `input_tokens` includes the cached prefix, so the row splits it: `billed_input + billed_cache_read` re-sums exactly to OpenAI's figure; `billed_cache_write = 0` (not billed separately on OpenAI). `conv_id` = sha256 of Codex's per-session `prompt_cache_key` (id-from-id, never text). |
| Supervisor path-awareness | `supervisor.rs` | `/openai/*` fallback (worker down) goes to the OpenAI upstream with the prefix stripped — the Anthropic fallback would have 404'd the wire exactly when fail-open matters. Upgrade deflection also lives at the routed port. |
| Attribution bug fix | `supervisor.rs` `worker_headers` | The supervisor's Anthropic-hop allowlist was stripping `x-parsec-tool` before the worker saw it, so **opencode rows lost their tool tag whenever traffic went through the supervisor (the normal path)**. The tag (charset-gated) is now re-added on the supervisor→worker hop; it still never leaves the machine's parsec hops. |
| Responses adapter (Milestone B) | `packages/proxy/src/responses.rs` + `curate_responses` in `openai.rs` | The Responses sibling of `internal.rs`+`splice.rs`: projects the wire body into the flat internal view (`instructions`→system; `message`→user/assistant/system; `function_call`→assistant with `tool_calls` + bash-twin `extra.actions`, argv arrays joined so chunk typing sees real shell commands; `function_call_output`→`tool` observation; reasoning/unknown types→inert `opaque` entries), serves it through the SAME `Freezer`/`BrainScorer` as the Anthropic path, and folds curated text back positionally onto the original items. Only text is ever rewritten: `reasoning.encrypted_content`, tool-call pairing, and every top-level field (`prompt_cache_key`, `store`, `include`, `tools`) ride through byte-identical by construction. The per-conversation fold map replays served bytes exactly across turns — the stand-in for cache_control on a wire with automatic prefix caching. Fail-open floor: any curation error forwards the original bytes verbatim, counted, `fail_open` on the row. No brain configured ⇒ passthrough curation with the wire freeze still live. |
| Installer | `parsec setup codex` (`packages/proxy/src/setup_codex.rs`) | Appends a marker-bounded `[model_providers.parsec]` block to `$CODEX_HOME/config.toml` (pure text append/replace — the user's bytes are never reserialized) and writes the standalone profile `$CODEX_HOME/parsec.config.toml` (`model_provider = "parsec"`). Opt-in per invocation: `codex --profile parsec`; plain `codex` is unaffected. `parsec disable codex` removes exactly our block + profile; `parsec uninstall` does the same quietly. Foreign files/blocks (no marker/sentinel) are never touched. |

The provider block the installer writes:

```toml
# >>> parsec-managed codex provider — do not edit; `parsec disable codex` removes this block >>>
[model_providers.parsec]
name = "parsec (context compression proxy)"
base_url = "http://127.0.0.1:8082/openai/v1"
env_key = "OPENAI_API_KEY"
wire_api = "responses"
http_headers = { "x-parsec-tool" = "codex" }
# <<< parsec-managed codex provider <<<
```

## Decisions and their whys

- **Namespaced route (`/openai/*`), not port-per-tool.** One shared proxy on
  the routed port; the prefix keeps the two wires impossible to confuse and
  the supervisor's fallback able to pick the right upstream. Attribution
  rides Codex's own `http_headers` config surface through the existing
  charset-gated `x-parsec-tool` seam.
- **Headers forward verbatim-minus-hop-by-hop on this wire** (vs the
  Anthropic auth allowlist): Codex sends provider headers (`openai-beta`,
  org/project ids) an allowlist would silently break. `x-parsec-*` is
  stripped before anything leaves for OpenAI. `accept-encoding` is stripped
  so the upstream answers identity — the usage scan (and the future adapter)
  must be able to read the wire.
- **Routing is DEFAULT-ON for every codex session** (revised 2026-08-12; the
  opt-in `--profile parsec` flow is gone). Two marker blocks in the user
  config: a HEAD block prepended with the top-level routing key (TOML
  requires top-level keys before any table — appending after the user's
  `[mcp_servers.*]` tables would land inside them), and the TAIL block with
  the BYOK provider table plus a `[[hooks.SessionStart]]` hook running
  `parsec up` (default-on means every session depends on the proxy, so
  every session revives it — the Claude Code self-heal). Subscription mode
  writes `openai_base_url = ".../chatgpt"`; `--byok` writes
  `model_provider = "parsec"` instead. Re-running setup switches modes; a
  user-set copy of the same key outside our block is refused, never
  stomped. The config docs explicitly sanction `openai_base_url` for "an
  LLM proxy or router" (learn.chatgpt.com/docs/config-file/config-advanced)
  — which materially softens the subscription-proxying ToS caveat.
- **Commands are skills** (revised 2026-08-12): five `parsec-*/SKILL.md`
  dirs (savings, proxy, key, setup, uninstall), invoked via `/skills`, a
  `$parsec-savings` mention, or implicitly. Installed into BOTH
  `~/.codex/skills/` (what codex 0.147.0 demonstrably scans — it creates
  that dir itself) and `~/.agents/skills/` (what the docs describe) —
  doc-vs-binary skew, so cover both. Custom prompts (`~/.codex/prompts/`,
  `/prompts:` namespace) are deprecated upstream and were not surfacing in
  0.147.0; setup migrates old installs (removes our sentinel-gated prompt
  files and the legacy opt-in profile).
- **Live wire findings (2026-08-12), all handled in `openai.rs`:**
  (1) Codex sends `content-encoding: zstd` request bodies — decoded
  (zstd/gzip/deflate) for curation, curated bytes forwarded identity with
  the stale header dropped; an undecodable body fails open verbatim WITH its
  header. (2) The ChatGPT backend serves its SSE stream with NO content-type
  header — SSE detection trusts the request's `"stream": true` flag
  (header sniff as fallback), else the relay buffers (breaking incremental
  display) and the usage scan reads nothing. (3) codex 0.147.0 rejects
  `async = true` hooks by skipping them — the SessionStart hook is
  synchronous (`parsec up` detaches instantly anyway). No-row outcomes are
  never silent: both response paths log event types / top-level key names
  (schema, never content) when usage is absent.
- **No row for sibling endpoints** (`responses/compact`, `models`, …): not
  model turns, nothing to attribute.
- **Unentitled = inert**, same as the Anthropic path: traffic relays, no rows.

## Distribution (added 2026-08-12)

`scripts/install.sh` — ONE unified installer that auto-detects the coding
agents on the machine (claude via `command -v`; codex via
`command -v`/`$CODEX_HOME`; opencode via `command -v`/XDG config dir) and
activates each:

```sh
curl -fsSL https://raw.githubusercontent.com/daseinlabs/claude-plugins/main/install.sh | bash
# explicit: … | bash -s -- claude | codex [--byok] | opencode
```

Claude Code gets the plugin (`claude plugin marketplace add` +
`claude plugin install parsec@parsec-marketplace` — the full surface:
statusline, hooks, skills; manual instructions printed on failure).
codex/opencode get the platform binary downloaded into
`~/.parsec/bin/parsec` (fresh-inode rename, macOS code-sign safe) followed
by `parsec setup <tool>`; the download is skipped when Claude Code is the
only tool found (the plugin ships its own binary). release.yml publishes it
beside the binaries from the same commit.
`install-opencode.sh` remains as a compat stub (that URL shipped 2026-08-11)
forwarding to `install.sh` pinned to opencode. Claude Code plugin users skip
the download entirely: `parsec setup codex` from the installed binary.

## Not done yet (follow-ups — Milestone C and beyond)

- **Measurement**: local-tokenizer counterfactual, labeled as estimated
  (open question §3.7.1) — until then codex rows show billed usage plus the
  `freeze_cut_tokens` diagnostic (a trim figure, never a savings claim).
- **`responses/compact` policy**: now that curation is live the two
  compactors CAN fight — today `responses/compact` still passes through
  untouched; decide intercept/disable (§3.7.3). Note a server-side compaction
  changes the transcript the client resends, which the freezer's purity
  guard handles as a fresh-prefix reset (correct, but cache-cold).
- **Not ported to this wire yet**: tool-roster pruning, the governor, the
  curator's per-range decision log, and `PARSEC_RECORD_DIR` capture — all
  Anthropic-path-only for now.
- ~~Command surface~~ shipped 2026-08-12 as prompts, migrated to skills the
  same day (see decisions above). On-demand is the savings-display ceiling:
  the Codex status line is a closed item catalog.
- **Free-tier UX drops still open**: `PreToolUse` no-reread hook,
  `[agents]` roles (no proxy dependency).
- **Pinned-version smoke test in CI**: 2026-08-12 surfaced five separate
  doc-vs-binary skews in codex 0.147.0 (prompts dead, skills dir, `async`
  hooks, zstd request bodies, content-type-less SSE) — a CI job driving
  `codex exec` at a pinned release through the proxy would catch the next
  one before a user does (same follow-up shape as opencode's).
- **Research refresh**: milestones A+B assumed the 2026-07-28 research
  verbatim; re-verify `model_providers` fields (esp. `http_headers`) and the
  Codex-side item shapes (`function_call` argv arrays, `function_call_output`
  output forms) against Codex HEAD before release.
- Per-tool savings split in `parsec savings` / statusline / dashboard (rows
  carry `tool`; nothing reads it yet — shared with opencode).

## Manual test

```
cargo build -p parsec-proxy
target/debug/parsec setup codex          # provider block + profile + warm proxy
# restart any already-running proxy so the new /openai route is live:
curl -X POST http://127.0.0.1:8082/shutdown && target/debug/parsec up
OPENAI_API_KEY=sk-… codex --profile parsec   # any session
tail -f ~/.parsec/proxy.log                  # per-request relay lines
grep '"tool":"codex"' ~/.parsec/ledger.jsonl | tail
target/debug/parsec disable codex        # clean removal, user config untouched
```
