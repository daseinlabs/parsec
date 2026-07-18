---
name: dasein-savings
description: Show measured token savings — re-reads blocked and command loops broken by the dasein no-reread hook, from the local ledger, never estimated.
---

Run `${CLAUDE_PLUGIN_ROOT}/bin/dasein savings` and present its output to the
user conversationally. Every number in that report is measured — proxy rows
are the per-request count_tokens counterfactual vs actually-billed usage,
hook rows are blocked re-reads x the on-disk bytes of the denied range; if
the report says the ledger is empty, say so plainly — never estimate or
extrapolate savings. The report is grouped by requests, conversations, and
Claude Code sessions where the ledger has session identity.

If the user asks about the live status line: `dasein setup` writes a managed
`statusLine` entry into `~/.claude/settings.json` automatically (plugins
cannot ship the key themselves) and never overwrites one the user already
has. If they want it back after removing it, rerunning `dasein setup` — or
just starting a new session — re-adds it; `dasein disable` removes it along
with the other managed keys.
