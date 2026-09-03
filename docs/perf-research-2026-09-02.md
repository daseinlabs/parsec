# Performance research — 2026-09-02

Full-stack latency audit following the post-incident slowness reports.
Grounded in live proxy.log pipeline lines + three code sweeps (client brain
path, brain server, proxy lifecycle). Companion to docs/cache-continuity.md.

## 0. The headline number was wrong — telemetry bug

`brain_ms` in the ledger/pipeline log is a **lifetime accumulator per lane**
(`BrainScorer.stats.brain_ms` only ever `+=`, and `server.rs:1036` copies it
raw while every neighbouring stat subtracts a `*_before` snapshot; same bug
`openai.rs:310`). The "82s scorer average" was cumulative-since-lane-birth.
`curate_ms` is the honest per-request number — and neither `curate_ms` nor
`probe_ms` is written to the ledger at all (log-only, `server.rs:2344-2376`).

Measured reality (proxy.log, conv fb45af8505e9): healthy incremental turns
cost **~2-3s**; the slow turns are **full-replay storms** at 30-127s.

## 1. Full-replay storms — the dominant real cost (P0)

Steady state, one turn scores 1-3 new steps (~1.2-2.6s/call). Three
mechanisms turn one request into up to `turn`-many sequential brain calls:

1. **Purity-guard reset** (`freeze.rs:919-925`): ANY byte change in a
   consumed message (client context-editing/trim/compaction — including
   parsec's own trim) wipes `replayed_steps` and replays every step from 0.
   Log signature: `births_scored` jumping by ~`turn` with `folds_new` small.
   Observed repeatedly at turn 88-91 → 105-127s per request.
2. **Memo eviction / restart** (TTL 3600s, cap 512; in-process only):
   cold rebuild, `folds_new == folds_total`, observed 127s.
3. **Timeout backlog cascade** (`freeze.rs:934-941` + no retry in
   `brain.rs:442`): one failed step leaves it and all later steps
   unreplayed; each next request retries a growing backlog with a 10s
   timeout each, unbounded per request.

Fixes, in leverage order:
- **Score memoization across resets**: scores are pure functions of
  (step, live-set fingerprint) — widen the single-entry cache
  (`brain.rs:296`) to a per-lane map so a replay after reset is CPU-only,
  zero HTTP. Turns a 105s storm into milliseconds.
- **Suffix repair instead of full reset**: reset only from the first
  divergent message's step forward.
- **Per-request brain wall-clock budget** (`PARSEC_BRAIN_REQUEST_BUDGET_MS`)
  checked in the serve loop: degrade to fail-open instead of stacking
  timeouts.
- **Batch endpoint** (see §2.1) makes even a genuine cold replay one RTT.
- Persist memos to disk across proxy restarts (pure caches; determinism
  unaffected).

## 2. Brain server (P0/P1)

1. **No multi-step batch endpoint** — and the architecture already supports
   it: a readout mask only SELECTS rows on a shared graph
   (`trace_train.py:12-14`); `masks[]` + hoisting `_embed_nodes` out of the
   per-mask loop = graph+forward once for N steps.
2. **`edges()` is triple O(n²) pure Python** (`pyg_model.py:374-444`):
   ~4M-iteration loops ×3 at n=2000, ~3-5s per call under the lock — and
   the supersession block's output is discarded on the served path
   (`v1graph.py:97-99`). Vectorize with numpy masks + `argpartition`; skip
   rel-4 when discarded. Likely the single largest server CPU cost after
   the embedder.
3. **v2→v1 adapter shoves 6.1M floats through pydantic** per 2000-node
   request (`app.py:467-471`), three list↔numpy round trips of ~24MB.
   Keep embeddings as one (n,1024) float32 array end-to-end. No orjson
   anywhere.
4. **GNN hard-pinned to CPU** (`bundle.py:109` map_location, never
   `.to(device)`) even on GPU deploys; no `inference_mode`, no
   `set_num_threads` under cgroup limits; graph tensors rebuilt (~55MB
   copied 4×) every call. Model was designed for ≤70-node graphs
   (`pyg_model.py:504`), now run at ~4500 nodes ≈ 35 GFLOP on 4 vCPU.
5. **bge-large loads lazily inside the first request** of every autoscaled
   instance (`embedding.py:73-75`), under the global lock — several-to-tens
   of seconds. Warm it in `create_app()`. And the default deploy is
   `_GPU: 'false'` + `_EMBED_BACKEND: local` (`cloudbuild.yaml:32`) — fp32
   bge-large on 4 CPU cores.
6. **Global lock ~10× wider than needed** (`app.py:305`): serializes embed +
   assembly + forward across ALL endpoints; only the embed-cache touch
   needs it. Narrow it, then raise `--concurrency` above 1 (today the
   whole fleet serializes through one forward pipeline at 1 req/instance).

## 3. Client → brain transport (P1)

- **No compression, HTTP/1.1 only** (`Cargo.toml:30`: no gzip/zstd/http2
  features; reqwest never compresses request bodies). Payload ≈ 0.6-3KB per
  node → 0.5-2MB per POST, 5-10× compressible. Add zstd request encoding
  (+ server acceptance); shorten 64-hex salted ids to 16.
- **Fresh `reqwest::blocking::Client` + uncached `/v1/bundle` handshake per
  tools/rules call** (`brain.rs:607, 719, 770`) — new TCP+TLS each time;
  rules runs every turn when governor is on. Process-global client +
  checkpoint-id cache with 409 invalidation.
- **Client featurization recomputed in full per call, zero memoization**:
  `supersession_edges` is O(n²) with per-chunk regex span sets
  (`features.rs:422-455`); live corpus regex-tokenized ~3× per call
  (`readout.rs`). Chunks are immutable once born — memoize per-chunk rows
  and compare only newborns.

## 4. Proxy critical path (P1)

- **count_tokens probe is serial, inside the lane lock, before the
  forward** (`server.rs:2298-2309`) — a full extra Anthropic RTT with an
  OVERSIZED (uncurated) body, used only for the ledger. Measured
  probe_ms 0.15-1.1s. Spawn it concurrently; await at ledger time.
- **~5 full JSON serializations + 3 SHA-256 passes + ~8 deep clones of the
  whole conversation per request** (see agent table: to_internal twice,
  msg_hash, parse re-chunk, fold-map clone, apply_curation +
  place_cache_breakpoint each re-fingerprinting every message).
  Fuse the two splice passes; memoize per-message fingerprints.
- **Fold-map deep clone under the GLOBAL `convs` mutex**
  (`server.rs:1105-1109`) — a big conversation stalls every other lane.
  `mem::take` under the lane lock instead.
- **Lane lock span**: drop the guard right after the fps commit
  (`server.rs:2417`) so non-streaming responses + ledger writes don't hold
  the lane. `curate_ms` also excludes lane-lock queueing wait — add a
  queued_ms field.
- Small: SSE relay JSON-parses every delta line (`server.rs:2077`) —
  substring-gate first; ledger `create_dir_all` + credentials re-read per
  row.

## 5. Measurement fixes to do first

1. `brain_ms` per-request delta (`server.rs:1036`, `openai.rs:310`).
2. Write `curate_ms`, `probe_ms`, `upstream_ms`, lane-lock wait to the
   ledger row (today log-only) — the savings dashboard P1 needs them too.
3. Log a `reset_reason` when the purity guard fires (which message index
   diverged) — distinguishes client compaction from identity bugs.

## Expected end state

Healthy turn: 2-3s → **sub-1s** (probe off-path, transport compression,
server CPU fixes). Storm turn: 30-127s → **~2-5s** (score memoization +
suffix repair + batch endpoint). Fleet under parallel agents: no more
global serialization (narrow lock + concurrency > 1 + GPU default).

## Status (updated 2026-09-02, phase 2)

SHIPPED (phase 1+2, uncommitted): brain_ms delta fix; ledger stage timings;
purity-reset attribution; per-lane score memo + DISK PERSISTENCE
(PARSEC_SCORE_MEMO_PERSIST — restart/eviction replays now HTTP-free,
checkpoint-drift-purged); probe overlapped with upstream; gzip request
bodies gated on the /v1/bundle `accept_gzip` handshake (server ASGI
gunzip middleware); AC_EDGES_FAST vectorized rel-0/rel-2 twins
(bit-identity pinned by tests/test_edges_fast.py); rel-4 skip on serve;
embedder startup warm-up; PARSEC_TORCH_THREADS; inference_mode.

BATCH ENDPOINT — REJECTED BY DESIGN: step s+1's live set depends on step
s's drop decisions; iterating drops server-side moves freezing across the
determinism boundary (CLAUDE.md non-negotiable). The score memo +
persistence deliver the same win (replays HTTP-free) without moving the
boundary. Remaining next: pydantic/numpy adapter bypass, lock narrowing +
concurrency>1, GPU deploy default (billing decision).
Phase 3 (2026-09-02, later): pydantic V1Node/V1Tool bypass via model_construct + raw cache rows (kills the 6.1M-float revalidation); shared blocking client + url-keyed 60s checkpoint cache for tools/rules/neighbors (no more per-call TLS+handshake); fold-map mem::take instead of clone under the global convs mutex; lane lock released before non-streaming response buffering. SSE delta-gate skipped (LOW). Still open: lock narrowing + concurrency>1, GPU default, session affinity.
