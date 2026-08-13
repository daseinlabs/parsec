# Harness research: pi, Hermes, Grok Build

Research date: 2026-08-12. Pre-implementation survey in the style of
`codex-cli-integration-research.md`, evaluated against the adapter model in
`tool-integrations-survey.md` §3.1 and the costs observed in the opencode
(`ba3a6ca`, ~620 LOC same-wire) and codex (`70a09b7`, ~2,800 LOC new-wire)
integrations. Claims are tagged `(doc)` / `(src)` / `(inference)`.

## 0. What parsec serves today (baseline for the verdicts)

- **Anthropic Messages** (`/v1/messages`): full curation pipeline, cache
  breakpoints, tool pruning, and the free `count_tokens` counterfactual —
  the only wire with honest savings measurement.
- **OpenAI Responses** (`/openai/*` BYOK, `/chatgpt/*` subscription):
  curation via `responses.rs`, **no counterfactual** (OpenAI has no
  count-tokens endpoint; `counterfactual_input_tokens` is always null).
- **No OpenAI Chat Completions adapter.** Survey §3.7 listed it "new —
  small, low priority". Both Hermes and Grok Build raise its priority
  (see §4).

Summary verdicts up front:

| Harness | Wire | Interposition | Subscription auth | Savings measurement | Verdict |
|---|---|---|---|---|---|
| **pi** | Anthropic Messages (also OpenAI Responses / codex-responses) | `models.json` provider `baseUrl` — config only, no shim | **Works** (Claude Pro/Max OAuth honors the override) | Full count_tokens counterfactual | **Tier 1 — cheapest add ever; ≤ opencode effort** |
| **Grok Build** | Chat Completions (default) / Responses / Messages, selectable per model | per-model `base_url` in `~/.grok/config.toml` (BYOK) | **Blocked** — bearer gate refuses http/loopback; needs local TLS MITM + `GROK_EXTRA_CA_BUNDLE` | Partial: xAI `tokenize-text` exists but is text-level, not request-level | **Tier 2 — BYOK-only first, like opencode** |
| **Hermes** | Chat Completions (canonical) / Anthropic Messages (Claude Max) | `provider: custom` `base_url` in `~/.hermes/config.yaml` | Claude Max OAuth path plausible but base-URL key unverified | Anthropic path: full; Chat Completions path: none | **Tier 3 — feasible, but adapter cost + own built-in compression stack on ours** |

---

## 1. pi (Earendil / Mario Zechner, `badlogic/pi-mono`)

TypeScript, ~39K stars. npm `@earendil-works/pi-coding-agent`, binary `pi`,
config dir `~/.pi/agent/`. Docs: pi.dev/docs/latest.

### 1.1 Interception

- **No `ANTHROPIC_BASE_URL` env support** `(src: env-api-keys.ts)`. Base
  URLs come exclusively from the model object, configured in
  **`~/.pi/agent/models.json`**, hot-reloaded without restart `(doc)`.
- Overriding a built-in provider is first-class `(src:
  provider-composer.ts:195,215)`:

  ```json
  { "providers": { "anthropic": { "baseUrl": "http://127.0.0.1:8082" } } }
  ```

  Auth resolution is untouched by the override, and **the OAuth client also
  uses `model.baseUrl`** — Claude Pro/Max subscription traffic follows the
  redirect `(src)`. No TLS pinning, no signing; localhost http needs no
  certs `(src: grep clean)`.
- Provider entries support custom `headers` → attribution via
  `"headers": {"x-parsec-tool": "pi"}` with no shim `(src:
  model-config.ts:140-210)`.
- Alternative host: pi extensions (`before_provider_request`,
  `registerProvider`) — richer, but TypeScript, so it would need the same
  §7b exemption debate as the opencode shim. **Not needed**: the config
  write covers interception + attribution, keeping the integration pure
  Rust. `(inference)`

### 1.2 Wire

- pi's Anthropic path uses the official SDK, SSE streaming, `system` as
  text-block array **with `cache_control` breakpoints attached** — parsec
  already clears client anchors and re-anchors (`splice.rs:402-479`), so
  this is the same shape Claude Code sends. `(src)`
