# Cache continuity — invariants and guardrails

Remediation for the 2026-08-30 Desktop cache incident
(`docs/Parsec_Desktop_Cache_Incident_2026-08-30.docx`): parsec v0.2.6
repeatedly rewrote large already-warm prompt prefixes as cache WRITES
(≥10.76M excess write tokens in the audited three-hour window) because
conversation state was keyed by mutable head content, recreated on every
Desktop/Cowork head mutation, and re-seeded at the tail. This doc records
the invariants the fix enforces and where each lives.

## Invariants (all in `packages/proxy/src/server.rs` / `splice.rs`)

1. **Stable lane identity.** With a client session id, the conversation lane
   is keyed by `(session_id, model, source tool tag, first-user-content
   hash)` — never by the system/task head, which Desktop mutates per turn.
   The head hash remains only as the fallback for clients that send no
   session id (`conversation_id`). The first-user hash is what separates
   parallel subagents sharing one session and model.

2. **Per-lane serialization.** One async mutex per lane, held from before
   curation until past the post-2xx fingerprint commit (`lane_lock`,
   acquired in the `messages` handler). Concurrent same-lane requests
   serialize; different lanes never contend. This closes the
   clone-out/write-back race on the Freezer, fold map, and fingerprints.

3. **Client cache TTL preserved.** `place_cache_breakpoint` reads the ttl
   off the client's own `cache_control` markers before clearing them and
   stamps it on every marker it recreates (`splice::client_cache_ttl`).
   A subscription client's one-hour cache is never silently downgraded to
   the five-minute default.

4. **Roster byte-stability, monotonic only.** The tool keep-set freezes
   exactly once per lane and only ever grows (reactive unfreeze). Two
   v0.2.6 flip paths are closed: a scoring failure now freezes the FULL
   roster for the lane instead of retrying (no mid-run full↔pruned flip),
   and a forced `tool_choice` unfreezes the forced tool into the keep-set
   instead of swapping in the full roster for one call.

5. **Cache-loss guardrail.** A WARM lane (prior fingerprints exist) about
   to rewrite more than `PARSEC_CACHE_GUARD_TOKENS` (default 50 000, chars/4)
   of previously covered prefix fails open: the client's original body and
   anchors go upstream verbatim, the lane latches to passthrough
   (`ConvState::bypass`, cleared by the fresh-run reset), and the fire is
   counted separately (`cache_guard_count`). Cold lanes are exempt — a
   first sight may legitimately seed in full.

6. **Diagnostics.** Per request: separate system-head and first-user-head
   hashes (`sys_head_sha8`/`user_head_sha8` — makes "which head component
   mutated" answerable from the log, which the incident could not do),
   prior/frozen fingerprint counts and preserved ttl on the "cache
   breakpoint placed" line, and pre-prune vs served roster hashes
   (`tools_served_sha8`, also a ledger field).

## Regression coverage

- `server.rs` unit tests: lane survives a mutating system head; agents and
  models separate within a session; no-session fallback unchanged.
- `splice.rs` unit tests: ttl preserved / bare-ephemeral when absent / 1h
  beats 5m.
- `tests/proxy_brain.rs`: end-to-end head-mutation lane survival, scorer-
  failure full-roster freeze, forced-choice monotonic unfreeze, guardrail
  latch (original bytes verbatim + counted fail-open), and same-lane
  serialization across the upstream call (arrival-gap assertion).

## Not yet done (report §8 P1 remainder)

- Dashboard surfacing of cache reads/writes and guardrail fires beside
  saved tokens (`packages/frontend`). The ledger and counters now carry
  the data.
- A representative multi-agent Desktop replay as a release gate (report
  §9 last item); the integration tests cover the mechanisms, not a full
  replay.

Known accepted cost: two subagents launched with byte-identical prompts on
the same model still share a lane (as they did under the old identical-head
scheme); serialization bounds that to latency, never corruption.
