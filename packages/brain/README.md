# parsec-brain

The scoring service the local proxy calls for keep/cut scores. One Python
process: a curator checkpoint (R-GCN, ~36 MB) plus the bge-large encoder,
served through the **vendored reference scoring path**
(`src/parsec_brain/vendored/` — the exact code the checkpoint was trained
under), so parity with the trainer forward is by construction. Design record:
the module docs in `src/parsec_brain/`.

Self-hostable by anyone. The base checkpoint is published on the Hugging Face
Hub; the training pipeline and the labeled trace corpus are not in this
repository.

## Self-host

You need a curator checkpoint. The released ones are on the Hugging Face
Hub at [huggingface.co/parsecai/curator](https://huggingface.co/parsecai/curator);
`curator_v7-9_10nn_prod.pt` is self-contained (no neighbor store at serve
time). Either download one to `~/.parsec/brain/curator_v4_prod.pt` (the
default `PARSEC_CKPT`) or point `PARSEC_CKPT` at the Hub:
`hf://parsecai/curator/<file>.pt` fetches and caches it via `huggingface_hub`.

**Docker (recommended):** from the repository root,

```sh
PARSEC_CKPT=hf://parsecai/curator/curator_v7-9_10nn_prod.pt docker compose up -d   # http://127.0.0.1:8090
curl -s http://127.0.0.1:8090/v1/bundle                         # checkpoint id, tau_q, contracts
```

**Bare process:**

```sh
cd packages/brain
python -m venv .venv && .venv/bin/pip install -e ".[embed]"
PARSEC_CKPT=hf://parsecai/curator/curator_v7-9_10nn_prod.pt \
  .venv/bin/hypercorn --bind 127.0.0.1:8090 'parsec_brain.app:create_app()'
```

Then tell the proxy where it is: `PARSEC_BRAIN_URL=http://127.0.0.1:8090`
(see the `PARSEC_*` doc comments in `packages/proxy/src/brain.rs` for the client side and `src/parsec_brain/app.py` for this side).

Embedding backends (`PARSEC_EMBED_BACKEND`):

| backend | what | when |
|---|---|---|
| `local` (default) | `BAAI/bge-large-en-v1.5` loaded in-process (`[embed]` extra; ~1.3 GB pulled from the Hub on first run, or baked into the image). CPU works; a GPU is faster. | the normal self-host |
| `remote` | the same encoder behind an HTTP service you run: `POST PARSEC_EMBED_URL {"model_id","texts"} → {"vectors"}` | the encoder lives on another box |
| `hash` | deterministic pseudo-vectors, no model | tests and wiring checks only — scores are **meaningless** |

The checkpoint was trained on bge-large vectors under the exact recipe in
`vendored/local_embed.py` (CLS pooling, L2 norm, 512 tokens, no query
instruction). Any other encoder silently shifts every score.

Auth, in order of precedence:

- `PARSEC_PLATFORM_URL` set → every request's bearer is a per-user `psc_` key
  validated against that platform's `/keys/validate` (`keyauth.py`; cached,
  fail-open-and-count unless `PARSEC_BRAIN_AUTH_STRICT=1`). This is the seam
  the hosted service uses; self-hosters normally leave it unset.
- else `PARSEC_BRAIN_KEY` set → a shared bearer secret on every `/v1/*` route
  (`/health` stays open). The proxy sends the same variable.
- else open. Fine on a private network; put it behind an authenticating
  proxy otherwise.

Checkpoints are `torch.load`ed with `weights_only=False` (the bundle carries
its calibration table and config alongside the state dict). Load checkpoints
only from sources you trust; the bundle's sha256 is reported as
`checkpoint_id` on every response so what you serve is always identifiable.

`cloudbuild.yaml` is a worked Google Cloud Run deploy (bakes the checkpoint
and the bge weights into the image so the image digest pins the bundle);
the Dockerfile itself is platform-neutral.

## Contracts

Two wire contracts, dispatched on the request `contract` field at the same
endpoints:

- **`brain-api/v1`** (`contracts/schemas/brain-api.schema.json`) — the
  data-plane-clean contract: client-computed vectors + structural features +
  opaque ids in, **raw text unrepresentable by construction** (every string
  field is a const, a hex id, or a bounded identifier; unknown fields
  rejected). The server rebuilds the trained graph topology from the payload
  (`v1graph.py`, reusing the vendored attach/edges functions over id
  stand-ins) and is pinned score-identical to the dev path by the cross-path
  parity gate (`tests/test_v1.py`: dev == v1 bit-exact on the golden
  conversations). The request `checkpoint_id` must match the loaded bundle —
  409 otherwise (features and weights move together, §8.2).
