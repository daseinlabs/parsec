---
name: proxy
description: Restart the local parsec proxy if it was killed mid-session — brings the supervisor back on the routed port so requests stop failing. Use when the user says the proxy died, requests hang or error against 127.0.0.1, or they killed the proxy and want it back.
---

Run:

```
${CLAUDE_PLUGIN_ROOT}/bin/parsec up
```

and report its output. The command is idempotent and detached: a live listener
on the routed port (parsec's or the user's own) is never double-spawned, so it
is always safe to run.

How to read the result:

- **"proxy already listening on 127.0.0.1:PORT — nothing to do"** — the
  routed port is healthy. If the user is still seeing failures, the problem
  is not a dead proxy; a dead *worker* needs no intervention either — the
  supervisor respawns it and forwards straight to Anthropic in the gap.
- **"proxy up on 127.0.0.1:PORT — a stuck session recovers on its next
  request"** — revived. No restart needed: sessions already routed at that
  port recover on their next request.
- **"proxy spawned … but never started listening — check ~/.parsec/proxy.log"**
  — the spawn failed. Read the tail of that log
  (`tail -30 ~/.parsec/proxy.log`) and summarize the actual error for the
  user rather than guessing.

Scope note: `parsec up` revives the proxy on the port sessions were launched
with. It cannot fix routing that was never written — if
`~/.claude/settings.json` has no parsec `ANTHROPIC_BASE_URL` (e.g. after
`parsec disable`), that is `/parsec:setup`, not this command.
