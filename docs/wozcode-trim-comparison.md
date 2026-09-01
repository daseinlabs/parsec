# WozCode vs parsec: context-trimming comparison

*Written 2026-08-28, from a reverse-engineering pass over the wozcode-plugin
bundles (obfuscated esbuild output; string literals and constants decoded
exactly, internal symbol names reconstructed) and the parsec trim pipeline
(`packages/proxy/src/trim.rs`, `packages/engine/src/needed.rs`,
`docs/trim.md`, `docs/DASEIN_TRIM_HANDOFF.md`).*

## TL;DR

WozCode doesn't trim at all. They let Claude Code's native compaction run
untouched, then **repair its losses after the fact**: a deterministic
~900-character "digest" of the turns compaction discarded, injected once as
`additionalContext`, where each bullet is a *pointer* the model can expand on
demand via their `recall` BM25 search agent. Parsec's `/parsec:trim` is the
opposite philosophy: **replace compaction entirely** — recompute from the full
transcript which chunks were provably "used later," inject that verbatim
needed-set (plus model-written standing directives) after `/clear`. Theirs is
a tiny lossy index with lazy retrieval; ours is a full extractive replacement
with no retrieval path.

## How wozcode's mechanism works

1. **Trigger:** `SessionStart` with `source === "compact"` writes only a
   marker file (`~/.claude/wozcode/post-compaction-digest/<sessionId>.json`,
   15-min TTL). The digest is built lazily on the next `UserPromptSubmit` or
   `PreToolUse`, whichever fires first.
2. **Recover the dropped set:** they stream the session JSONL, find the
   `compact_boundary` system entry, and diff its
   `compactMetadata.preservedMessages.allUuids` against the turns before the
   boundary — exactly the set compaction discarded. No token thresholds, no
   content heuristics.