- **`brain-api-dev/v0`** (`contracts/schemas/brain-api-dev.schema.json`) —
  the internal message view, RAW TEXT. Only for a scoring host you operate
  yourself (below).

## ⚠️ dev-raw posture (v0 only)

The v0 contract (`brain-api-dev/v0`) sends the **internal message view — raw
conversation text — to this service**. That is a deliberate exception to the
"raw code/text never leaves the user's machine" rule, acceptable only when the
user also operates the scoring host. The proxy activates this path only when
`PARSEC_BRAIN_URL` is set **and** `PARSEC_BRAIN_DEV_RAW=1`. The v1 contract
carries no text and needs no opt-in.

## Bundle layout

The bundle self-validates at startup (`bundle.py`): dims derived from the state
weights must equal the ckpt's recorded config (hidden 384 / layers 3 / nrel 14 /
read_struct 49 / struct_dim 50 / emb_dim 3072), `chunk_mode=fixed` /
`chunk_lines=10` must match the engine chunker, `tau_calib` must resolve
`PARSEC_TARGET_COV` (string keys, nearest-key), and the rules roster must parse.
Any mismatch raises at load — the app never starts on a bad bundle.

| piece | where | tracked? |
|---|---|---|
| curator checkpoint (36MB `.pt`) | `PARSEC_CKPT`: a path (default `~/.parsec/brain/curator_v4_prod.pt`) or `hf://parsecai/curator/<file>`; baked into the image at deploy | no — out of the repo |
| `rules.json` (16-rule roster; `/v1/score/rules` default = the `active`+`always_on` subset) | `models/rules.json` (override: `PARSEC_RULES_JSON`) | yes |
| `changeprone.pkl` (readout col 42 sidecar) | `models/changeprone.pkl` (override: `AC_CHANGEPRONE_PKL`); absent ⇒ zero-filled col | yes |
| checkpoint identity | `checkpoint_id` = sha256 of the `.pt`, on every response + `/v1/bundle` | — |

Parity-critical env flags are pinned in `_flags.py` (imported before any
vendored module — the chunker reads `AC_CHUNK_MODE` at import time). The
effective snapshot is exposed on `/v1/bundle` and logged by the proxy into the
ledger row.

## Run (development)

Hermetic, no model download (deterministic hash embeddings — wiring only):

```sh
cd packages/brain
PARSEC_EMBED_BACKEND=hash PYTHONPATH=src \
  .venv/bin/hypercorn --bind 127.0.0.1:8080 'parsec_brain.app:create_app()'
```

Env: `PARSEC_CKPT`, `PARSEC_RULES_JSON`, `PARSEC_EMBED_BACKEND`
(`local` | `remote` | `hash`), `PARSEC_EMBED_URL` (remote), `PARSEC_TARGET_COV`
(default 0.70), `PARSEC_SERVE_TAU` (explicit operating-point override — the
reference's `AC_SERVE_TAU` dial, namespaced so `_flags` can keep popping stray
`AC_*`; forces deterministic cuts for demos and smoke tests), `PARSEC_BRAIN_KEY`
(optional bearer auth on `/v1/*`; `/health` stays open), `PARSEC_HOODS_PKL`
(hoods artifact path; **unset = neighbors OFF**, exactly the v0 serving;
set-but-missing/corrupt = refuse to start — train/serve-skew guard, no
auto-fetch), `PARSEC_NEIGHBORS` (runaway cost top-k, default 16),
`PARSEC_NEIGHBORS_X` (GNN block top-x, default 2 — the trained value, do not
change), `PARSEC_BRAIN_LOG` (level, default INFO), `PARSEC_BRAIN_LOG_JSON=1`
(JSON-lines logs). Logs never carry raw message/task text or tool schemas;
conversation ids ride as sha8 prefixes only.

## Endpoints

- `GET /health` → `{"status":"ok", "fail_opens": n, "requests": n}` —
  fail-open events are counted and alertable (CLAUDE.md)
