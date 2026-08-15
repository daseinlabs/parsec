---
name: trim
description: Stage a det+dir compaction of this session — a deterministic needed-set trim of the transcript plus a standing-directives block, injected automatically after /clear. Use when context is nearly full and the user wants a better /compact.
---

Follow these steps in order. Every number you report from step 1 is an
ESTIMATE (chars/4) — always say "estimated", never present it as measured.

If the parsec MCP tools `trim_stage` and `trim_finalize` are available, use
them instead of the shell commands below: `trim_stage` (optionally with
`level`) replaces step 1 and `trim_finalize` (directives as the argument)
replaces step 3 — no heredoc needed. Everything else is unchanged.

1. Run `"${CLAUDE_PLUGIN_ROOT}/bin/parsec" trim --json`.
   - If the user asked for a specific trim aggressiveness, add `--level N`
     (1 = low trimming / keep more, 5 = very high). Omit it otherwise — the
     default (3) is the only measured configuration; the other levels shift
     what counts as "used later" and are estimates all the way down.
   - Exit code 2 means the session is too short to trim (or no transcript was
     found) — relay the binary's message plainly and stop.
   - Otherwise present the stats conversationally: kept X of Y chunks,
     roughly A of B estimated tokens. This is the deterministic trim — the
     parts of this session that were actually re-read, edited, or used later.

2. Now write the STANDING DIRECTIVES block from your own in-context history.
   You are the compaction assistant here; the history is already in your
   context. Your job is NOT to summarize the work. Extract ONLY the STANDING
   DIRECTIVES: durable rulings, policies, quantitative floors, model/tool
   choices, scope decisions, and the north-star — the things the user
   established that must keep governing behavior AFTER compaction. Exclude
   one-off task steps and transient state. Output a terse pinned-constraints
   list, each line an imperative the agent must keep obeying. If a directive
   was stated with force or repeated, keep its force. Keep the list under
   roughly 3,000 tokens.

3. Stage the directives (heredoc keeps the artifact on disk complete before
   anything else happens):

   ```
   "${CLAUDE_PLUGIN_ROOT}/bin/parsec" trim --finalize <<'PARSEC_DIRECTIVES'
   <the directives list>
   PARSEC_DIRECTIVES
   ```

4. When the binary prints `ready`, tell the user: run `/clear` now — the next
   session in this project starts with the trimmed context injected
   automatically. Mention it expires unused after 30 minutes, and that it
   fires on `/clear` (or a fresh start), not on resume.

5. If step 3 fails for any reason, say so plainly and tell the user the
   deterministic trim alone is still staged and will be injected without the
   directives block after `/clear` (that is the designed fallback, not an
   error state).

Never estimate savings the ledger does not show, and never run `/clear`
yourself — the user decides when the session ends.