- On Claude OAuth, pi runs "stealth mode": Claude Code identity system
  block, `anthropic-beta: claude-code-20250219,oauth-2025-04-20`,
  Bearer auth `(src: anthropic-messages.ts)`. All of that rides through the
  `forward_auth_headers` allowlist (`authorization`, `anthropic-*`)
  unmodified. `(inference)`
- No Claude Code `metadata.user_id` → session identity falls back to
  `conversation_id` (hash of first two internal messages), which already
  exists on the Anthropic path. `(inference)`
- Bonus: pi's `openai` provider defaults to the **Responses API**, and it
  has an `openai-codex-responses` backend against
  `chatgpt.com/backend-api` — both map onto the existing `/openai` and
  `/chatgpt` namespaces if we ever want pi+OpenAI coverage. `(src)`

### 1.3 Measurement

Full `count_tokens` counterfactual on the Anthropic path — pi is measured
exactly like Claude Code from day one.

### 1.4 Effort estimate

**Below opencode (~620 LOC): no shim artifact at all.**

1. `setup_pi.rs`: detect `pi` on PATH or `~/.pi/agent/`; merge a
   `providers.anthropic.baseUrl` + `headers` entry into `models.json`
   (JSON merge with the same Foreign-conflict posture as
   `merge_settings` in `setup.rs` — never stomp an existing foreign
   baseUrl); sentinel strategy needed since JSON has no comments
   (`inference`: probably a `"x-parsec-managed": true` sibling key or
   exact-value match, mirroring `is_managed_statusline`).
2. `main.rs` dispatch + `lib.rs` + `uninstall()` hook.
3. Command surface: pi has skills (`~/.pi/agent/skills/`) — port the five
   commands as skill files, same as codex.
4. `install.sh` / `install.ps1` detection branch; docs; installer tests.

### 1.5 Open questions

- `models.json` merge semantics when the user already has a `providers`
  block — verify hot-reload tolerates our merged file, and that a
  provider entry with *only* `baseUrl`+`headers` doesn't clobber model
  lists. (Agent-verified at composer level, not end-to-end.)
- Does pi's retry wrapper (`provider-retry.ts`) re-send byte-identical
  bodies on retry? Matters for fingerprint commit-after-2xx logic.
  `(unverified)`
- Pin a pi version and add the per-harness CI smoke test the codex retro
  called for (`codex-integration.md:142-146`) — pi hot-reloads config and
  moves fast (recent npm scope migration `@mariozechner/*` →
  `@earendil-works/*`).

---

## 2. Grok Build (xAI, `xai-org/grok-build`)

Rust, Apache-2.0 (external contributions not accepted), binary `grok`,
config `~/.grok/config.toml` (project `.grok/config.toml`, `GROK_HOME`).
Docs: docs.x.ai/build. Note: this is the official harness; the community
`superagent-ai/grok-cli` (TS, `GROK_BASE_URL`, plain Chat Completions) is a
different, trivial-to-interpose tool we should not target.

### 2.1 Interception

- **BYOK — clean.** Per-model config `(doc: user-guide 11-custom-models.md)`:

  ```toml
  [model.parsec]
  model = "grok-4.5"
  base_url = "http://127.0.0.1:8082/xai/v1"
  env_key = "XAI_API_KEY"
  api_backend = "chat_completions"   # or "responses"
  extra_headers = { "x-parsec-tool" = "grok" }

  [models]
  default = "parsec"
  ```

  TOML config with a managed block — same format-preserving text-block
  technique as `setup_codex.rs`. `extra_headers` gives attribution for
  free. No signing, no pinning `(src: sampler grep clean)`.
