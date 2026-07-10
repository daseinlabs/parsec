# Brain serving v0 — inference-first design (2026-07-09)

Decision record for the first end-to-end real-scorer path (STATUS.md "REVISED
PLAN"): serve `curator_v4_prod.pt` behind `packages/brain`, wire the proxy's
`Freezer` to it, tool-prune, fail open everywhere.

## The boundary

The reference's `ContextCurator` mixes two things that this architecture
splits: the **decision engine** (birth schedule, registries, insist valve,
render) and the **scorer** (`_trace_scores`: graph build + GNN forward + tau).
The decision engine is `engine::freeze` (already ported, client-side). The
scorer becomes the brain service. `ChunkScorer` is the service boundary:

```
proxy (Rust, user machine)                 brain (Python, our GKE)
  to_internal → Freezer::serve               vendored curator scoring path
    └ BrainScorer::score(BirthQuery) ──────▶ /v1/score/trace  (chunks scored)
    └ tool keep-set (once per conv) ───────▶ /v1/score/tools  (schemas scored)
  scores_q/tau_q on the 1e-6 grid ◀────────  quantized, calib tau constant
```

## v0-dev posture: raw text rides to OUR cluster, by explicit opt-in

The target contract (§4, CLAUDE.md) is vectors+features with raw text
unrepresentable — that requires the client-side featurization port (49-col
readout struct incl. symbol-graph centrality, plus the local ONNX embedder).
Inference-first v0 instead sends the **internal message view** (raw text) to
the brain, which embeds via the in-cluster `dasein-embed` (bge-large) and runs
the exact vendored reference code. This is the same data-plane exception
STATUS.md already blesses for the dev embed fallback, extended to the scorer:
**fine for our own machines, never for real users.**

- Proxy side: the raw-text scorer only activates when `DASEIN_BRAIN_URL` is
  set AND `DASEIN_BRAIN_DEV_RAW=1` — nothing flows without the explicit flag.
- Contract split: `contracts/schemas/brain-api.schema.json` (v1 target,
  vectors+features, no raw text representable) is the commitment;
  `brain-api-dev.schema.json` (v0, what's implemented) is marked dev-only.
- Migration = swap the BrainScorer payload builder; the Freezer, tool prune,
  fail-open, ledger, and cache machinery are identical under both.

Why vendor rather than re-implement the scoring path: the graph contract has
~50 struct cols + 49 readout cols whose composition is env-flag-dependent
(AC_HUBPROPS **off**, AC_ISSUEMATCH on, scout/changeprone zero-fill, kNN over
content-emb only, R_HEAD_OBS actually head→command…). Every mismatch is a
silently-wrong-score bug (§8.2). The vendored path is the code that produced
the validated numbers; parity is by construction, pinned by a golden fixture.

## Scoring semantics (from the checkpoint + reference, verified)

- tau is **constant per (ckpt, target_cov)**: `tau_calib["0.70"] = 0.3152651`
  (raw-sigmoid scale). The budget head is overridden on calibrated ckpts; the
  Freezer's per-owner-pool tau calls are served from the BrainScorer's cache
  (no extra round trips). One HTTP call per birth step.
- Scores: raw `sigmoid(score_decided(...))` per masked (undecided) live chunk;
  non-masked rows are 1.0 (never cut). Quantized server-side:
  `q = round(score * 1_000_000)` (engine `SCORE_SCALE`), tau likewise.
- Chunker drift guard: the client sends per-chunk `(step, kind, tokens)`
  triples' hash; the brain re-parses the internal view with the vendored
  Python chunker and refuses to score on mismatch (409) — the cross-language
  parity invariant enforced live, per request.
- Neighbors (cross-trace blocks): **off in v0** (`nf=None`, +3 zero struct
  cols) — valid because training applied 20% block dropout; recorded in the
  bundle info. Doom/gate/rule heads: not scored in v0 (gate τ=0.5 locked,
  rule needs bench calibration — deferred per STATUS step 5).

## Scorer failure = per-step fail-open, not per-request

`ChunkScorer::score` now returns `Result`. On error the Freezer stops
replaying birth steps (this step and later stay undecided), still renders
prior decisions, and the serve succeeds; the proxy counts a scorer fail-open.
Because births are quantized to step boundaries, the next healthy call replays
the skipped steps exactly as if they had been current — brain downtime costs
savings, never correctness, and (unlike the reference, where chunks aged out
of the birth gate forever) the decision is only deferred.

The wire freeze (FoldMap) keeps already-served turns byte-identical
regardless — resident bytes never depend on brain availability.

## Tool prune (ported bug-for-bug, one guard added)

`prune(scores, tokens, names, target_cut=0.70)`: sort ascending by score, cut
until ≥ target_cut of roster token mass (chars/4 of the sort_keys schema
JSON), keep-set = names, **frozen once per conversation** (cache stability),
retry-until-success on scoring failure, new tools mid-conversation are
filtered out (reference behavior, known Glob-drop bug). Added guard: a request
whose `tool_choice` forces a specific tool is served the full roster for that
call (the reference could 400 upstream; only its force-spawn path guarded).

## Bundle self-validation (§8.2)

The brain refuses to start unless: dims derived from `state` weights ==
ckpt-recorded config (hidden 384, layers 3, nrel 14, read_struct 49,
struct_dim 50, emb_dim 3072 = 3×1024); `tau_calib` present and resolves the
target cov; `chunk_mode/chunk_lines` == engine defaults (fixed/10);
`rules.json` roster parses. `/v1/bundle` exposes checkpoint id (sha256 of the
.pt), resolved tau_q, grid, and the pinned env-flag snapshot; the proxy logs
it into the ledger row (capture seam).

## Config surface

Proxy: `DASEIN_BRAIN_URL`, `DASEIN_BRAIN_CONTRACT` (dev | v1; default dev),
`DASEIN_BRAIN_DEV_RAW=1` (explicit raw-text opt-in — dev contract only; v1
sends no raw text and needs none), `DASEIN_BRAIN_KEY` (bearer, optional),
`DASEIN_BRAIN_TIMEOUT_MS` (default 10000), `DASEIN_TARGET_COV` (default
0.70), `DASEIN_TOOL_PRUNE` (default on when brain configured),
`DASEIN_TOOL_CUT` (default 0.70), `DASEIN_FREEZE=off` escape hatch. v1
client embedder: `DASEIN_EMBED_BACKEND=hash|remote|onnx` (default hash —
test vectors, warns), `DASEIN_EMBED_URL` (remote), `DASEIN_ONNX_DIR` (onnx;
needs the `onnx` cargo feature).
Brain: `DASEIN_CKPT`, `DASEIN_RULES_JSON`, `DASEIN_EMBED_URL` (in-cluster
dasein-embed /embed; JSON path), `DASEIN_EMBED_BACKEND=hash` for hermetic
tests, `DASEIN_TARGET_COV`, `DASEIN_BRAIN_KEY` (optional bearer check).

## Deploy

House pattern (dasein-serving-engine): image
`us-central1-docker.pkg.dev/dasein-473321/dasein-images/dasein-brain:<sha>`,
ckpt + rules.json baked into the image (bundle version == image digest),
Deployment on the `serving` pool (nodeSelector dasein-role=serving +
toleration), ClusterIP `dasein-brain.default.svc.cluster.local:80→8080`,
`/health` probes, CPU inference (36MB R-GCN). Dev access: kubectl
port-forward. No LB.

## Capture seams designed in (not built)

Ledger row gains: `checkpoint_id`, `brain_ms`, `scorer_fail_opens`,
`freeze_cut_tokens`, `tools_total/tools_kept/tools_pre_prune_sha8`. These are
the TRAINING_CAPTURE_GAPS #1/#2 seams (pre-prune roster + served-cut
provenance) so the retrain contract can be honored later without a sidecar
retrofit; the telemetry pipeline itself stays unbuilt (§6 consent UX first).

## Appendix: v1 migration status — embedder parity gate (2026-07-10)

`scripts/embed/parity_gate.py` over a 14-text serve-prepped battery
(un-prefixed, chunk[:2000]/head[:240], 512-token truncation, CLS pool, L2):

- **fp32 ONNX export** (optimum, `scripts/embed/export_bge_onnx.py`) vs the
  **live dasein-embed** service (port-forward, fp16 weights): min cosine
  **0.9999957**, overall max|abs diff| **3.24e-04** — PASS (gate ≥ 0.9999).
- **Rust `engine::embed::OnnxEmbedder`** (feature `onnx`, ort 2.0.0-rc.12)
  vs python onnxruntime on the same export: min cosine **0.9999999**,
  max|abs diff| **7.82e-07** — PASS.
- **int8 dynamic quantization** (336MB vs 1.34GB fp32): min cosine
  **0.7309** (empty string; 0.93–0.97 typical) — fails the cosine gate
  outright; not shippable as-is, and any quant variant must additionally be
  validated on scores, not cosine.
- **HashEmbedder** is bit-equal to the vendored `_hash_embed`
  (`packages/engine/tests/parity_embed.rs`, fixture from
  `parity/gen_embed_fixtures.py`).

### Cross-contract E2E gate — dev == v1 on served bytes (2026-07-10): PASS

The proxy now speaks both contracts (`DASEIN_BRAIN_CONTRACT=dev|v1`, default
dev). In v1 mode the client featurizes where the text lives
(`proxy/src/featurize.rs`: local embedder from `DASEIN_EMBED_BACKEND=
hash|remote|onnx`, engine `node_struct_with_type`/`decided_struct`/
`supersession_edges`, salted 16-hex ids, the committed changeprone sidecar)
and handshakes `GET /v1/bundle` once per conversation for the checkpoint_id
(§8.2 pairing; unreachable ⇒ per-step fail-open, retried). No raw-text
opt-in is needed on v1 — the wire cannot represent text.

`scripts/parity_v1.sh` (hermetic; ONE local brain, hash embed both sides,
`DASEIN_SERVE_TAU` forced like golden_replay.sh) replays the committed
21-turn golden fixture through fresh proxies on each contract into separate
mock-upstream spools:

- forced tau 0.999: **all 21 forwarded turns byte-identical**, 0 fail-opens
  in both ledgers, `freeze_cut_tokens` 58276 == 58276.
- calibrated tau (`PARITY_TAU=""`, 0.3152651): **all 21 turns
  byte-identical**, 0 fail-opens, 48332 == 48332.
- fast per-query twin: `proxy/tests/proxy_brain_v1.rs::
  live_brain_dev_v1_score_parity` (#[ignore]; needs a running hash-backend
  brain) — one BirthQuery both ways, **scores_q exactly equal**, tau_q
  315265.

Supporting parity: `engine::features::supersession_edges` (the rel-4 port,
the one text-dependent edge relation) is pinned pair-for-pair INCLUDING
emission order against the vendored `pyg_model.edges` by
`engine/tests/parity_features.rs::parity_supersession_edges` (fixtures from
`parity/gen_supersession_fixtures.py`). The readout col-47 dupcos last-ulp
risk (sequential f32 vs numpy BLAS summation) did not surface on either
operating point — no score straddled tau across the epsilon.

Known scoped deviation (documented in featurize.rs): for a REPLAYED birth
step the v1 payload derives t/T/recent from the step being decided
(prefix-pure), while the dev brain re-derives them from the full message
list it re-parses; the two agree whenever the replayed step is the newest
one — every call of the sequential golden replay — and diverge only on
multi-step replays (post-fail-open retries), where v1 is the intended
semantics.
