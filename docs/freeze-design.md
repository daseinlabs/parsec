# Deterministic quantized freezing — design (2026-07-09; implemented in engine::freeze)

Replaces the reference stack's session-resident freeze state (DIRECTION.md §4
decision 2). Reference behavior being replaced (`adaptive-context-clean`):

- `service/anthropic_shapes.py::apply_curation` — `served_folds` dict in
  `sess.extra`: `"{index}:{sha256(raw content)}" -> exact served bytes`,
  frozen at first wire appearance ("the cache law": a turn's upstream bytes
  are decided exactly once).
- `adaptive_context/optimizer/curator.py` — decision schedule v3: per-chunk
  decisions at the message's birth call, "decide at birth or never"
  (curator.py:1023), with `decided_msgs` / `dropped` / `frozen` session dicts.
- `service/curating_proxy.py::_cache_refreeze` — v9 hard-kill refreeze.

**The trap**: all of it lives in an in-process session keyed by conversation
id with TTL eviction. Restart, TTL, or a second replica loses the dicts; the
next call re-decides with the curator's *current* opinions and serves
different bytes for the same prefix — cache bust at best, silent divergence
at worst.

## The rule

> Served bytes for message j are a pure function of
> **(messages[0..=j], checkpoint bundle, config)**.

No session dict is ever the source of truth. Any store is a transparent cache
of this function; losing it changes latency, never bytes.

Three shifts against the reference make this hold:

