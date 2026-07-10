# dasein-brain

Hosted GNN scoring API (control plane, private). Serves `curator_v4_prod.pt`
through the **vendored reference scoring path** (`src/dasein_brain/vendored/` —
the exact code that trained the checkpoint), so parity with the trainer forward
is by construction. Design record: `docs/brain-serving-v0.md`. Two wire
contracts, dispatched on the request `contract` field at the same endpoints:

- **`brain-api-dev/v0`** (`contracts/schemas/brain-api-dev.schema.json`) —
  the internal message view, RAW TEXT, our machines only (below).
- **`brain-api/v1`** (`contracts/schemas/brain-api.schema.json`) — the
  data-plane-clean target: client-computed vectors + structural features +
  opaque ids in, **raw text unrepresentable by construction** (every string
  field is a const, a hex id, or a bounded identifier; unknown fields
  rejected). The server rebuilds the trained graph topology from the payload
  (`v1graph.py`, reusing the vendored attach/edges functions over id
  stand-ins) and is pinned score-identical to the dev path by the cross-path
  parity gate (`tests/test_v1.py`: dev == v1 bit-exact on the golden
  conversations). The request `checkpoint_id` must match the loaded bundle —
  409 otherwise (features and weights move together, §8.2).

## ⚠️ dev-raw posture (v0 only)

The v0 contract (`brain-api-dev/v0`) sends the **internal message view — raw
conversation text — to this service**. That is a deliberate, dev-only exception
to the "raw code/text never leaves the user's machine" rule: fine for our own
machines, never for real users. The proxy only activates this path when
`DASEIN_BRAIN_URL` is set **and** `DASEIN_BRAIN_DEV_RAW=1`. The v1 contract
carries no text; the proxy migrates to it as the client-side featurization
port + local embedder land (the embedder parity gate is already green — see
the docs/brain-serving-v0.md appendix).

## Bundle layout

The bundle self-validates at startup (`bundle.py`): dims derived from the state
weights must equal the ckpt's recorded config (hidden 384 / layers 3 / nrel 14 /
read_struct 49 / struct_dim 50 / emb_dim 3072), `chunk_mode=fixed` /
`chunk_lines=10` must match the engine chunker, `tau_calib` must resolve
`DASEIN_TARGET_COV` (string keys, nearest-key), and the rules roster must parse.
Any mismatch raises at load — the app never starts on a bad bundle.

| piece | where | tracked? |
|---|---|---|
| `curator_v4_prod.pt` (36MB) | `~/.dasein/brain/curator_v4_prod.pt` (override: `DASEIN_CKPT`); baked into the image at deploy | no — out of the repo |
| `rules.json` (16-rule roster; `/v1/score/rules` default = the `active`+`always_on` subset) | `models/rules.json` (override: `DASEIN_RULES_JSON`) | yes |
| `changeprone.pkl` (readout col 42 sidecar) | `models/changeprone.pkl` (override: `AC_CHANGEPRONE_PKL`); absent ⇒ zero-filled col | yes |
| checkpoint identity | `checkpoint_id` = sha256 of the `.pt`, on every response + `/v1/bundle` | — |

Parity-critical env flags are pinned in `_flags.py` (imported before any
vendored module — the chunker reads `AC_CHUNK_MODE` at import time). The
effective snapshot is exposed on `/v1/bundle` and logged by the proxy into the
ledger row.

## Run

Local, hermetic (deterministic hash embeddings — no cluster access):

```sh
cd packages/brain
DASEIN_EMBED_BACKEND=hash PYTHONPATH=src \
  .venv/bin/uvicorn --factory dasein_brain.app:create_app --port 8080
```

Local against the real in-cluster embedder (bge-large, 1024-d), port-forwarded:

```sh
kubectl port-forward svc/dasein-embed 9090:80 &
DASEIN_EMBED_URL=http://127.0.0.1:9090/embed PYTHONPATH=src \
  .venv/bin/uvicorn --factory dasein_brain.app:create_app --port 8080
```

In-cluster the default `DASEIN_EMBED_URL`
(`http://dasein-embed.default.svc.cluster.local/embed`) needs no config. Scores
from the hash backend are **valid only for tests** — real serving requires the
bge vectors the ckpt was trained on.

Env: `DASEIN_CKPT`, `DASEIN_RULES_JSON`, `DASEIN_EMBED_URL`,
`DASEIN_EMBED_BACKEND=hash`, `DASEIN_TARGET_COV` (default 0.70),
`DASEIN_SERVE_TAU` (explicit operating-point override — the reference's
`AC_SERVE_TAU` dial, namespaced so `_flags` can keep popping stray `AC_*`;
used by `scripts/e2e_smoke.sh` to force deterministic cuts),
`DASEIN_BRAIN_KEY` (optional bearer auth on `/v1/*`; `/health` stays open).