- **Subscription — hard-blocked at localhost.** The session bearer is only
  attached when the URL passes `is_xai_api_bearer_url()`: **https only,
  host `*.x.ai` or exactly `cli-chat-proxy.grok.com`, loopback explicitly
  rejected** `(src: xai-grok-shell-base/src/util/mod.rs;
  config.rs:3599,5121 — fails closed / drops the bearer)`. Pointing
  `GROK_CLI_CHAT_PROXY_BASE_URL` at `http://127.0.0.1` silently loses
  auth.
  - Escape hatches exist — `HTTPS_PROXY` (reqwest honors it) plus the
    official `GROK_EXTRA_CA_BUNDLE` additive-root mechanism, i.e. a local
    TLS-terminating MITM presenting a cert for `cli-chat-proxy.grok.com`.
    Data-plane-local is preserved (the MITM is on the user's machine), but
    installing a local CA and terminating TLS is a materially bigger
    trust ask and a new proxy capability. **Recommend BYOK-only for v1**,
    exactly the opencode posture. `(inference)`

### 2.2 Wire

- Three selectable backends per model: `chat_completions` (enum default),
  `responses`, `messages` `(src: sampler/src/client.rs,
  sampling-types)`. Which backend xAI's server-delivered model catalog
  specifies in production is **unconfirmed** — the catalog comes from the
  authenticated `/v1/models` endpoint.
- If `api_backend = "responses"` works against `api.x.ai/v1/responses`
  for the target models, we can **reuse `responses.rs`** and only need:
  a new route namespace (e.g. `/xai/*`) + `PARSEC_XAI_UPSTREAM`
  (default `https://api.x.ai`) in `AppState` + supervisor path-aware
  fallback — a fraction of a new adapter. Dialect drift between xAI's
  Responses implementation and OpenAI's must be verified first (xAI also
  has `POST /v1/responses/compact`, server-side compaction — an
  interaction to watch). `(inference)`
- Otherwise it's the Chat Completions adapter (§4).
- **Header contract:** forward `x-grok-conv-id`, `x-grok-req-id`,
  `x-grok-session-id`, `x-grok-turn-idx`, `x-grok-client-version`, etc.
  verbatim (version gating happens server-side on
  `x-grok-client-version`), and pass response headers
  `x-grok-context-window` / `x-grok-max-completion-tokens` back
  untouched. The OpenAI-path policy (verbatim minus hop-by-hop) already
  does this; the Anthropic-path allowlist would strip them, so if we ever
  serve grok's `messages` backend it needs an allowlist addition. `(src)`
- `x-grok-conv-id` is a better conversation-id source than body hashing —
  analogous to `prompt_cache_key` on the codex path. `(inference)`

### 2.3 Measurement

- xAI has **`POST /v1/tokenize-text`** — a true tokenizer (returns
  `token_ids[]`) `(doc: docs.x.ai rest-api-reference)`. It tokenizes
  *text*, not a full chat request: no chat-template / tool-schema
  accounting. Using it for the counterfactual is not a modeled baseline,
  but it is also not the request-level truth Anthropic's `count_tokens`
  gives. **Open policy question** against the measurement-honesty
  invariant (DIRECTION §8): ship with `counterfactual_input_tokens: null`
  like the OpenAI wire, or define a documented, conservative
  tokenize-text-based counterfactual. Default to null until decided.
- No dedicated count-only endpoint confirmed.

### 2.4 Effort estimate

- If xAI-Responses dialect ≈ OpenAI-Responses: **between opencode and
  codex** — `setup_grok.rs` (TOML block, skills — grok has skills, hooks,
  plugins), `/xai` namespace + upstream plumbing, dialect tests.
- If Chat Completions is required: codex-scale (§4 adapter) + the above.

### 2.5 Open questions

- Production `api_backend` for grok models via BYOK (`grok inspect`
  against a real account answers this in minutes).
- xAI Responses dialect vs. `responses.rs` assumptions (unknown item
  types already project as `""` and serve verbatim, so drift degrades
  gracefully, but verify fold-back fingerprints).
- Whether `/v1/responses/compact` or 30-day server-side response storage
  interacts with prefix determinism.
- Subscription MITM route: park it; revisit only if BYOK adoption shows
  demand.

---

## 3. Hermes Agent (Nous Research, `NousResearch/hermes-agent`)

Python core + Ink TUI, MIT. Config `~/.hermes/config.yaml`, secrets in
`~/.hermes/.env`, OAuth in `~/.hermes/auth.json`, state in `state.db`
(SQLite). Docs: hermes-agent.nousresearch.com/docs.

### 3.1 Interception

- **Canonical wire is OpenAI Chat Completions** — Hermes stores history
  internally in OpenAI chat shape and converts per-transport `(src:
  agent/transports/, runtime_provider.py)`. Also `anthropic_messages`
  (Anthropic SDK, used for API-key and **Claude Max OAuth**),
  `codex_responses`, `bedrock_converse`.
- First-class custom endpoint `(doc: FAQ, configuration)`:

  ```yaml
  model:
    provider: custom
    base_url: http://127.0.0.1:8082/v1   # Hermes appends /chat/completions
  ```

  Explicitly documented for LiteLLM routers / corporate proxies. No
  signing or pinning found (not exhaustively verified from source).
