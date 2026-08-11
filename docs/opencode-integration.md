# opencode integration — implementation notes

**Status:** implemented 2026-07-29 (Phase 1 of `docs/tool-integrations-survey.md`
§3.6 — zero engine work, Anthropic-wire reuse). Research background: survey §2.

## What shipped

| Piece | Where | What it does |
|---|---|---|
| Shim plugin | `packages/opencode-plugin/index.js` | Routes `provider.anthropic.options.baseURL` at the local proxy **only after `/health` proves a parsec proxy is listening**; tags requests `x-parsec-tool: opencode`; toasts session savings on `session.idle` (opencode has no status line). Plain JS, zero deps, no build step. |
| Installer | `parsec setup opencode` (`packages/proxy/src/setup_opencode.rs`) | Drops the shim (embedded via `include_str!` — byte-identical to the npm artifact by construction) into `~/.config/opencode/plugin/parsec.js`, symlinks `~/.parsec/bin/parsec` at the running binary (unix; the stable path the shim and its command templates probe when `parsec` is not on PATH — a stale link dangles and degrades to PATH probing, never a stale binary), and warms the proxy. `parsec disable opencode` removes the shim; `parsec uninstall` removes both. |
| Ledger attribution | `tool` field on savings-ledger rows | Proxy extracts `x-parsec-tool` (charset-gated slug, so the contract still cannot carry raw text), writes it on the row; schema + platform pydantic mirror + governor example updated. Absent = Claude Code. |
| Commands | `commandsFor()` in `packages/opencode-plugin/index.js` (added 2026-08-11) | `/parsec-savings`, `/parsec-proxy`, `/parsec-key`, `/parsec-setup`, `/parsec-uninstall` — the opencode port of the Claude Code skills, injected as `config.command` entries through the same config hook. No files on disk; a user-defined command of the same name always wins; the resolved `parsec` binary path is baked into each template at plugin load. The `share` skill is **not** ported: the CLI has no `share` subcommand for it to call (same gap exists in the Claude Code skill). |

## Decisions and their whys

- **File drop over npm as the primary install.** opencode auto-loads
  `~/.config/opencode/plugin/*.js`, so `parsec setup opencode` needs no
  npm publish, no network, and no `opencode.json` edit (nothing of the
  user's is rewritten). The same `index.js` is npm-publishable as
  `parsec-opencode` later; `include_str!` keeps the two artifacts identical.
  Ownership discipline mirrors settings.json: a sentinel
  (`parsec-managed-plugin`) marks our file; a file without it is never
  touched, refreshed, or deleted.
- **The shim never touches `~/.parsec/setup_state.json`.** That file drives
  Claude Code's auto-setup phase machine; an opencode-only install marking it
  `ready` would make a later Claude Code setup skip its settings routing.
  Both sides fall back to the default port (8082) so they agree without
  sharing state.
- **Fail open, per §8.3.** No live proxy after a 2s `parsec up` attempt →
  the shim leaves config untouched and opencode runs unrouted. A foreign
  `baseURL` (user's own gateway) is never overwritten; a *stale local
  parsec* route is re-pointed at the live port.
- **BYOK only.** opencode removed Claude Pro/Max OAuth in v1.3.0 ("Anthropic
  explicitly prohibits this") — subscription users are out of scope, matching
  our own OAuth rule (§3).
- **Thin-JS exemption.** Client code is Rust by rule; opencode plugins are
  unavoidably JS. The shim carries no curation logic — routing, tagging, and
  a toast — everything real stays in the proxy. (DIRECTION.md ratification
  of this exemption is still an open item: survey §3.7.4.)
- **Attribution via header, not port.** One shared proxy for all tools; the
  allowlist in `forward_auth_headers` already guarantees the tag never
  reaches Anthropic. Slug pattern `^[a-z0-9-]{1,32}$` is the contract-level
  guarantee the field can't smuggle text (same posture as `session_id`).

## Not done yet (follow-ups)

- ~~Distribution for opencode-only users~~ shipped 2026-08-11:
  `scripts/install-opencode.sh` (`curl …/install-opencode.sh | bash`) downloads
  the platform binary from the public marketplace repo into
  `~/.parsec/bin/parsec` (the shim's probe path) and runs `parsec setup
  opencode`; release.yml publishes the script beside the binaries so both
  always ship from the same commit. npm publish of `parsec-opencode` remains
  an optional alternative install path (`"plugin": [...]` config entry), not
  a prerequisite.
- Free-tier UX ports: explore/map agent md → `.opencode/agents/`, no-reread
  via `tool.execute.before` (survey §3.4). Commands shipped 2026-08-11 via
  config-hook injection (see table above), deliberately not as
  `.opencode/commands/` file drops — one artifact, no extra ownership
  tracking, removed with the plugin.
- `parsec share` CLI subcommand — until it exists, neither Claude Code's
  share skill nor an opencode `/parsec-share` command has anything to call.
- Per-tool savings split in `parsec savings` / statusline / dashboard (rows
  carry `tool` now; nothing reads it yet).
- Cline (survey Phase 1 second target): base-URL instruction flow.
- Verify shim hook names against opencode at install-time version pinning —
  `config` and `event` hooks and `client.tui.showToast` are wrapped in
  try/catch fail-opens, but a rename would silently disable routing; add a
  smoke test against a pinned opencode release in CI.

## Manual test

```
cargo build -p parsec-proxy
target/debug/parsec setup opencode      # drops shim, warms proxy
opencode                                # run any Anthropic API-key session
/parsec-savings                         # in the TUI: commands registered + ledger read
tail -f ~/.parsec/proxy.log             # per-request savings lines
grep '"tool":"opencode"' ~/.parsec/ledger.jsonl | tail
target/debug/parsec disable opencode    # clean removal
```
