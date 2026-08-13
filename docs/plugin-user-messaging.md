# Plugin UX — user messaging and install flow

Part 1: where the Claude Code plugin can show the user information like "you
saved N tokens", ranked by fit. Part 2: how users discover, install, and
configure the plugin, and what we must publish. Source:
https://code.claude.com/docs (plugins, plugin-marketplaces, plugins-reference,
discover-plugins, hooks, statusline, skills, settings, MCP references),
surveyed 2026-07-15.

# Part 1 — User messaging surfaces

## High-value surfaces

### 1. Statusline — ambient savings ticker (best passive signal)

The persistent footer row of the Claude Code TUI, refreshed after every
assistant message (plus optional polling via `refreshInterval`).

- Plugins canNOT ship a `statusLine` key (verified 2026-07-18: plugin
  `settings.json` supports only `agent` and `subagentStatusLine`). Implemented
  delivery: `parsec setup` merges a managed `statusLine` entry into the
  user's `~/.claude/settings.json` — additive with the same ownership rules
  as the env merge (a user-authored statusLine is never overwritten; ours is
  repointed on version bumps by the SessionStart re-assert). Claude Code
  pipes session JSON to the command on stdin (`model.display_name`,
  `cost.total_cost_usd`, `context_window.used_percentage`, `session_id`,
  rate limits, ~30 fields) and renders whatever it prints. ANSI colors and
  OSC 8 hyperlinks work.
- Claude Code does **not** pass plugin data in — the script must read savings
  from local state. Implemented bridge: the proxy stamps each savings-ledger
  row with the optional `session_id` it extracts from the request's
  `metadata.user_id` (a JSON-encoded object carrying the Claude Code session
  uuid — verified live against CC 2.1.214); the statusline filters
  `~/.parsec/ledger.jsonl` to the current session. If the ledger outgrows the
  render budget, switch to a pre-aggregated per-session state file.
- Example: `parsec ▸ saved 12.5k tokens ($0.06) this session`
- Caveat: runs on every refresh — keep the read path cheap (pre-aggregated
  file, never a live computation).

Ref: https://code.claude.com/docs/en/statusline.md

### 2. On-demand skill: `/parsec:savings` — the detailed report

`skills/savings/SKILL.md` with:

- `disable-model-invocation: true` — only the user triggers it.
- A `` !`${CLAUDE_PLUGIN_ROOT}/bin/parsec savings` `` shell injection that
  pulls the numbers from the binary.

This is where the rich report lives: session vs. month-to-date, compression
ratio, and the methodology. Good place to make **measurement honesty**
user-visible — state that savings come from the per-request `count_tokens`
counterfactual, not a modeled baseline.

Ref: https://code.claude.com/docs/en/skills.md

### 3. SessionStart hook — one-time awareness message

