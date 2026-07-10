---
name: dasein-savings
description: Show measured token savings — re-reads blocked and command loops broken by the dasein no-reread hook, from the local ledger, never estimated.
---

Run `${CLAUDE_PLUGIN_ROOT}/bin/dasein savings` and present its output to the
user conversationally. Every number in that report is measured (blocked
re-reads x the on-disk bytes of the denied range); if the report says the
ledger is empty, say so plainly — never estimate or extrapolate savings.

If the user asks about the live status line, tell them it is a one-line user
setting (plugins cannot set it): add to `~/.claude/settings.json`:

```json
{ "statusLine": { "type": "command", "command": "dasein statusline" } }
```

(using the full path to the plugin's `bin/dasein` if `dasein` is not on PATH).
