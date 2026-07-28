# Codex CLI integration research (for parsec)

Research date: 2026-07-28. How parsec — a local HTTP proxy between a coding agent
and the model API — could integrate with OpenAI Codex CLI (github.com/openai/codex)
and, secondarily, the Codex IDE extension/cloud.

Sources: official docs at developers.openai.com/codex (currently 308-redirects to
learn.chatgpt.com/docs/*; the developers.openai.com URLs are cited as canonical),
plus the openai/codex source at HEAD (paths below are within `codex-rs/`).
Each claim is tagged **[DOC]** (documented), **[SRC]** (verified in shipped
source), or **[INF]** (inference).

## 1. Traffic interception

**Custom provider** — **[DOC]** `~/.codex/config.toml`:

```toml
[model_providers.proxy]
name = "OpenAI using LLM proxy"
base_url = "http://proxy.example.com"
env_key = "OPENAI_API_KEY"
wire_api = "responses"
```

selected via `model_provider = "proxy"` (or a profile). Fields available
**[SRC** `model-provider-info/src/lib.rs`**]**: `base_url`, `env_key`,
`env_key_instructions`, `experimental_bearer_token`, `auth = {command=...}`
(command-backed token), `aws` (SigV4), `wire_api`, `query_params`,
`http_headers`, `env_http_headers`, `request_max_retries`, `stream_max_retries`,
`stream_idle_timeout_ms`, `requires_openai_auth`, `supports_websockets`.
Reserved IDs: `openai`, `ollama`, `lmstudio`, `amazon-bedrock`.
(https://developers.openai.com/codex/config-advanced,
https://developers.openai.com/codex/config-reference)

**Built-in provider override** — **[DOC]** `openai_base_url =
"https://us.api.openai.com/v1"` in user config overrides the built-in `openai`
provider's base URL without defining a new provider (documented for data
residency / "proxy routing"). (config-advanced)

**`OPENAI_BASE_URL` env var: effectively gone.** **[SRC]** No code path reads
`OPENAI_BASE_URL` anymore; the only occurrences are TUI display constants
(`tui/src/chatwidget.rs`). The override is the `openai_base_url` config key
(plumbed at `core/src/config/mod.rs:3653` into `built_in_model_providers`).
Third-party blogs still describe the env var — treat as stale.

**ChatGPT-subscription auth — the critical finding:**

- **[SRC]** ChatGPT-auth model traffic goes to
  `pub const CHATGPT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex"`
  (`model-provider-info/src/lib.rs:38`), NOT api.openai.com.
- **[SRC]** **A configured `base_url` overrides even in ChatGPT auth mode.** In
  `to_api_provider()` (lib.rs:244–262), the chatgpt-vs-api.openai.com default is
  applied only when `self.base_url` is `None`:
  `let base_url = self.base_url.clone().unwrap_or_else(|| default_base_url.to_string())`.
  So `openai_base_url = "http://127.0.0.1:PORT"` reroutes subscription traffic
  (Bearer ChatGPT OAuth token + `chatgpt-account-id` header attached) to a local
  proxy, which forwards to `chatgpt.com/backend-api/codex/responses`.
- **[SRC]** Alternatively a custom provider with `requires_openai_auth = true` +
  `base_url` uses the ChatGPT/API-key login from `auth.json` ("If true, user is
  presented with login screen... login preference and token/key are stored in
  auth.json" — field doc comment; also listed in the config reference **[DOC]**).
- **[INF]** Docs never explicitly bless proxying subscription traffic; the
  mechanism exists and is stable-looking config surface, but it is not a
  supported/contractual integration point, and ToS posture is unverified.
  Tokens stay on the user's machine (localhost hop), which matches parsec's
  "subscription OAuth tokens never through our cloud" rule.
- **[DOC/SRC]** Separate `chatgpt_base_url` overrides non-model backend-api
  calls (login flow, connectors, file endpoints) — user-level config only;
  **project-level config cannot override provider auth or base URLs** ("for
  security", config-advanced; enforced in
  `core/src/config/config_loader_tests.rs`, which treats `chatgpt_base_url` as
  an attack vector from project config).

**HTTPS_PROXY** — **[DOC]** "Standard environment variables HTTP_PROXY and
HTTPS_PROXY apply to Codex network requests" (config-advanced). **[INF]**
Useless for content rewriting: HTTPS goes through as a CONNECT tunnel;
rewriting would require MITM cert installation. The in-repo `network-proxy`
crate (with `mitm_hook`, `network_policy`) is for sandboxed-command egress
control, not the model path.

**WebSocket caveat** — **[SRC]** The built-in `openai` provider has
`supports_websockets: true`; Codex first tries Responses-over-WebSocket at
`{base_url}/responses` (ws), with a session-scoped fallback to HTTP SSE on
failure (`core/src/client.rs` `responses_websocket_enabled`,
`force_http_fallback` → "falling back to HTTP"). Custom providers default
`supports_websockets: false` → plain HTTP streaming. So: parsec custom provider
= HTTP only (good); overriding the built-in provider via `openai_base_url` =
proxy should reject the WS upgrade (client falls back) or speak WS.

## 2. Wire protocol

- **[SRC/DOC]** **Responses API only.** `wire_api = "chat"` was **removed**:
  the deserializer now errors with "`wire_api = "chat"` is no longer
  supported... set `wire_api = "responses"`...
  https://github.com/openai/codex/discussions/7782"
  (`model-provider-info/src/lib.rs:50-84`). A parsec Codex adapter must
  parse/rewrite the Responses API, not chat completions.
- **[SRC]** **Stateless over HTTP: full conversation each turn.** Request built
  in `core/src/client.rs` (~line 905):
  `store: provider.is_azure_responses_endpoint()` → `store=false` for
  OpenAI/ChatGPT; full `input` item list sent every turn;
  `include: ["reasoning.encrypted_content"]` — reasoning items travel in-band
  as encrypted content and must be preserved (or carefully dropped) by any
  history-rewriting proxy.
- **[SRC]** `previous_response_id` is used **only** on the WebSocket transport
  (`codex-api/src/endpoint/responses_websocket.rs`), where Codex sends deltas
  when request properties (model, instructions, tools, prompt_cache_key, etc.)
  match (`responses_request_properties_match`, client.rs:307). Over HTTP this
  is not used → a proxy on a custom provider always sees the whole transcript.
- **[SRC]** Other endpoints hit under the same base_url that a proxy must pass
  through: `responses/compact` (server-side compaction —
  `supports_remote_compaction()` is true for the openai provider), `models`,
  `search` (standalone web search), `session`, `images`, `memories`
  (`codex-api/src/endpoint/`).

## 3. Prompt caching

**[DOC]** (https://developers.openai.com/api/docs/guides/prompt-caching,
formerly platform.openai.com/docs/guides/prompt-caching)

- Automatic prefix caching for prompts ≥1024 tokens; no client-side
  cache_control required; manual clearing unavailable.
- `prompt_cache_key` routes requests with shared prefixes to the same cache
  (keep ≈15 req/min per key); "required for reliable matching" on GPT-5.6
  models.
- GPT-5.6+: cache writes billed at 1.25× uncached input (`cache_write_tokens`);
  cached prefix retained ≥30 min. Older models: 5–10 min inactivity, max 1 h
  (extended up to 24 h on supported models). Cache reads discounted (doc says
  discounted; exact read multiplier not captured).
- Responses API additionally supports explicit cache breakpoints on
  `input_text`/`input_image`/`input_file` blocks.
- **[SRC]** Codex sends `prompt_cache_key` automatically per session
  (client.rs `prompt_cache_key()`, override hook exists). **[INF]** Parsec's
  deterministic-splice requirement maps cleanly: keep the rewritten prefix
  byte-stable across turns and preserve the `prompt_cache_key`.

## 4. Extensibility surfaces

- **Hooks — Claude-Code-style, shipped and on by default.** **[DOC]**
  (https://developers.openai.com/codex/hooks) + **[SRC]** (`hooks/src/lib.rs`):
  11 events — `PreToolUse`, `PermissionRequest`, `PostToolUse`, `PreCompact`,
  `PostCompact`, `SessionStart`, `SessionEnd`, `UserPromptSubmit`,
  `SubagentStart`, `SubagentStop`, `Stop`. Config in `~/.codex/hooks.json`,
  `~/.codex/config.toml` `[[hooks.PreToolUse]]` tables, or project `.codex/`
  (trusted repos only). PreToolUse can deny (`"permissionDecision": "deny"`)
  or allow with `updatedInput`; UserPromptSubmit can block; hooks can inject
  `additionalContext`; exit 0 + JSON / exit 2 = failure-with-feedback.
  Feature flag: **[SRC]** `Feature::CodexHooks, key: "hooks", Stage::Stable,
  default_enabled: true` (`features/src/lib.rs:1012`) — note one docs page
  still says "off by default"; source says on. Bonus **[SRC]**:
  `external-agent-migration/src/hooks_cla.rs` — Codex ships a migrator for
  Claude Code hook configs.
- **Custom prompts / slash commands** — **[DOC]** `~/.codex/prompts/` markdown
  with YAML frontmatter (`description`, `argument-hint`),
  `$1..$9`/`$ARGUMENTS`/named placeholders, invoked as `/prompts:name`;
  **deprecated in favor of skills**
  (https://developers.openai.com/codex/custom-prompts,
  https://developers.openai.com/codex/skills).
- **AGENTS.md** — **[DOC]** supported project instruction file
  (https://developers.openai.com/codex/guides/agents-md).
- **Custom agents** — **[SRC]** `[agents]` roles loaded per config layer
  (`core/src/config/agent_roles.rs`), i.e., custom subagent roles exist.
- **MCP** — **[DOC]** (https://developers.openai.com/codex/mcp)
  `[mcp_servers.<id>]` stdio (`command`/`args`/`env`/`env_vars`/`cwd`) and
  streamable HTTP (`url`, `bearer_token_env_var`, `http_headers`, OAuth via
  `codex mcp login`); `codex mcp add name -- cmd`; per-server/per-tool
  approval modes (`default_tools_approval_mode` = auto|prompt|writes|approve,
  `enabled_tools`/`disabled_tools`).
- **notify** — **[DOC]** `notify = ["python3", "/path/notify.py"]`; receives
  JSON (`type`, `thread-id`, `turn-id`, message) on agent events
  (config-advanced).
- **Status line: exists but closed.** **[SRC]** `[tui] status_line = [...]`
  with a **fixed catalog** of items
  (`tui/src/bottom_pane/status_line_setup.rs`): `model`,
  `model-with-reasoning`, `current-dir`, `git-branch`, `context-remaining`,
  `context-used`, `five-hour-limit`, `weekly-limit`, `used-tokens`,
  `total-input-tokens`, `total-output-tokens`, `task-progress`, etc.
  **No custom-command/script item** — a parsec "savings" display cannot be
  injected into the Codex TUI. Nearest workaround **[INF]**: `notify`/hooks
  driving OS notifications, or a `/prompts:`-style skill that reports savings
  on demand.
- **Scripting** — **[DOC]** `codex exec "prompt"` (final message → stdout,
  progress → stderr), `--json` (JSONL events), `--output-schema`, `-o`,
  `codex exec resume --last`, `--ephemeral`, `CODEX_API_KEY` for CI
  (https://developers.openai.com/codex/noninteractive).

## 5. Config / install story

- **[DOC]** Layers, highest→lowest: CLI `-c` overrides → project
  `.codex/config.toml` (trusted repos only; **cannot** set providers/base
  URLs/auth) → profile (`$CODEX_HOME/<name>.config.toml`,
  `codex --profile <name>`) → user `~/.codex/config.toml` → system
  (`/etc/codex`) / managed `requirements.toml` → defaults
  (https://developers.openai.com/codex/config-basic). `auth.json` in
  `~/.codex` holds tokens ("treat like a password").
- **[INF]** Non-destructive install: append a `[model_providers.parsec]` table
  to the user `config.toml` (TOML-aware, format-preserving edit) **plus** drop
  a standalone profile file `~/.codex/parsec.config.toml` containing
  `model_provider = "parsec"` — profile-as-file means zero mutation of the
  user's base config beyond the provider table; users opt in per-invocation
  with `codex --profile parsec`. For subscription users, the single line
  `openai_base_url = "http://127.0.0.1:PORT"` is the only base-config mutation
  needed. Provider config must be user-level (project-level is blocked by
  design).

Example installer output:

```toml
# appended to ~/.codex/config.toml
[model_providers.parsec]
name = "parsec (context compression proxy)"
base_url = "http://127.0.0.1:8377/v1"
env_key = "OPENAI_API_KEY"        # BYOK path
wire_api = "responses"

# ~/.codex/parsec.config.toml  (profile: codex --profile parsec)
model_provider = "parsec"
```

## Integration feasibility summary

| Path | Rating | Reason |
|---|---|---|
| (a) Local proxy interception, API-key users | **Easy** | First-class documented `model_providers` + `base_url`/`env_key`; custom providers are HTTP-SSE with full history each turn, `store=false`. |
| (b) Local proxy interception, ChatGPT-subscription users | **Medium** | Mechanically works today (`openai_base_url` or `requires_openai_auth=true` provider override even in ChatGPT auth mode — source-verified, tokens stay local), but undocumented-as-proxying, must pass through extra backend endpoints (`responses/compact`, `models`, `search`) and handle/deflect the default WebSocket transport; ToS risk unassessed. |
| (c) Free-tier-style UX features | **Medium** | Hooks are near-Claude-Code parity (PreToolUse deny/rewrite, UserPromptSubmit, SessionStart), plus prompts/skills, MCP, agent roles, `notify` — but the status line is a closed item catalog with no custom-command slot, so no persistent savings display. |
| (d) Install story | **Easy** | Single well-known user config file + standalone profile files; append one TOML table + one profile file; only constraint is providers must live in user-level (not project) config. |

Hard blocker for none; the one genuinely impossible piece today is a custom TUI
status-line segment.

## Citations

- https://developers.openai.com/codex/config-basic
- https://developers.openai.com/codex/config-advanced
- https://developers.openai.com/codex/config-reference
- https://developers.openai.com/codex/auth
- https://developers.openai.com/codex/hooks
- https://developers.openai.com/codex/custom-prompts
- https://developers.openai.com/codex/skills
- https://developers.openai.com/codex/guides/agents-md
- https://developers.openai.com/codex/mcp
- https://developers.openai.com/codex/noninteractive
- https://developers.openai.com/api/docs/guides/prompt-caching
- https://github.com/openai/codex/discussions/7782 (chat wire API removal)
- Source (openai/codex @ HEAD, 2026-07-28): `codex-rs/model-provider-info/src/lib.rs`,
  `codex-rs/core/src/client.rs`, `codex-rs/core/src/config/mod.rs`,
  `codex-rs/hooks/src/lib.rs`, `codex-rs/features/src/lib.rs`,
  `codex-rs/tui/src/bottom_pane/status_line_setup.rs`,
  `codex-rs/codex-api/src/endpoint/`
