# Production no-reread hook: yes, it's broken — it blocks test execution

**TL;DR: The shipped no-reread hook treats any Bash command that *mentions* a previously-read
filename as a "re-read" and fully blocks it. That includes `pytest path/to/test_file.py`.
In our 33 SWE-bench instance-runs (3 hook-bearing arms × 11 instances, Sonnet 4.6), it fully
blocked 115 test/interpreter executions — the agent got a 500-char denial instead of test
output, every single time (0 were advisory). Blocking a test run is never a valid re-read
denial: test output cannot be "reused from the messages above." Two smaller design gaps
compound it. Fix directions at the bottom.**

All numbers below are measured from full run transcripts (swemem15m, 2026-08-14; arms =
plugin-only, plugin+proxy, memory+plugin+proxy). Nothing estimated.

> **Status 2026-08-14: fixed in `packages/proxy/src/noreread.rs`** (regression tests use the
> verbatim blocked commands below). What shipped, and how the diagnosis was refined:
>
> - **Defect 1's actual trigger is narrower than "mentions a filename":** a bare
>   `pytest x.py` was never gated. The trigger was the `2>&1 | tail -40` truncation idiom —
>   the range scan matched `tail -40` anywhere in the statement and path collection was
>   statement-wide, so the interpreter's *argument* became the "file read." Same mechanism
>   caught `grep … | head -20`. **Fix:** per-pipe-stage parsing — a stage is a read only when
>   the read verb and a file argument share the stage; a file-less `head`/`tail` consumes
>   stdin and only narrows an upstream `cat`'s range. This also stops `record_bash` from
>   *recording* phantom reads (the false positives were self-generating: two `| tail -30`
>   pytest runs produced the "last 20/30 lines" records quoted below without any real read).
> - **Defect 2:** `MIN_DENY_TOKENS = 150` floor — a re-read cheaper than the denial message
>   is always allowed and books nothing.
> - **New — compound-command deadlock:** the insist valve was keyed per (path, range), so
>   `cat a; cat b` alternated denials between the two files *forever*. Now a denial grants
>   every denied path a one-shot pass (cross-surface: shell or Read tool, any range), so one
>   re-issue passes whole, as the denial text promises. Consuming a grant refunds the booked
>   savings (the content was re-fetched — measurement honesty).
> - **New — loop-breaker false positives:** `cmd_counts` never reset, so the 4th identical
>   `pytest` run after three edit→test cycles was denied with a fabricated "produced no new
>   result" claim. Any edit/eviction now resets the counts; the denial text now only claims
>   "no edits in between."
> - **Defect 3: fixed via a curator→hook visibility channel**
>   (`packages/proxy/src/visibility.rs`). After each curated serve the proxy exports the
>   freezer's cut registries (dropped ranges + insist-restored ranges, keyed by the request's
>   Claude Code session id from `metadata.user_id`) to
>   `~/.parsec/sessions/<sid>.elided.json`; the hook loads it before gating and never denies
>   a re-read that touches dropped-and-never-restored content. This closes the measured
>   deny → workaround → insist-restore loop: the elided re-read passes the hook once, the
>   proxy's insist valve serves it full, the range moves to "served," and only then does the
>   hook deny again — correctly. For the elision source the proxy cannot see (Claude Code's
>   native compaction), `SessionStart source=compact|clear` now resets all read/loop state.
>   Fail-open everywhere: no export file (plugin-only arm) means the pre-channel behavior,
>   and the escape grant remains the valve of last resort. See
>   `bug-2026-08-06-noreread-vs-context-compression.md` (same class, observed live) — its
>   `cp`-to-scratchpad savings-inflation concern is also addressed, since blocked-then-
>   escaped reads now refund their booked savings.

---

## Defect 1 (severe): filename-substring matching blocks Bash EXECUTION

**Mechanism.** The hook tracks per-session read ranges (`~/.parsec/sessions/<uuid>.json`).
On a Bash PreToolUse, if the command string contains a filename the session has read and the
file is unchanged, the command is denied as a re-read. It does not distinguish *reading* a
file from *executing* something that takes the file as an argument.

**Measured, verbatim blocks** (result = denial only, no command output):

```
CMD:    python -m pytest testing/test_mark_expression.py -v 2>&1 | tail -40
DENIED: [CONTEXT-REUSE — do not re-read what you already have] You already read
        test_mark_expression.py (the entire file) earlier in THIS conversation...

CMD:    python tests/runtests.py generic_views.test_base --parallel 1 2>&1 | tail -40
DENIED: ...You already read runtests.py (last 20 lines, last 30 lines) earlier...

CMD:    python -m sympy.testing.runtests sympy/core/tests/test_arit.py::test_Mod
DENIED: ...You already read test_arit.py (last 20 lines) earlier...
```