- `GET /v1/bundle` → checkpoint id, resolved `tau_q`, grid (1e6), served
  `contracts` (`brain-api-dev/v0` + `brain-api/v1`), heads, `neighbors`
  (true when the hoods artifact is mounted, + `hoods_anchors` count),
  `doom` (`{gf, served}` — the ckpt's doom-head gf width), flag snapshot
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
  - Both accept an optional `gf` (the client-computed `loop_feats` 4-vector,
    each value in [-1, 1]) and then return `doom_q` = sigmoid(doom head) on
    the 1e-6 grid, pooled over the request's own chunk rows only — same
    forward, zero extra cost. No `gf` ⇒ no `doom_q` (v0 behavior).
  - Both return the same shape: per-row `scores_q` (non-mask rows exactly
    1000000) + constant calibrated `tau_q` + `checkpoint_id` + `timings_ms`
    (v1 `embed` is structurally 0.0 — the server embeds nothing). The two
    paths are bit-identical on the same conversation (`tests/test_v1.py`) —
    including with neighbors mounted and doom scored
    (`tests/test_neighbors_doom.py`). Client fails open on any error.
  - With `PARSEC_HOODS_PKL` mounted, the top-x=2 cross-trace neighbor blocks
    attach into the trace graph (assemble_trace order: after `attach_task`,
    before `attach_steps`; rels 5/6; **no serve-time dropout**). Anchor query:
    dev = the task text embedded via the request backend; v1 = the payload
    `task_emb`. Unset ⇒ nf=None, +3 zero block-parity cols, bit-identical to
    the committed goldens.
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
- `POST /v1/score/rules` → `{scores_q: {eid: int}, rules: [{eid, text, p_q,
  fire_step}], tau_hint_q, checkpoint_id, description}` — both contracts:
  - `brain-api-dev/v0`: internal messages + tools + `step`; `rules` omitted ⇒
    the bundle roster's `active`+`always_on` subset (shipped `rules.json`:
    `rule1 rule2 rule3 rule4 rule6 rule7`; candidate/retired eids are never
    scored unrequested). Scores each candidate's fire-probability at `step`
    through the vendored assemble path (`AC_RULE=on` scoped to the assemble
    call); a fire step with no step node (pure-reasoning turn) clamps to the
    nearest **earlier** step node, exactly as trained.
  - `brain-api/v1`: the trace-shaped graph payload (`nodes` = the tool-spec
    pipeline view, `task_emb`, `sys_emb?`, `edges_supersession`) + `cur_step`.
    The rule ROSTER and TEXT live server-side; the server embeds its own
    roster — returning that text is data-plane-clean (nothing of the user's
    leaves). Parity with dev is pinned (`tests/test_neighbors_doom.py`).
  - `scores_q: {}` (+ `rules: []`) = ineligible/failed ⇒ fail open (ungated
    fire is the baseline). `fire_step` echoes the requested step — the
    consumer's `(eid, fire_step)` dedupe key.
- `POST /v1/neighbors` → `{nbr_cost_median, nbr_count, neighbors_active,
  checkpoint_id}` — the runaway cost baseline, called ONCE per conversation
  (the neighbor set is per-task constant; the client caches it). dev body:
  `task_text`; v1 body: `task_vec` (client-embedded, 409 on checkpoint
  mismatch). Median = MEDIAN realized cost of the top-16 nearest anchors;
  `null` when hoods are off or <4 costs — the runaway signal stays inert.
- `POST /v1/score/gate` → `{score_q, tau_q: 500000, fire: score_q >= tau_q,
  fail_open, checkpoint_id}` for the codescout `brief` (vendored
  `attach_brief` + `brief_stats`; the scout-record scalars zero-fill at serve
  — the trained missing-value convention). Empty brief / empty tool roster /
  any error ⇒ `{score_q: 1000000, fire: true, fail_open: true}` — serve the
  brief ungated, measurably flagged.

**⚠️ rule/gate caveat:** neither head has a proxy consumer yet — these
endpoints exist for offline calibration and inspection. `tau_hint_q`
(150000, the handoff's ~0.15) is **advisory only**: rule scores are compressed
(ranking is the signal) and the firing τ must be bench-calibrated per
checkpoint before anything fires on it. Gate τ is the reference's locked 0.5.

## Tests

```sh
cd packages/brain && .venv/bin/pip install -e ".[test]" && .venv/bin/python -m pytest tests/
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
example against the served models. Most tests need a checkpoint at
`PARSEC_CKPT` (default `~/.parsec/brain/curator_v4_prod.pt`) and skip loudly
without one; the `no_ckpt`-marked suites always run, and that subset is what
CI runs. The committed goldens were generated against a specific checkpoint
(the one recorded in each fixture); a different checkpoint fails them by
design.
