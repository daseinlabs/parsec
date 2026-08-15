# Bug: no-reread hook blocks re-reads of content the curator already elided

> **Status 2026-08-14: fixed.** Suggested fixes 1/3 landed as the curator→hook visibility
> channel (`packages/proxy/src/visibility.rs` + `noreread.rs` gate check); fix 2 landed as
> per-file cross-surface escape grants that survive compound commands; fix 4 was resolved by
> stage-aware shell parsing (network fetches no longer produce read records); fix 5 by
> refunding booked savings whenever a denial is escaped. Native compaction is handled by
> resetting hook state on `SessionStart source=compact`. Full accounting in
> `NOREREAD_HOOK_DEFECT.md`.

**Date:** 2026-08-06
**Source:** live Claude Code session (Fable 5) with the parsec plugin active, reported by the agent itself mid-session
**Severity:** high — puts agents in a workaround loop and burns more tokens than the hook saves

## Summary

The no-reread hook and the context curator/compressor don't share state. The curator can elide a file's content from a tool result *before the model ever sees it* (e.g. `[... 85 lines (~1013 tokens) · re-read dasein.md:L1-88 omitted ...]`), but the hook still records that file as "fully read." Any subsequent read is then rejected with the CONTEXT-REUSE error telling the model to "reuse the content from the messages above" — content which does not exist in its context.

## Repro (observed in session)

1. `Read` two files in one parallel batch. The second result was consolidated into the first, and the first was itself elided by the curator. Net: model saw ~2 lines of an 88-line file.
2. `cat` the same files → blocked: "You already read dasein.md (the entire file)… reuse the content above."
3. The error's escape hatch says: "If the earlier content was genuinely summarized away and you need it back, re-issue this exact read and it will be allowed once." Re-issuing the **exact same command** was blocked again (it errored on the *other* file in the compound command, and the allowed-once grant apparently didn't survive / didn't apply to the compound).
4. Same failure later on a single-file read (`cat user_profile.md`): first read elided to 2 lines, re-issue blocked.
5. Also fired on **non-file content**: a `curl https://daseinlabs.ai | sed …` pipeline was tracked as "daseinlabs.ai:L1-1" and its output elided (~6018 tokens) — same trap would apply on retry.

**Workaround the agent found:** `cp` the file into the session scratchpad under a new path and `cat` the copy. Works every time, defeats the hook entirely, and costs an extra tool call + full file tokens — strictly worse than just allowing the re-read.

## Why this is bad

- The hook's premise ("you already have this content") is false whenever the curator elided the original. The two features are individually correct but compose into a deadlock.
- The advertised escape hatch ("re-issue this exact read and it will be allowed once") does not work reliably, at least for compound commands (`cat a; cat b`) and possibly after any intervening tool call.
- Agents will discover the copy-to-new-path workaround (ours did within one attempt), so the failure mode in the wild is silent: no bug report, just inflated token usage that the ledger will attribute as savings ("~582 tokens saved") while the session actually spent more.

## Suggested fixes

1. **Share state:** when the curator elides/summarizes a region of a read result, mark that region as *not held* in the reread tracker (or record the post-elision line ranges as the only "seen" content).
2. **Honor the escape hatch:** make the allow-once grant keyed per file, robust to compound commands, and persistent until actually consumed by a successful read.
3. **Treat elision markers as evidence:** if the tool result delivered to the model contained an `omitted` marker for file X, never block a re-read of X.
4. Don't apply file-read tracking to network fetches (`curl` output tracked as `daseinlabs.ai:L1-1`).
5. Ledger accounting: a blocked read that leads to a `cp` + re-read workaround should not be counted as savings.

## Notes

- Session context: `~/code/subygan/subygan.github.io/blog`, session `e1c7e46c-612a-411e-858a-9cf54574da95` (Claude Code local transcript should have the full trace, including exact hook error text).