Hooks can return `systemMessage`, which renders as a prominent notice bar
directly to the user (distinct from `additionalContext`, which is injected
into Claude's context instead). A SessionStart hook with matcher `startup`:

> parsec active — cumulative savings: 1.2M tokens. `/parsec:savings` for details.

Once per session; not naggy.

Ref: https://code.claude.com/docs/en/hooks.md

### 4. Stop / SessionEnd hooks — per-turn and rollup

- **Stop**: can emit `systemMessage` after Claude finishes a response
  ("this turn: −3,400 tokens"). Optional; may be noisy — consider off by
  default.
- **SessionEnd**: right place to roll session savings into the monthly
  aggregate even if nothing is displayed.

## Supporting surfaces

| Surface | Verdict |
|---|---|
| MCP tool (`get_savings`) | Do it — server already exists; lets Claude itself answer "how much have I saved?" in conversation, result shows in transcript. |
| Manifest + README | Do it — install-time trust story in `/plugin` manager and marketplace. `description` should say "measured, not modeled — per-request count_tokens counterfactual". |
| MessageDisplay hook | Skip — can append a badge to every response but runs constantly and feels noisy. |
| Monitors (`monitors/monitors.json`) | Skip for savings — stdout lines become notifications *to Claude* (prompts a reaction), wrong shape for informing the user. Possible fit for alerting (e.g., proxy fell back to passthrough). |
| MCP resources / prompts, custom agents | Technically visible but awkward for this use case. |

## Hook output rendering cheat sheet

| Hook output field | Renders as | Directly user-visible? |
|---|---|---|
| `systemMessage` | Prominent notice bar | Yes |
| `additionalContext` | Injected into Claude's context | Only if Claude repeats it |
| `updatedToolOutput` (PostToolUse) | Replaces tool result in transcript | Yes |
| `displayContent` (MessageDisplay) | Replaces on-screen text | Yes |
| `terminalSequence` | Escape sequences (bell, title, desktop notification) | System-level |

## Recommended combination

1. **Statusline** — ambient, every turn.
2. **`/parsec:savings` skill** — detailed on-demand report + methodology.
3. **SessionStart `systemMessage`** — one-time cumulative stat + pointer to
   the skill (discoverability).
4. **README/manifest** — install-time trust story.
5. **Brand surfaces** (§5 below) — agent name, subagent line, spinner verbs.

All four read the same local state file written by the Rust proxy. This keeps
the **data-plane-local** invariant: savings numbers never need to leave the
machine to be displayed. Savings figures shown anywhere must come from the
per-request `count_tokens` counterfactual (measurement honesty — see
CLAUDE.md / DIRECTION.md).

## 5. Brand surfaces — where the NAME appears

Everything above answers "where do we show a savings number?". A separate
question is "where does the user see that parsec is here at all?", and it has
different answers. Surveyed against CC 2.1.220 (2026-07-25) by reading the
shipped binary's settings schema; cross-checked against `wozcode-plugin`,
which leans on all of these.

| Surface | Key | Ships in plugin? | Status |
|---|---|---|---|
| Main-thread agent | `agent` in plugin `settings.json` | **Yes** | `parsec:code` (`agents/code.md`) |
| Per-subagent line | `subagentStatusLine` | Yes, but see below | `parsec subagent-statusline` |
| Spinner verbs | `spinnerVerbs` | No | appended by `parsec setup` |
| Commit/PR trailer | `attribution.commit` / `.pr` | No | **not done** — see below |
| Tool namespace | MCP server key in `.mcp.json` | Yes | `mcp__plugin_parsec_scout__*` |

Notes that cost time to establish:

- **Plugin `settings.json` is auto-discovered** from the plugin root (no
  `plugin.json` key points at it) and accepts only `agent` and
  `subagentStatusLine`. Confirmed again on 2.1.220.
- **`${CLAUDE_PLUGIN_ROOT}` does NOT expand in `settings.json`** — only in
  hooks declared in `hooks/hooks.json`. The binary itself says so:
  *"This variable is only available in hooks defined in a plugin's
  hooks/hooks.json file, not in settings.json."* So although a plugin *may*
  ship `subagentStatusLine`, it cannot name our binary there. Ours therefore
  rides the `parsec setup` managed-settings merge with the absolute exe path,
  exactly like `statusLine`.
- **`subagentStatusLine` protocol**: stdin gets
  `{columns, tasks: [{id, name, type, status, description, label, startTime,
  model, effort, contextWindowSize, tokenCount, tokenSamples, cwd}], …}`;
  stdout is JSON Lines, one `{"id", "content"}` per task, and any
  unparseable line is dropped. Polled every 5s with a 5s timeout — keep it a
  pure format of the payload, no disk or network.
- **`spinnerVerbs`** is `{mode: "append" | "replace", verbs: string[]}`. We
  use `append`: the stock verbs are part of the harness's personality and we
  are a guest in it. Ownership is marked by the sentinel verb `Parsecing`
  rather than an exact list match, so a later version can change the list and
  still recognise (and clean up) its own block.
- **`attribution`** is `{commit, pr, sessionUrl}`, free-form strings. This
  writes our name into the user's git history, so it must stay opt-in behind
  an explicit toggle — not something `setup` does silently. Deliberately not
  implemented yet.

All three managed keys follow the same ownership discipline as the env merge:
absent → written; ours → refreshed; **user-authored → never touched, and
never removed by `parsec disable`**.

# Part 2 — Install flow: distribution, setup, and onboarding

## User-side install paths

Interactive: `/plugin` opens the manager UI — Discover tab to browse/search,
Marketplaces tab to add sources, Installed tab to enable/disable/uninstall.
Before install the UI shows a "Will install" section (skills, agents, hooks,
MCP servers) plus a context-cost estimate.

CLI equivalents:

```bash
claude plugin marketplace add daseinlabs/plugins   # GitHub shorthand; also git URLs, local paths, raw marketplace.json URLs, @tag pinning
claude plugin install parsec@parsec-marketplace --scope user|project|local
claude plugin list / enable / disable / uninstall / update / details
/reload-plugins                                            # activate in current session
```

Direct git-repo install without a marketplace exists
(`claude plugin install <name> --plugin-url <repo>`) but is a one-off path,
not for distribution.

Ref: https://code.claude.com/docs/en/discover-plugins.md

## Marketplace (what we publish)

A git repo with `.claude-plugin/marketplace.json` cataloging plugins:

```json
{
  "name": "parsec-marketplace",
  "owner": { "name": "Dasein Labs", "email": "support@daseinlabs.ai" },
  "plugins": [
    {
      "name": "parsec",
      "source": "./plugins/parsec",
      "description": "Measured token savings via local compression proxy",
      "version": "1.0.0",
      "category": "productivity",
      "keywords": ["tokens", "savings", "proxy"]
    }
  ]
}
```

- Plugin `source` can be a relative path, GitHub repo (pinnable to ref/SHA),
  git URL, git subdirectory (monorepo-friendly), or npm package.
- Anthropic ships an official marketplace (auto-available) and a community
  one (`anthropics/claude-plugins-community`) — a listing there is a
  discovery channel worth pursuing.

Ref: https://code.claude.com/docs/en/plugin-marketplaces.md

## Scopes and team auto-install

| Scope | Written to | Visibility |
|---|---|---|
| user (default) | `~/.claude/settings.json` | all projects, this user |
| project | `.claude/settings.json` (in git) | whole team |
| local | `.claude/settings.local.json` (gitignored) | this user, this project |

Precedence: local > project > user. Team auto-install: check
`extraKnownMarketplaces` + `enabledPlugins` into `.claude/settings.json`;
teammates get a workspace-trust dialog on first open, then are prompted to
install the listed plugins.

Ref: https://code.claude.com/docs/en/plugins-reference.md#plugin-installation-scopes

## Updates and versioning

Version resolution order: `version` in plugin.json → `version` in marketplace
entry → git commit SHA → `unknown`. Two viable strategies:

- Explicit `version` in plugin.json, bumped per release (pushing commits
  alone does NOT update existing users), or
- No version field → commit SHA acts as version, every commit is an update.

User commands: `claude plugin update [name@marketplace]`,
`/plugin marketplace update`. Marketplaces refresh on startup (random ~10min
delay). Auto-update is ON for official marketplaces, OFF by default for
third-party ones (user-toggleable) — so we should not rely on silent updates.
Plugins cache to `~/.claude/plugins/cache/<marketplace>/<plugin>/<version>/`.

Ref: https://code.claude.com/docs/en/plugin-marketplaces.md#version-resolution-and-release-channels

## Bootstrapping the native binary + proxy (the hard part)

**There is no post-install hook.** The `Setup` hook only fires under
`claude --init-only` / `-p --init` / `--maintenance` (CI/container flows),
NOT on interactive install or first session. Documented pieces we can
combine:

1. **`bin/` directory** — plugins can ship executables in `bin/`; it is added
   to `PATH` for the session. So: ship **pre-built** `parsec` binaries per
   platform (macos-arm64/x86_64, linux-x86_64; sign + notarize macOS). No
   build step on install. Referenced as `${CLAUDE_PLUGIN_ROOT}/bin/parsec`
   in hooks/MCP config.
2. **`userConfig` in plugin.json** (v2.1.154+) — typed setup prompts shown at
   enable time (string/boolean fields, `sensitive: true` for secrets →
   Keychain). Values reach hooks as `CLAUDE_PLUGIN_OPTION_<NAME>` env vars
   and MCP config via `${user_config.<name>}` substitution. Use for proxy
   endpoint / BYOK key. **Constraint (CLAUDE.md): BYOK API keys only —
   subscription OAuth tokens must never route through our cloud.**
3. **`env` in the plugin's `settings.json`** — sets env vars for the session
   while the plugin is enabled, e.g. `ANTHROPIC_BASE_URL` pointed at the
   local proxy. Plugin env cannot override user-level env. This is the
   documented mechanism for the transparent proxy wiring.
4. **SessionStart hook as health check** — every session, verify the proxy is
   reachable; on failure emit `systemMessage`
   ("parsec proxy not running — start with `parsec proxy`") and fail open.
5. **`/parsec:setup` skill** — interactive fallback wizard for anything that
   can't happen automatically (first proxy launch, login, building from
   source).

