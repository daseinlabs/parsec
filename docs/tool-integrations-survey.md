# Multi-tool integration survey — Cursor, Codex CLI, opencode, Gemini CLI, Cline, Windsurf

**Status:** research/design note, 2026-07-28. No implementation. Question: how hard is it to
bring parsec (local curating proxy + free-tier UX plugin) to agents other than Claude Code?
Findings from official docs + source inspection (openai/codex and anomalyco/opencode read at
HEAD). Claims below marked *(doc)*, *(src)*, or *(inference)* where it matters.

---

## 1. TL;DR difficulty ranking

| Tool | Data-plane interception (local proxy) | Free-tier UX (agents/hooks/status) | Wire adapter needed | Verdict |
|---|---|---|---|---|
| **opencode** | **Easy** — `provider.anthropic.options.baseURL` in `opencode.json` | **Easy** — md agents/commands, npm plugin with request-mutating hooks; no status line (toast instead) | none (Anthropic Messages) | **First port** |
| **Cline** | **Easy** — native Anthropic provider has a custom-base-URL checkbox | Easy-Medium — hooks (v3.36+, not Windows), rules, workflows, MCP; no status line | none (Anthropic Messages) | Cheap second target |
| **Codex CLI** | **Easy** (API key, `model_providers` table) / **Medium** (ChatGPT subscription — mechanically works, see §3) | Medium — hooks near CC-parity; status line is a closed item catalog | **OpenAI Responses API** (chat wire removed from Codex) | Main new-engine work |
| **Gemini CLI** | Easy-Medium — `GOOGLE_GEMINI_BASE_URL` (API key), `CODE_ASSIST_ENDPOINT` (OAuth tier, documented) | **Easy** — first-class extensions bundle hooks/commands/MCP/subagents/skills | Gemini `generateContent` (+ Code Assist `v1internal` wrapper for OAuth tier) | Worth it after Responses |
| **Cursor** | **Impossible locally** — all model traffic transits Cursor's servers for "final prompt building", even BYOK; localhost unreachable | Easy-Medium — hooks, subagents, skills, MCP, VS Code ext for status bar | Cursor's OpenAI-ish dialect (OpenRouter needed a dedicated normalizing endpoint) | Free-tier UX only; proxy only viable as hosted Team gateway |
| **Windsurf** | Hard-Impossible — subscription via their servers; BYOK exists but no endpoint override field | Medium-Hard — MCP + rules only; no hooks, no packaging | n/a | Skip (MCP presence at most) |
| **Devin** | **Impossible** — fully cloud-hosted, no BYO key/endpoint | n/a (Knowledge/Playbooks/MCP only, no model path) | n/a | Skip |

The two structural findings:

1. **The market converged on Claude Code's UX surfaces.** Cursor, Codex, Gemini CLI, Cline and
   opencode all now ship hooks (PreToolUse-style, deny/rewrite/inject), markdown custom
   agents, and skills/commands. The free tier ports almost everywhere; only the *status line*
   is Claude-Code-unique (Codex: fixed item catalog *(src)*; opencode: none; Cline: none).
2. **The data plane splits on one question: does the vendor middleman model traffic?**
   Cursor and Windsurf do (uninterceptable locally, by architecture). Codex, opencode,
   Gemini CLI, Cline are stateless clients that send full history to a configurable base URL —
   exactly the Claude Code shape.

## 2. Per-tool detail

### opencode (opencode.ai, github.com/anomalyco/opencode)

- Interception *(doc/src)*: any provider accepts `options.baseURL`; AI SDK
  `@ai-sdk/anthropic` speaks Anthropic Messages with `cache_control` breakpoints placed like
  Claude Code (first 2 system + last 2 non-system messages, `provider/transform.ts`
  `applyCaching`). Full history rebuilt per turn — stateless. No `ANTHROPIC_BASE_URL` env
  support; config edit required.
- Subscription: Claude Pro/Max OAuth **removed from core in v1.3.0** — docs say "Anthropic
  explicitly prohibits this". Only community plugins restore it. **opencode subscription
  users are out of scope** (consistent with our OAuth rule). API-key (BYOK) users only.