- **Credential trap:** `runtime_provider.py` attaches API keys by
  *hostname matching* (e.g. `OPENROUTER_API_KEY` only when the host is
  `openrouter.ai`) — pointing a vendor `*_BASE_URL` at localhost can
  silently drop the credential `(src)`. The robust path is
  `provider: custom` with an explicit `api_key`, with parsec forwarding
  the real upstream key.
- Anthropic base-URL override exists in code
  (`cfg_base_url or explicit or api.anthropic.com`) but the **config key
  path is unverified** — must be confirmed before the Claude Max route is
  real.

### 3.2 The structural problem: Hermes compresses too

Hermes ships its own context compression: triggers at 50% of context,
keeps a 20% tail, protects the last 20 messages, `/compress` command, a
dedicated auxiliary compression model `(doc)`. Parsec freezing would stack
on top of a harness that is already rewriting its own history — every
Hermes compaction is a prefix change that invalidates frozen fingerprints
and re-curates from scratch. Not fatal (Claude Code `/compact` is the same
event, just rarer), but it caps realized savings and muddies attribution
of "who saved what". Hermes also does context-length detection via model
metadata endpoints, so parsec must relay those cleanly. `(inference)`

### 3.3 Measurement

- Chat Completions path (Nous Portal, OpenRouter, self-hosted, etc.):
  **no counterfactual** — same null posture as the OpenAI wire.
- Anthropic path (Claude Max / API key): full `count_tokens`
  counterfactual — the only Hermes slice with honest savings numbers.

### 3.4 Effort estimate & recommended posture

- **Cheap entry `(inference)`:** Anthropic-wire-only — `setup_hermes.rs`
  writing the (to-be-verified) Anthropic base-URL override, reusing
  `/v1/messages` end-to-end. Roughly opencode-scale, YAML merge instead
  of a plugin drop.
- **Full support:** requires the Chat Completions adapter (§4), the
  custom-provider YAML block, key-forwarding design around the
  hostname-matching trap, and a story for compression stacking. Codex-
  scale or above.
- Hermes can also drive Claude Code / Codex / OpenCode *as sub-agents* —
  those sessions already flow through parsec's existing integrations, so
  some Hermes traffic is covered today for free. `(doc)`

### 3.5 Open questions

- Anthropic base-URL config key path (blocker for the cheap entry).
- Whether Hermes's per-request bodies are stable enough across its own
  compaction for fold-back replay to ever hit.
- Attribution: does the custom-provider config support extra headers? If
  not, infer tool from a dedicated route namespace (as `/chatgpt` does).

---

## 4. Shared dependency: the Chat Completions adapter

Hermes's canonical wire and Grok Build's default backend are both OpenAI
Chat Completions. One adapter (`chat_completions.rs`: `to_internal` +
`apply_curation` + relay + routes) unlocks both, plus a long tail of
OpenAI-compatible harnesses. Costs to expect, from the codex experience:
~1,300–2,000 LOC of adapter+tests, and the same fold-back invariants
(`internal content == wire_text(item)` exactly; positional fold map;
unknown parts project as `""` and serve verbatim). Measurement stays null
on this wire — the §3.2 survey gap (local tokenizer vs billed-delta) is
still open and now blocks two harnesses instead of zero.

## 5. Recommended sequencing

1. **pi** — config-only interposition, existing wire, full measurement,
   sub-opencode effort. Do first; it also pressure-tests the
   "single static binary for non-Claude-Code harnesses" story
   (DIRECTION §7b) with zero shim code.
2. **Grok Build BYOK** — run the `grok inspect` / dialect verification
   spike; if Responses-compatible, ship on the existing adapter with a
   `/xai` namespace. Decide the tokenize-text counterfactual question
   explicitly; default null.
3. **Chat Completions adapter** — build once Grok (if needed) or Hermes
   justifies it.
4. **Hermes** — verify the Anthropic override key for a cheap Claude-Max
   entry; hold full support until the adapter exists and the
   compression-stacking behavior is observed in practice.

Per the codex retro, every one of these needs a pinned-version CI smoke
test — all three tools ship fast and two (pi, Hermes) hot-reload or
auto-update config surfaces.