The django one is the purest case: the agent read the *last 30 lines of the test runner
script* once, and from then on **every invocation of the test runner** matched "runtests.py"
and got blocked.

**Size of the problem (33 instance-runs):**

| arm | total denials | blocked Bash EXECUTIONS | blocked Read/greps | denials claiming <150 tok saved |
|---|---|---|---|---|
| plugin only ("scout") | 79 | 51 | 28 | 27 |
| plugin + trim proxy | 76 | 49 | 27 | 29 |
| memory + plugin + proxy | 147 | 79 | 68 | 42 |
| **total** | **302** | **179 (59%)** | **123** | **98 (32%)** |

Of the exec blocks, 115 are interpreter/test-runner invocations (pytest, `python -m`,
runtests.py); the rest are shell searches (`grep -n "def test_"`, `wc -l`) that are searches,
not re-reads. **This defect needs no proxy and no special setup — the plugin-only arm shows
it at full strength. Every production user hits it.**

**Cost.**
- *Direct:* the denial text tells the agent "re-issue this exact read and it will be allowed
  once" — so the *floor* cost is one wasted model round-trip per false positive (we measured
  19 exact identical re-issues; the rest reformulated instead, which is worse). ~5.4 wasted-
  or-detoured requests per instance on average, ~$0.035/request in our runs → small direct
  dollars (~$2–4 per 11-instance run) **but that is the floor, not the cost.**
- *Behavioral (the real cost):* agents don't reliably re-issue; they route around. On
  pytest-7324 the workaround cascade contributed ~22 of 30 extra requests (2.3× the steps for
  the identical fix). On sympy-23413 the agent burned turns on `python -c` re-derivations of
  values it couldn't re-fetch and **missed a second real bug** that the unhindered arm found
  and fixed. Blocking *verification commands* specifically degrades the agent at the exact
  moment it is trying to confirm its work.
- *Context pollution:* each denial injects ~125 tokens (median 498 chars) permanently into
  the conversation. 302 denials ≈ 37k tokens of denial text across the three runs — against
  ~128k claimed saved, and 98 of the 302 denials individually claimed less savings (~17–150
  tokens) than their own message cost.

## Defect 2: no minimum-savings floor

Disassembly-confirmed (`noreread::tokens_for_range` → bytes/4; `gate_read` has no comparison
against any floor before denying): the hook denies *any* tracked re-read regardless of size.
We measured denials proudly reporting "~17 tokens saved" — delivered via a ~125-token denial.
A third of all denials (98/302) were net-negative or near-zero even before counting the
wasted round-trip.

## Defect 3 (hook + trim proxy together): the hook doesn't know what the proxy removed

The hook's state records what was *read*; the proxy curates what stays *visible*. There is no
coordination channel (hook state is a local sessions file; byte-identical hook binary with or
without the proxy). So when the proxy trims a big file read out of context and the agent
re-reads it, the hook denies with "reuse the content from the messages above" — **content that
is no longer above**. The agent then reconstructs via Grep cascades (12 consecutive
reconstruction Greps on 7324), and the proxy's insist-valve restores the trimmed content
anyway (317 insist events in the run window, concentrated on exactly the inflated instances)
— re-paying what trimming saved. Hook and proxy each behave as designed; together they form a
deny → workaround → restore loop.

## What is NOT broken

- The core idea works: 123 genuine re-read/search blocks with real savings; where nothing
  interfered, the full stack showed its intended shape (one instance: 50 → 13 requests).
- Nothing hard-blocks: the re-issue-once escape always exists. This is a tax and a behavior
  degrader, not a hard failure.
- Solve rates in our runs were not visibly moved by the hook alone — the cost is steps,
  tokens, latency, and occasionally missed secondary findings.

## Fix directions (in priority order)

1. **Stop gating executions.** Only deny Bash commands that are *pure reads* (`cat`, `head`,
   `tail`, `sed -n` with no interpreter in the pipeline) — or drop Bash matching entirely and
   gate only the Read tool. An executed command's output is new information by definition.
2. **Minimum-savings floor** ≥ the denial message's own cost (~150 tokens).
3. **Visibility check before denial** when the trim proxy is active: the proxy knows what
   survived curation; the hook should only deny re-reads of content still in the visible
   context (or the proxy should pin ranges the hook has marked read).

*Provenance: swemem15m run transcripts (33 instance-runs), hook binary disassembly
(sha256 a56c0a65…, identical across plugin variants), proxy log `~/.parsec/proxy.log`.
Happy to hand over the per-denial CSV or the transcript excerpts.*