3. **Rank, don't summarize:** dropped turns are scored with BM25F (same
   ranker as their `recall` CLI; k1=1.2, b=0.75 over `text` + `toolCall`
   fields) against a "direction query" — 1500 chars sliced from Claude Code's
   own compact summary starting at the first of the headings
   `Pending Tasks / Current Work / Optional Next Step / Next Step`. So it
   surfaces dropped turns *relevant to what the session was about to do
   next*. Score is penalized by `1 - 0.6 × overlap(turn, retainedTurns)`
   (don't re-inject what survived), and pure echoes of the summary are
   filtered out.
4. **Render:** max 5 bullets, 120 chars each (first non-empty line of the
   tool invocation or prose), 900 chars total, each tagged
   `[id: <turnAnchorUuid>]`, headed by:
   > Recovered from before the last compaction (ask the recall subagent to
   > expand any item — `recall --id <sessionId>:<id>`):
   Fully deterministic — no LLM call anywhere in the path. One-shot: the
   marker is cleared once a boundary is found; if the boundary isn't in the
   transcript yet the next hook retries, up to the 15-min TTL.
5. **Everything else is upstream prevention**, not transcript surgery:
   - PreToolUse redirects native Read/Grep/Glob/Edit to their MCP
     equivalents, which truncate output and support `if_modified_since`
     conditional reads (skip unchanged files already in context).
   - Anti-re-read prompting baked into tool descriptions ("text you just
     wrote is text you already have"; "avoid the re-read spiral").
   - Their `recall` agent is Bash-only (Read/Grep/Glob/Edit disallowed) and
     is compaction-aware in its own right (`--since-last-compactions <N>`).

Notably: `/clear` gets nothing — the pipeline only fires on `compact`. Their
PreCompact/PostCompact hooks are pure telemetry (`compaction_started` /
`compaction_finished` events with duration/length counters).

## Side-by-side

| | wozcode | parsec (`/parsec:trim`) |
|---|---|---|
| Relationship to native compaction | Keeps it; patches its losses | Replaces it (injects on `clear`/`startup`, deliberately never on `compact` to avoid stacking on the native summary) |
| Selection signal | BM25F relevance to the compact summary's "next steps" | Backward-looking evidence: chunk's file/lines actually re-read later, or rare spans echoed in later commands (`needed.rs:419-497`) |
| Payload size | ≤900 chars of pointers | Full kept chunks verbatim (~10% of chunks on the reference session, ≈58k tokens) |
| Lossy remainder | Recoverable on demand via `recall --id` | Gone — no retrieval path back into the dropped transcript |
| LLM involvement | None | None in the binary; the STANDING DIRECTIVES block is written by the in-session model (the skill's job) |
| Persistent state | 15-min marker file only; the JSONL transcripts *are* the memory | Staged payload at `~/.parsec/trim/<project-key>.json`, 30-min TTL, one-shot consume-before-inject |
| Tunability | On/off toggle (`postCompactionDigest`) | Level knob 1–5; L3 is the parity-locked measured det arm |
| Evidence | None published in the plugin | SWE-bench Verified, 100 attempts/arm: det+dir 49/100 @ $1.62/task vs native /compact 47/100 @ $2.37 (not statistically separated; claim is match-or-beat at lower cost/variance) |

## Differences in kind (and what's worth borrowing)

- **Prediction direction.** Wozcode predicts *forward* — "given what the
  summary says comes next, which dropped turns are relevant?" Parsec labels
  *backward* — "which chunks does the rest of the session prove were
  needed?" Ours is evidence-based and parity-locked; theirs is a relevance
  guess, but a cheap one with a recovery path when the guess is wrong.
- **The recall escape hatch is the piece we don't have.** Their digest can
  afford to be tiny and wrong because every bullet is rehydratable. Our trim
  is all-or-nothing: if the needed-set missed something, the model can't get
  it back. If anything from their design is worth borrowing it's this — a
  pointer/retrieval layer over the dropped remainder would let our trim run
  at aggressive levels (L4/L5) with a safety net.
- **The directives idea has no counterpart on their side.** They rely on
  Claude Code's native summary to carry rulings/constraints; we extract them
  explicitly because (`DASEIN_TRIM_HANDOFF.md` §1b) decision rationale is
  spread across turns and no extractive trim can hold it. Their digest has
  the same blind spot the handoff identifies — BM25 over turn text can't
  recover a decision that lives in no single turn.
- **Prevention vs. cure overlap.** Their anti-re-read tool descriptions and
  `if_modified_since` caching cover the same territory as our no-reread
  hook — they prompt against re-reads and dedupe at the tool layer; we gate
  and count them.

## Wozcode source pointers

All under `wozcode-plugin/` (bundled, obfuscated; chunk hashes as of
2026-08-28):

- `hooks/hooks.json` — wiring
- `chunks/session-hook-start-Q4YGEIWE.js` — marks digest pending on
  `source === "compact"`, sweeps stale markers
- `chunks/chunk-YH7DA2VZ.js` — marker I/O, TTL, additionalContext merge
- `chunks/chunk-P73KWSTD.js` — settings/env gate
  (`WOZCODE_RECALL_POST_COMPACTION_DIGEST`), lazy import
- `chunks/claude-post-compaction-digest-FNWFW7GA.js` — the whole algorithm
  and constants (budget 900, 5 bullets × 120 chars, redundancy weight 0.6,
  query slice 1500)
- `chunks/session-hook-pretool-MOLQWV7K.js`,
  `chunks/session-hook-prompt-DI7IBVKK.js` — the two injection points
- `chunks/chunk-TEHRMIFV.js` — BM25F index + `compact_boundary` /
  `isCompactSummary` predicates
- `agents/recall.md` — the expansion path the digest points at
- `chunks/chunk-BFHGHFPD.js`, `chunks/tool-redirect-hook-IEYFOLGR.js` — the
  anti-re-read prompting + tool redirection layer
