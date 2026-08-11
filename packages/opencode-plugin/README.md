# parsec-opencode

Thin [opencode](https://opencode.ai) shim for the parsec curating proxy
(docs/tool-integrations-survey.md §3.4 — Phase 1). All curation stays in the
local Rust `parsec` binary; this plugin only:

1. routes opencode's **Anthropic provider** at the local proxy
   (`provider.anthropic.options.baseURL`), and only after `/health` proves a
   parsec proxy is actually listening — otherwise opencode runs untouched;
2. tags requests with `x-parsec-tool: opencode` so the savings ledger
   attributes rows per tool (the proxy strips the header before upstream);
3. shows session savings as a TUI toast (opencode has no status line).

**Scope: BYOK API-key users only.** opencode removed Claude Pro/Max OAuth in
v1.3.0; parsec does not route subscription traffic here either.

## Install

No parsec binary yet (opencode-only users) — one line, no npm, no sudo;
writes only `~/.parsec` and `~/.config/opencode`:

```
curl -fsSL https://raw.githubusercontent.com/daseinlabs/claude-plugins/main/install-opencode.sh | bash
```

Already have the parsec binary (Claude Code plugin users):

```
parsec setup opencode
```

Or as an npm plugin in `opencode.json` / `~/.config/opencode/opencode.json`:

```json
{ "plugin": ["parsec-opencode"] }
```

The plugin finds the proxy port from `~/.parsec/setup_state.json` (default
8082) and starts the proxy via `parsec up` when it isn't running (binary
resolution: `PARSEC_BIN`, then `PATH`, then `~/.parsec/bin/parsec`).

## Uninstall

`parsec disable opencode` (file drop), or remove the `plugin` entry (npm).

## Why is this JavaScript?

Client code here is Rust by rule (CLAUDE.md §7b); opencode plugins are
unavoidably JS. This file is the sanctioned thin-shim exemption: no
dependencies, no build step, no curation logic — everything real happens in
the `parsec` binary and the proxy.