## Endpoints

- `GET /health` → `{"status":"ok"}`
- `GET /v1/bundle` → checkpoint id, resolved `tau_q`, grid (1e6), served
  `contracts` (`brain-api-dev/v0` + `brain-api/v1`), heads, neighbors=false,
  flag snapshot
- `POST /v1/score/trace` — dispatches on the request `contract`:
  - `brain-api-dev/v0`: internal messages in; the server re-parses with the
    vendored chunker and refuses (**409**) when its sha256(`step:kind:tokens`…)
    disagrees with the client's `chunk_checksum` — the cross-language
    chunker-parity guard, live on every request.
  - `brain-api/v1`: `nodes` (per-chunk `emb_text`/`emb_cmd`/`emb_head` +
    21-col struct + step/kind/tokens + `file_id`/`lo`/`hi`/`cmd_id`/`head_id`
    opaque ids), `task_emb`, `sys_emb?`, `mask`, per-mask-row 49-col
    `decided_struct`, and `edges_supersession` (client-computed rel-4 pairs —
    the only text-dependent edge relation). **409** on `checkpoint_id`
    mismatch with the bundle; 422 on mask/width/edge-range drift.
  - Both return the same shape: per-row `scores_q` (non-mask rows exactly
    1000000) + constant calibrated `tau_q` + `checkpoint_id` + `timings_ms`
    (v1 `embed` is structurally 0.0 — the server embeds nothing). The two
    paths are bit-identical on the same conversation (`tests/test_v1.py`).
    Client fails open on any error.
- `POST /v1/score/tools` — same dispatch:
  - `brain-api-dev/v0`: internal messages + native tool defs.
  - `brain-api/v1`: `nodes` (tool-spec pipeline view), `task_emb`, `sys_emb?`,
    `tools: [{name, emb, tokens}]` — tool **names** ride in the clear
    deliberately: they are harness identifiers (the keep-set filter's join key),
    not user code.
  - Both → aligned `names`/`scores_q`/`tokens` for the deduped roster;
    `names: []` = ineligible/failed ⇒ serve the full roster (fail-open).
    `prune()` itself (rank-to-target 0.70, keep-set frozen per conversation) is
    ported client-side.
- `POST /v1/score/rules` → `{scores_q: {eid: int}, tau_hint_q, checkpoint_id,
  description}`. Scores each candidate rule's fire-probability at `step`
  through the vendored assemble path (`AC_RULE=on` scoped to the assemble
  call); a fire step with no step node (pure-reasoning turn) clamps to the
  nearest **earlier** step node, exactly as trained. `rules` omitted ⇒ the
  bundle roster's `active`+`always_on` subset (shipped `rules.json`: `rule1
  rule2 rule3 rule4 rule6 rule7`; candidate/retired eids are never scored
  unrequested). `scores_q: {}` = ineligible/failed ⇒ fail open (ungated fire
  is the baseline).
- `POST /v1/score/gate` → `{score_q, tau_q: 500000, fire: score_q >= tau_q,
  fail_open, checkpoint_id}` for the codescout `brief` (vendored
  `attach_brief` + `brief_stats`; the scout-record scalars zero-fill at serve
  — the trained missing-value convention). Empty brief / empty tool roster /
  any error ⇒ `{score_q: 1000000, fire: true, fail_open: true}` — serve the
  brief ungated, measurably flagged.

**⚠️ rule/gate caveat:** neither head has a proxy consumer yet — these
endpoints exist for bench calibration and dogfood inspection. `tau_hint_q`
(150000, the handoff's ~0.15) is **advisory only**: rule scores are compressed
(ranking is the signal) and the firing τ must be bench-calibrated per
checkpoint before anything fires on it. Gate τ is the reference's locked 0.5.

## Tests

```sh
cd packages/brain && .venv/bin/python -m pytest tests/
```

Hermetic (hash backend). The golden test pins exact quantized outputs of the
full scoring path on a handcrafted conversation, twice from cold — regenerate
after a deliberate change with `.venv/bin/python tests/test_service.py`.
`tests/test_v1.py` is the cross-contract parity gate: it featurizes
conversations into `brain-api/v1` payloads **using the vendored code itself**
(parse → chunks → node struct → readout → hash embeddings → supersession
pairs) and asserts the v1 endpoints return exactly the dev endpoints' ints on
the handcrafted golden AND on turns of the proxy's recorded
`golden_conversation.json`; it also validates the committed v1 schema +
example against the served models. Requires the ckpt at
`~/.dasein/brain/curator_v4_prod.pt` (tests skip loudly without it).