- Plugins *(src, `packages/plugin/src/index.ts`)*: npm-distributed, one-line install
  (`"plugin": ["parsec-opencode"]`). Hooks include `chat.params`, `chat.headers`,
  `tool.definition` (tool-schema pruning!), `tool.execute.before/after`, and
  `experimental.chat.messages.transform` — full mutable message history before every model
  call. In principle the curation could run **in-process with no proxy**; rejected as the
  primary path (experimental API, TS runtime vs. our no-TS-client rule, and the proxy is our
  determinism boundary). Use a **thin plugin** for install/UX; keep curation in the proxy.
- UX: markdown agents + commands near-identical to ours; themes but no status line —
  `tui.toast.show` is the savings-display substitute.

### Cline (cline.bot)

- Interception *(doc)*: native **Anthropic provider with "Use custom base URL"** — parsec
  drops in with zero protocol translation. Also a generic OpenAI-Compatible provider with
  free-form base URL. (Known regression hid the base-URL field on the plain OpenAI provider,
  issue #7128 — steer users to Anthropic or OpenAI-Compatible.)
- Fully BYOK by design; no vendor middleman.
- UX: hooks (v3.36+, `~/Documents/Cline/Rules/Hooks/` or `.clinerules/hooks/`, macOS/Linux
  only), `.clinerules`, workflows (slash-command style), MCP marketplace. No status line.

### Codex CLI (github.com/openai/codex)

Deep dive with per-claim citations: `docs/codex-cli-integration-research.md`.

- Interception, API key *(doc)*: first-class `[model_providers.parsec]` in
  `~/.codex/config.toml` with `base_url`, `env_key`, `wire_api = "responses"`; select via a
  profile (`codex --profile parsec`). Provider/auth config is **user-level only** — project
  config cannot set base URLs (by design, security).
- Interception, ChatGPT subscription *(src)*: subscription traffic goes to
  `https://chatgpt.com/backend-api/codex`, but a configured `base_url` / `openai_base_url`
  **overrides even in ChatGPT-auth mode** (`model-provider-info/src/lib.rs`
  `to_api_provider()`: default applied only when `base_url` is `None`). OAuth Bearer +
  `chatgpt-account-id` header pass through a localhost hop — tokens never leave the machine,
  the pxpipe posture. *Caveats:* undocumented-as-proxying, ToS posture unassessed; proxy must
  also pass through `responses/compact` (server-side compaction!), `models`, `search`,
  `session` endpoints; built-in provider tries **Responses-over-WebSocket** first (proxy must
  reject the WS upgrade → client falls back to HTTP SSE; custom providers are HTTP-only).
- Wire *(src)*: **Responses API only** — `wire_api = "chat"` removed (deser error points to
  discussion #7782). HTTP is stateless: `store=false`, full `input` list each turn,
  `include: ["reasoning.encrypted_content"]` — encrypted reasoning items ride in-band and
  must be preserved or carefully dropped by any history rewriter. `previous_response_id` is
  WebSocket-transport-only.
- Caching *(doc)*: automatic prefix caching ≥1024 tokens, no client breakpoints;
  `prompt_cache_key` sent per session (must be preserved). Cache writes billed 1.25× on
  GPT-5.6+. Deterministic byte-stable prefixes map cleanly; no splice markers to manage.
- UX: hooks shipped and stable-on (11 events incl. PreToolUse deny/`updatedInput`,
  UserPromptSubmit, `additionalContext` injection); skills/prompts; MCP; `[agents]` roles;
  `notify` command. **Status line is a fixed item catalog — no custom slot** *(src)*; savings
  display must fall back to a `/skill` on demand or `notify`. Fun fact *(src)*: Codex ships a
  migrator for Claude Code hook configs (`external-agent-migration/hooks_cla.rs`).

### Gemini CLI (github.com/google-gemini/gemini-cli)

- Interception: API-key path honors `GOOGLE_GEMINI_BASE_URL` (SDK env var; semi-documented,
  version-sensitive — PR #6380 closed, follow-ups tracked in #6746). OAuth "Login with
  Google" tier hits Code Assist `cloudcode-pa.googleapis.com` `v1internal:generateContent`,
  overridable via the **documented** `CODE_ASSIST_ENDPOINT` env var. Env vars may not
  propagate into sandbox mode (issue #2168).
- Wire: Gemini `generateContent`/`streamGenerateContent`; OAuth tier wraps it in a thin Code
  Assist envelope *(inference on exact shape)*. New adapter required.
- UX: **extensions** are the closest thing to Claude Code plugins anywhere — one
  `gemini-extension.json` bundles MCP servers, TOML commands, hooks, subagents, skills,
  themes.

### Cursor

- **All requests are routed through Cursor's servers for "final prompt building"** — official
  docs, verbatim, including BYOK mode (the API key is sent to their backend with every
  request). BYOK base-URL override exists **only for OpenAI-key mode**, and the request to
  that URL originates from **Cursor's backend** → localhost proxies are unreachable; people
  use ngrok tunnels. Corroborated by users with mitmproxy: with a localhost base-URL override
  set, traffic still goes to `api2.cursor.sh` (forum.cursor.com t/3975). The full prompt
  never exists client-side. Tab/Composer stay on
  Cursor-hosted models regardless.
- Consequences: an individual-tier Cursor integration violates "data plane local" twice over
  (code transits Cursor's cloud; our endpoint must be public). The only coherent offering is
  the **hosted Team gateway** (§3), plus dialect work: Cursor's egress is OpenAI-ish but
  non-standard — OpenRouter had to stand up a dedicated `/api/v1/cursor` normalizing endpoint.
- UX-only free tier is viable and cheap: `.cursor/hooks.json` (preToolUse can deny/modify/
  inject — direct no-reread analog), `.cursor/agents/*.md` subagents, skills/commands, MCP,
  and a small Open VSX extension for a status-bar savings item.

### Windsurf

- Subscription traffic via Windsurf inference infra; BYOK (Anthropic keys) exists but has
  **no base-URL field**. No hooks, no plugin packaging; MCP + rules only. Not a target.

### Devin (Cognition)

- Fully cloud-hosted agent: the loop and all model calls run inside Cognition's cloud
  (docs.devin.ai/enterprise/deployment/overview); no BYO API key or custom model endpoint
  exists. Nothing to intercept locally — **Impossible**, not a target. (Extension surfaces —
  Knowledge, Playbooks, MCP — give no access to model traffic.)

## 3. Proposed system

### 3.1 One proxy, N wire adapters (the engine stays put)

The engine already operates on a normalized message layer (`engine/src/messages.rs`) and the
proxy has an upstream-provider seam (`proxy/src/providers.rs`). The port surface is a
**wire-format adapter** per protocol, not per tool:

```
adapter = parse(wire body) → normalized steps/chunks → curate → serialize(wire body)
          + caching model (splice rules) + auth passthrough + endpoint passthrough list
```

| Adapter | Serves | Caching model | Status |
|---|---|---|---|
| Anthropic Messages | Claude Code, opencode, Cline | explicit `cache_control` breakpoints | exists |
| OpenAI Responses | Codex CLI (+ opencode OpenAI models) | automatic prefix + `prompt_cache_key` | new — largest piece |
| OpenAI chat completions | Cline OpenAI-compat, misc gateways | automatic prefix | new — small, low priority |
| Gemini generateContent (+ Code Assist envelope) | Gemini CLI | implicit caching | new |

Adapter-specific hazards to design for now:
- **Responses API**: preserve `reasoning.encrypted_content` items and `prompt_cache_key`
  verbatim; pass through `responses/compact`, `models`, `search`, `session` untouched;
  reject WebSocket upgrades so the client falls back to SSE. Decide policy for server-side
  compaction (`responses/compact`) vs our own curation — probably disable/intercept, else
  the two compactors fight.
- **Parity rule extends per-wire**: byte-stable serialization per format is the new port
  definition-of-done (serde_json `preserve_order` equivalent discipline in every adapter).

### 3.2 Measurement honesty has a gap on OpenAI

Anthropic gives us the free `count_tokens` counterfactual; **OpenAI has no count-tokens
endpoint**. Options, in decreasing honesty: (a) local tokenizer (tiktoken/harmony) on the
original vs. curated body — deterministic but approximates OpenAI's internal serialization of
tools/reasoning; (b) report only *billed* deltas across turns (no counterfactual); (c) modeled
baseline — forbidden by our own rules. Gemini has a free `countTokens` endpoint → fine.
**Open decision:** whether (a) is honest enough to print, and how to label it.

### 3.3 Subscription-auth policy per tool

- Claude Code: unchanged (local rewrite of user's own traffic, pxpipe posture).
- Codex ChatGPT auth: mechanically proxyable with tokens staying local *(src-verified)* —
  same posture as Claude Code subscriptions, but OpenAI's ToS stance is unassessed.
  **Gate behind an explicit flag + ToS review; ship BYOK first.**
- opencode + Claude subscription: prohibited per opencode's own docs — do not support.
- Gemini OAuth free tier: `CODE_ASSIST_ENDPOINT` is documented for redirection; tokens stay
  local. Acceptable posture, version-sensitive.
- Cursor/Windsurf subscriptions: architecturally out of reach; don't try.

### 3.4 Free-tier UX ports (no proxy required)

Highest-leverage, lowest-cost distribution — same wedge as the Claude Code free tier:

| Surface | opencode | Cline | Codex | Gemini CLI | Cursor |
|---|---|---|---|---|---|
| explore/map agent | `.opencode/agents/*.md` | (rules/workflows) | `[agents]` roles | extension subagent | `.cursor/agents/*.md` |
| no-reread hook | plugin `tool.execute.before` | `.clinerules/hooks/` | `PreToolUse` hooks.json | extension hooks | `.cursor/hooks.json` preToolUse |
| savings display | toast | — | skill/notify (no statusline slot) | extension theme/command | VS Code ext status bar |
| slash commands | `.opencode/commands/` | workflows | skills/prompts | extension TOML commands | skills |
| packaging | **npm plugin (1 line)** | file drop | user config.toml + profile | **gemini-extension.json** | file drop + Open VSX ext |

TS-rule note: opencode plugins and a Cursor VS Code extension are unavoidably JS/TS. Keep
them **thin shims** (config management, toast/status display, exec `parsec` binary for
anything real) — same exemption logic as the web dashboard, to be ratified in DIRECTION.md.

### 3.5 Install story: `parsec setup <tool>`

One Rust installer subcommand per tool, all non-destructive:
- `opencode`: add `"plugin": ["parsec-opencode"]`; plugin manages `options.baseURL` itself.
- `cline`: instruct/automate provider base-URL setting; drop `.clinerules/hooks/`.
- `codex`: append `[model_providers.parsec]` via a **format-preserving TOML editor** + write
  standalone profile `~/.codex/parsec.config.toml` (zero base-config mutation beyond the
  table; opt-in per invocation via `codex --profile parsec`). Must be user-level config.
- `gemini`: install a gemini-extension + set env vars in launcher wrapper (sandbox caveat).
- `cursor`: free-tier files only (`.cursor/hooks.json`, agents, skills) + optional extension.
The proxy identifies the calling tool (distinct localhost ports or a header set at install)
so the ledger can attribute savings per tool.

### 3.6 Phasing

1. **Phase 1 — prove multi-tool with zero engine work:** opencode + Cline (Anthropic wire
   reuse). Deliverables: `parsec setup opencode|cline`, `parsec-opencode` npm shim, per-tool
   ledger attribution. Risk: low.
2. **Phase 2 — Responses adapter → Codex CLI (BYOK):** the real build. Includes the
   tokenizer-based measurement decision (§3.2) and `responses/compact` policy. ChatGPT-auth
   flag only after ToS review.
3. **Phase 3 — Gemini adapter → Gemini CLI extension** (API-key first; OAuth tier via
   `CODE_ASSIST_ENDPOINT` second).
4. **Parallel, anytime — free-tier UX drops** for Cursor and Codex (no proxy dependency):
   pure distribution.
5. **Team tier only — Cursor gateway:** hosted BYOK endpoint speaking Cursor's dialect
   (OpenRouter-style normalization). Only if Team demand materializes; never an
   individual-tier product.

### 3.7 Open questions

1. OpenAI savings counterfactual: is local tokenization honest enough to print? (§3.2)
2. Codex ChatGPT-auth proxying: ToS review before any flag ships.
3. `responses/compact`: intercept, disable, or coexist with our curation?
4. Ratify the thin-TS-shim exemption (opencode plugin, Cursor extension) in DIRECTION.md.
5. Gemini env-var stability: pin tested CLI versions or upstream a real config knob.