Ref: https://code.claude.com/docs/en/plugins-reference.md#user-configuration,
https://code.claude.com/docs/en/hooks.md, https://code.claude.com/docs/en/settings.md

## Trust and permissions

- **Workspace-trust dialog** appears on first open of a project whose
  `.claude/settings.json` / `.mcp.json` / plugin config can run scripts
  (hooks, MCP servers, monitors). Decline → project-scope plugins don't load.
- After trust: skills, agents, hooks, and plugin `env` run without further
  prompts; MCP tool calls still get per-tool permission prompts by default.
- Org admins can allowlist/block marketplaces via managed settings
  (`strictKnownMarketplaces` / `blockedMarketplaces`) — relevant for
  enterprise distribution later.

Ref: https://code.claude.com/docs/en/settings.md#workspace-trust

## Recommended onboarding flow

1. User: `claude plugin marketplace add daseinlabs/plugins` (one time).
2. `claude plugin install parsec@parsec-marketplace` (or via `/plugin` UI).
3. Enable-time `userConfig` prompts: proxy port/endpoint, BYOK key (optional,
   `sensitive: true`).
4. Plugin `settings.json` `env` sets `ANTHROPIC_BASE_URL` → local proxy.
5. SessionStart hook health-checks the proxy; `systemMessage` with the fix
   command if down (fail open — never block the session).
6. Statusline + `/parsec:savings` take over from there (see Part 1).

Open items to verify against a live Claude Code version before building:
`userConfig` field semantics, `bin/`-on-PATH behavior, and whether plugin
`env` can set `ANTHROPIC_BASE_URL` specifically (vs. it being reserved) —
these came from docs current as of 2026-07-15 and the feature set moves fast.