1. **Birth quantized to message boundaries, not call boundaries.** The
   reference decides a message at its birth *call*; which messages share a
   call depends on client batching — unrecoverable from the prefix alone. We
   decide message j against the chunk context of messages 0..=j only. (The
   only behavioral delta: two messages born in one call no longer see each
   other's chunks at decision time — strictly more conservative.)
2. **Quantized scores.** Keep/cut compares fixed-point integers:
   `q = round_half_even(score * 1000)` vs `tau_q`, computed server-side in the
   brain and returned as integers. Float jitter across replicas/hardware can
   never flip a decision; purity survives the network hop because (vectors,
   checkpoint) -> q is deterministic and re-queryable.
3. **Append-only by construction.** The decision function for j cannot read
   anything after j, so replaying a longer prefix reproduces every earlier
   message's bytes byte-identically. This is CI invariant §8.1, tested as:
   serve prefixes of a recorded conversation incrementally, then re-serve
   from a cold start at each length — all previously-served turns must be
   byte-identical in both runs.

## Layering

- `engine::freeze` (wire-agnostic core):
  - `ChunkScorer` trait — quantized scores for a message's chunks given the
    accumulated context; implementations: brain-API client (proxy side,
    `BrainScorer`), `PassthroughScorer` (fail-open floor: keep everything),
    deterministic `StubScorer` (tests). Since the inference-first wiring
    (docs/brain-serving-v0.md) `score` returns `Result<ScoreResult,
    ScoreError>`: a scorer failure leaves the birth step UNDECIDED (step-
    atomic commits, `scorer_fail_opens` counted) and it is retried on a
    later serve — recovery is byte-identical to a cold replay.
  - `decide_message(j, steps, scorer, config) -> MessageDecision` — pure.
  - `serve_conversation(steps, scorer, config)` — folds decisions over a
    prefix; the golden-test surface.
- `proxy::splice` (already ported): fold-back of curated text onto original
  Anthropic blocks; `(index, fingerprint)` keying stays for client-edit
  detection — an edited turn is a *different* input to the pure function,
  decided fresh for the new bytes.
- The ledger (`FoldMap`) becomes an optional memo: consulted for speed,
  reconstructible at any moment. A replica or restart recomputes and MUST
  arrive at the same bytes (asserted in tests, alertable in prod via the
  §8.3 fail-open metric family).

## Reference semantics reproduced (verified in source, 2026-07-09)

The reference's live path is PURE-T0 birth-only (curator.py:1114-1118): the
age-1-kill and epoch-compaction channels in the module docstring are dead
code; the only decision channel is the birth gate `c.step == cur_step`
(curator.py:1080). engine::freeze reproduces, byte-for-byte (proven by
tests/parity_freeze.rs driving the REAL curate() with an identical stub
scorer):

- parse: step assignment (task consumes step 0, never chunked), last_cmd
  from `extra.actions`, reasoning/assistant/observation chunking, stable
  (step, reasoning-last) node ordering (trainer parity).
- birth admission: insist valve (`_insisted`/`_narrow`, the 0.5-overlap and
  120-char fp rules), `_budget_cut` (score-ascending stable sort, stop at
  tau, contiguous same-owner runs >= 10 tokens), per-owner-message decisions
  with per-pool qhat when >1 message is born in a step.
- registries: `dropped` (60-char lossy ckeys), dropped/served ranges + fps,
  in exact commit order.
- render: original bytes when untouched; provider-reasoning strip; full-drop
  digest (returncode line survives; first-2/last-1 body lines at 300 chars;
  `[... N lines (~T tokens) · re-read f:Llo-hi omitted ...]`); partial
  grouped elisions (`[... ~N tokens · re-read f:Llo-hi omitted ...]`);
  returncode re-injection.

## Additional deviations (adversarial review, 2026-07-09)

An adversarially-verified review of the port surfaced these; each is either
fixed in code or recorded here as a deliberate deviation (full list also in
the freeze.rs module doc):

- **Render is call-history-free** (deviation): the reference re-serves the
  untouched ORIGINAL bytes of an already-decided message on a repeat call at
  the same cur_step (curator.py:1154), so an identical-request retry
  flip-flops digest→original→digest across calls. The port always renders
  the frozen form — a retry serves the same bytes.
- **Fail-open on malformed values** (fixed): non-string `text` in a content
  part / truthy non-string action command raise a TypeError out of the
  reference's curate() (fail-open, no state advance); serve() now returns
  `FreezeError::Invalid` before any state advances instead of silently
  coercing to "".
- **Purity guard on edited history** (fixed): a client-edited resident turn
  now resets the fold memo so warm == cold (the reference silently kept
  stale decisions).
- **Empty-string basenames are Python-falsy** (fixed): "src/:5:…" grep lines
  carry file="" which every reference truthiness gate skips; ported guards
  now match (insist valve, range registries, span pointers, dup features).
- **CPython number repr** (fixed): fingerprint/json-dumps bytes now use
  CPython float repr (scientific iff exp < -4 or >= 16, signed 2-digit
  exponent, ".0" suffix) with serde_json's float_roundtrip; integers beyond
  i64/u64 remain unrepresentable (documented residual).
- **Python `\s` includes \x1c-\x1f** (fixed in all ported regexes);
  **Python `\w`/`\b` count No/Nl numerics and exclude combining marks**
  (deviation — pathological unicode adjacent to command keywords only).
- **Line numbers beyond i64 saturate** (deviation; Python ints are unbounded).

## Reference behaviors deliberately NOT ported into the pure core

- **Governor directive injection into resident turns** (governor.py:684-760
  appends adjudicator/wall-clock/cost-derived text to the newest message
  before fold): contaminates frozen bytes with non-recomputable inputs
  (Haiku verdicts, `time.time()`, cache-hit-dependent cost). Directives must
  ride the moving tail (appended user turn, like noreread) — proxy-layer
  work, tracked for the governor port.
- **count_tokens statefulness** (curating_proxy.py:723 advances curator birth
  state without wire-freeze writes): a probe request cannot change decisions
  here — there is no state to advance.
- **Explore-reset stubs** (curator.py:787-795): the reference map proved they
  never reach the wire on the sessioned path (served_folds replays before the
  curator's opinion is consulted) while still being credited as savings by
  `removed_mass` — a billing-honesty bug, not a feature. Excluded.
- **The v9 kill refreeze** (`_cache_refreeze`): its output is discarded on the
  live proxy path (finalize turns are synthesized, never forwarded). Not
  ported until some path demonstrably forwards kill turns.

## Open items

- Embedder determinism: scores are deterministic only if vectors are; the
  ONNX embedder must be pinned + deterministic (no nondeterministic kernels)
  — part of the embedder-retrain track. Until then the brain API must
  quantize server-side and treat (checkpoint, embedder, grammar versions) as
  one matched artifact.
- Client-edit cascade: an edit at turn k changes prefixes for all j >= k;
  decisions recompute against the new prefix (correct), but the provider
  cache for the tail is lost regardless (same as reference).
- Proxy glue: wiring Freezer + splice::apply_curation into `parsec proxy`,
  with the FoldMap as a pure memo and the (index, fingerprint) key kept for
  client-edit detection; breakpoint placement (place_cache_breakpoint port)
  and its `last_msg_fps` — note the reference writes fps before the upstream
  call succeeds (anthropic_shapes.py:484), an ordering bug to fix in the
  port, and mutates fold-aliased dicts on string→block promotion
  (anthropic_shapes.py:520) — the Rust proxy must store canonical
  cache_control-free content and apply placement at serialize time.
