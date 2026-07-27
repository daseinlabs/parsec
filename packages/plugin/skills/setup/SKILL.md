---
name: setup
description: Activate parsec — write the Claude Code routing env (ANTHROPIC_BASE_URL → local proxy), install the savings status line, and start the proxy. Use when the user wants to set up parsec, enable curation, or re-run a failed first-time setup.
---

Setup normally runs automatically on first session, so reaching for this skill
usually means that run failed or the user undid it with `parsec disable`. Run:

```
${CLAUDE_PLUGIN_ROOT}/bin/parsec setup
```

(No `--auto` flag — that is the hook-spawned first-run mode which respects
terminal states; a manual run should retry.) Show the user the command's
output and explain what happened based on which line it printed:

- **"routing written to Claude Code settings (127.0.0.1:PORT)"** — setup added
  `ANTHROPIC_BASE_URL` to the `env` block of `~/.claude/settings.json` and
  started the proxy on that port. Tell the user to **restart their Claude Code
  sessions**: routing env is read at session launch, so this session and any
  other open ones are still talking straight to Anthropic. Undo anytime with
  `parsec disable`.
- **"routing NOT written: ANTHROPIC_BASE_URL is already …"** — the user has
  their own base URL set and parsec will never overwrite it. Relay the printed
  URL and the port; if they want curation they must point that variable at
  `http://127.0.0.1:PORT` themselves (or clear it and re-run setup).
- **"routing already pointed at a local parsec proxy — kept as-is"** — setup
  was already done; nothing changed. If their concern is a dead proxy rather
  than routing, that is `/parsec:proxy`, not setup.
- **"proxy pre-warm failed …"** — routing is written but the proxy did not
  start. Not fatal (the SessionStart hook retries next session), but you can
  fix it now by running `${CLAUDE_PLUGIN_ROOT}/bin/parsec up` and reporting
  the result.

Setup is additive-only and idempotent: it never overwrites an env key or
`statusLine`/`subagentStatusLine` the user authored themselves — it only
writes absent keys and repoints entries it wrote under an older plugin
version. Safe to re-run.

If the user has a dashboard API key to connect afterwards, that is
`/parsec:key`, not part of setup.
