# Server-side embedding — direction change + work plan

**Status:** design doc, 2026-07-20. `DIRECTION.md` §1, §2, §4, §5, §7, §7b, §9, and §10 have
been revised to match, as has the `CLAUDE.md` non-negotiables list. This doc is the detail
behind those edits.

**Deliberately deferred** (per user, 2026-07-20): the Free/Pro consent surface and the
tiered-contract split in §3 below. The near-term goal is getting plugin + proxy + brain running
end-to-end. §3 is retained as the record of what the change costs, not as an active work item.

---

## 1. The change in one paragraph

The embedder moves from the user's machine into the brain, in-process, on a GPU. The proxy
stops shipping a 1.3GB ONNX model and stops running ORT; it sends chunk **text** plus
structural features to the brain and gets back scores. The brain hosts `bge-large-en-v1.5`
itself rather than proxying to an external `dasein-embed` service, so the request path becomes
`proxy → brain → proxy` with no second hop. Serving stays **single-threaded per process**;
concurrency comes from running N brain processes behind an nginx layer, not from making one
process concurrent.

## 2. Why

Three things forced this, in order of weight:

1. **The local embedder does not fit on small machines.** `DIRECTION.md` §10 (2026-07-18):
   fp32 bge-large needs a ~3–4GB init peak at ORT's default graph-opt level; a small Windows
   VM hit "bad allocation" at session init. The proxy fails open correctly, but the warm-up
   failure is cached per-process in a `OnceLock` (`packages/proxy/src/brain.rs:216-218`), so
   v1 scoring stays off until restart — **silent zero savings**.
2. **It kills the Pro long pole.** `DIRECTION.md` §9 step 3 is "pick the local ONNX embedder,
   retrain the curator against its vectors." The current checkpoints are already matched to
   dasein-embed `bge-large-en-v1.5` vectors. Serving those vectors from our own GPU means
   **the central retrain disappears entirely**.
3. **Install weight.** ~1,128 lines of download machinery (`packages/proxy/src/setup.rs`)
   plus the `ort`/`tokenizers` dependency and its Windows CRT workaround
   (`packages/engine/Cargo.toml:31-36`) stop existing on the client.

## 3. What this costs — the privacy posture

Embedding requires the text. This directly contradicts two standing non-negotiables:

> **CLAUDE.md:** "Raw code/text never leaves the user's machine. The brain API contract must be
> unable to represent raw text."
>
> **DIRECTION.md §2:** "The local proxy sends chunk vectors + structural features, never raw text."

The v1 schema enforces that *structurally* — every string field is a const, enum, fixed-length
hex, or bounded ident, and `packages/brain/tests/test_v1.py:419`
(`test_v1_no_raw_text_representable`) asserts it. This is not a convention that can be quietly
relaxed; it is the claim behind the Team zero-retention posture and the Enterprise "learns your
codebase, never sends us a byte" close.

**Resolution: tier the contract.** The repo already carries two contracts, which is the
bifurcation mechanism:

| Tier | Contract | Rationale |
|---|---|---|
| Free / Pro | text-carrying (§4) | perf + zero install weight; explicit consent at signup |
| Team / Enterprise / self-host | `brain-api/v1` textless, or an on-prem brain | the privacy claim stays literally true where it is sold |

The engine must therefore keep **both** client paths working. This is not throwaway
compatibility code — it is the product boundary.

## 4. The contract: v1-with-text, not v0

There are two ways to send text, and the obvious one is wrong.

`brain-api-dev/v0` already exists and already works: the client posts raw `messages`, and the
brain chunks and embeds server-side (`app.py:336`, dev branch `:341-381`). **Do not promote
it.** The two contracts have an inverted risk profile:

| | v0 (raw messages) | v1 (vectors) |
|---|---|---|
| Server re-chunks the messages | Yes — chunker parity enforced live, 409 on drift | No |
| `checkpoint_id` 409 matched-pair guard | **Absent** | Present (`app.py:270-278`) |
| Bundle handshake used by proxy | No (`brain.rs:375-383`) | Yes |

Two problems with v0:

- **It drops the matched-pair guard**, which is `DIRECTION.md` §8.2 — *"the checkpoint/env
  matched pair trap — silent wrong scores — dies by making bundles self-validating."* Dev
  requests carry no `checkpoint_id` at all. A dev 409 is not even specially handled proxy-side;
  it falls through to generic per-step fail-open, because the reset is gated on
  `contract == V1` (`brain.rs:519`).
- **It makes the brain re-chunk**, so the Rust chunker must stay byte-in-lockstep with the
  Python chunker, enforced on every single request via `chunk_checksum` (formula:
  `scorer.py:70-75` ↔ `packages/engine/src/freeze.rs:309-321`; verified `app.py:342-352`).
  That is *more* cross-language coupling than v1, not less, plus duplicated CPU work on the
  server.

**Build a third contract instead: `brain-api/v1` with three fields swapped.** Replace
`emb_text` / `emb_cmd` / `emb_head` (`app.py:95-97`) with `text` / `cmd` / `head` strings.
Everything else is unchanged — struct rows, `decided_struct`, supersession edges, salted opaque
ids, and the `checkpoint_id` guard.

- **Keeps:** the 409 matched-pair guard, no server-side re-chunking, and all the client-side
  featurization that is already written and parity-tested.
- **Gains:** server-side embedding.
- **Loses:** nothing.

Brain-side this is a small insertion: embed the three text fields, then feed the **existing**
`v1graph` path that already consumes vectors at `packages/brain/src/dasein_brain/v1graph.py:78-80`.
Featurization, graph build, and readout are untouched.

### Feature parity is already done

The audit found the text path is a **superset** of v1, not a subset. Trace, tools, rules,
neighbors, and doom/`gf` all have working dev paths on both server and proxy. The gate head is
in fact **v0-only** — `ScoreGateRequest.contract` is `Literal["brain-api-dev/v0"]`
(`app.py:162-167`), with no v1 equivalent. (It has no proxy consumer today either way —
`app.py:498`: *"no proxy consumer yet"*.)

So "full feature parity" is not a build item. The new contract inherits it.

## 5. The embedder: onnxruntime-gpu, not sentence-transformers

The swap point is clean. `TraceScorer.__init__` builds `EmbeddingClient(cfg, backend)` from
`DASEIN_EMBED_BACKEND` (`scorer.py:158-160`), and everything funnels through one method,
`_embed()` (`scorer.py:178-186`). Add a backend branch at
`packages/brain/src/dasein_brain/vendored/embedding.py:54-62`, lazy-imported (mirroring how the
`dasein` branch does it at `:56`), preserving the
`embed(texts, as_query=False) -> list[list[float]]` contract. No call-site changes.

**DECIDED 2026-07-21 — implemented with `transformers`, not ORT** (reversing the
recommendation below). `vendored/local_embed.py` loads bge-large via
`transformers.AutoModel` in-process (`DASEIN_EMBED_BACKEND=local`). The ORT
concern — that adding transformers pressures the `torch>=2.2,<2.3` pin and
drifts the curator — did **not** materialize: `transformers>=4.40,<5` resolves
against the existing torch 2.2.2 with no upgrade, and all golden/parity tests
pass unchanged. Chosen because the cluster embedder *is* a SentenceTransformer
of the same model, so transformers-with-the-same-model matches by construction,
and it avoids reimplementing tokenize+pool+normalize around an ONNX session.
Cost accepted: `transformers` in the image, and the `parity_gate.py`/ONNX export
below are not on this path (a one-off cosine check vs the `dasein` endpoint
still gates prod — see `docs/deploy-cloud-run.md`).

<details><summary>Superseded recommendation (ORT)</summary>

Use `onnxruntime-gpu`, not sentence-transformers. `pyproject.toml` pins
`torch>=2.2,<2.3` because the vendored GNN reference path was validated against that line.
Pulling in transformers pressures that pin and risks silent score drift on the curator. ORT
decouples the embedder from torch entirely, and it reuses the export
(`scripts/embed/export_bge_onnx.py`) and parity gate (`scripts/embed/parity_gate.py`) already
built for the client — that work relocates rather than being discarded.
</details>

### Text-prep contract — IMPLEMENTED in `local_embed.py`

Character truncation happens brain-side already (`[:2000]` chunk/task/sys, `[:240]` heads —
`scorer.py:236-253`). The remaining three — **512-token truncation, CLS pooling, and L2
normalization** — used to happen only inside the remote pod; the in-process backend now does
all three itself (`local_embed.py`: `max_length=512`, `last_hidden_state[:, 0]`,
`F.normalize`). `as_query` is a no-op — **no bge query prefix, ever** (`parity_gate.py:17-19`;
a prefix would break checkpoint match).

Rule texts are the deliberate exception: untruncated, because the raw text is the assemble
cache key (`scorer.py:502`, `app.py:433-435`).

### Baked-in assumptions

1024-d is asserted in five places — `scorer.py:55`, `v1graph.py:40`, `neighbors.py:39`
(hard-enforced on the hoods artifact at `:76-78`, `:96-97`), `app.py:89` (pydantic
`Vec1024`, the strictest gate), `app.py:486`. The tri-embedding invariant `xe = [emb ‖ ace ‖ hde]`
makes the checkpoint's `emb_dim` 3×1024 = 3072, asserted at `bundle.py:147-150`.

### Acceptance gate

Run `scripts/embed/parity_gate.py` with a new arm for the in-process embedder against the live
dasein-embed service: **cosine ≥ 0.9999 per text**. Then, per the gate's own note at
`:14-15`, validate on **scores, not cosine**, before shipping.

## 6. Concurrency: replicate processes, don't thread

Serving stays single-threaded for now. **The scaling answer is N single-threaded brain
processes behind nginx, not an in-process concurrency refactor.**

Why this works: `DIRECTION.md` §4.2 requires served bytes to be a pure function of
(conversation prefix, checkpoint, config) precisely so the engine runs "as a laptop process or
a replicated cloud service **without sticky routing**." `TraceScorer.cache` and `_nscaches`
are pure performance — a cold process returns byte-identical results. So **plain round-robin
is safe**; no session affinity, no consistent hashing.

- Constraint is GPU memory, not correctness: bge-large at fp16 is ~700MB, so several processes
  fit on one L4. Cache hit rate drops across processes; nothing else changes.
- A useful consequence: the `AC_RULE` / `AC_GATE` `os.environ` mutation around `assemble_trace`
  (`scorer.py:510-514`, `:560-564`) is process-global state that is only safe because of the
  wide lock. Under process replication it never detonates. **Do not pay that refactor.**

### What to measure before sizing

Throughput per process is `1 / (embed_ms + forward_ms)`, fully serialized by the single
`threading.Lock` at `app.py:210`. Note that **embedding gets much bigger under the text
contract**: today the lock wraps embed only for rule text (`app.py:435`) and neighbors
(`app.py:483`); under the new contract it wraps every chunk in the turn (`scorer.py:238`
embeds all chunk texts, `:243` cmds and heads). A turn with 50–200 chunks is a real batch.

`_embed_ms` and `_forward_ms` are already instrumented (`scorer.py:166`, `:183`). **Get them
into the per-request log before launch.** There is no percentile aggregation and no `/metrics`
endpoint anywhere today.

### GPU scope

The brain has **no device selection at all** — CPU is hardcoded in three places:
`bundle.py:109` (`map_location="cpu"`), `scorer.py:463,518,571` (`"cpu"` literal to
`collate_traces`), and bare `torch.from_numpy` with no `.to(device)` in `v1graph.py` /
`scorer.py:263-283`. The plumbing exists upstream (`vendored/trace_train.py:190-208` threads a
`dev` arg).

**The GPU is for the embedder only.** The GNN is a 36MB R-GCN and CPU forward is fine. Be
deliberate about this — do not pay for a GPU the inference path never touches, and do not
assume "the brain has a GPU" speeds up scoring.

## 7. What stays on the client

**~1,500 of 13,451 lines move. The other ~89% stays.** The proxy remains a curation engine
that asks a remote service for scores — not a pipe.

Moving off: the ONNX arm of `packages/engine/src/embed.rs` (~350 of 560) and most of
`packages/proxy/src/setup.rs` (1,128).

Staying, and why:

- **The data plane.** Model traffic goes to `api.anthropic.com` with the user's own credentials
  (`forward_auth_headers`, `server.rs:438-462`). Subscription OAuth tokens are only legitimate
  as the user's own local traffic. Immovable at any tier.
- **Chunking.** The freezer must chunk to know what to drop and render (`freeze.rs:284-330`).
  Required regardless of contract.
- **Decision application.** The brain returns *scores*, not decisions. `budget_cut` applies tau
  plus the coverage budget (`freeze.rs:479-511`); commit at `:619-646`; the insist valve at
  `:545-547`; digest/omission rendering at `:659`, `:723`.
- **Determinism and cache stability.** The `Freezer` memo (`freeze.rs:335-355`) and the purity
  guard that forces cold replay when a consumed message changes (`:831-838`). **This is what
  makes brain replicas stateless and round-robin-safe** — see §6.
- **Wire surgery.** `splice::apply_curation` (`splice.rs:222`), cache breakpoint placement
  (`:402`), and the 2xx-gated fingerprint commit so a 429 does not burn state on a request the
  model never saw (`server.rs:1881-1927`).
- **Measurement.** The `count_tokens` counterfactual (`server.rs:1330-1407`) runs against
  Anthropic with the user's credentials. The one honest savings number is computed locally.
- **Tool prune application.** `prune` (`brain.rs:957`), `stub_tool` (`:1011`), reactive unfreeze
  (`server.rs:960-984`), forced-`tool_choice` guard (`:985-992`).
- **Fail-open.** Ten distinct local paths. Brain downtime costs savings, never correctness.
- **Streaming.** Byte-unmodified SSE tee with usage scan (`server.rs:1959-1996`).
- **Never touches the brain at all:** `governor.rs` (1,150), `noreread.rs` (727),
  `adjudicator.rs` (591), `statusline.rs`, and `mapgen`.

⚠️ **Do not move freezing server-side later** "since the text is already there." That trade
costs sticky routing and restart-safety, and breaks the round-robin property in §6.

## 8. Work plan

1. **Contract** — add the text-carrying variant alongside v1; keep `extra="forbid"` and the
   `checkpoint_id` guard. Extend `test_v1.py`'s parity harness (which already asserts
   `dev == v1` scores across handcrafted, golden-conversation, alive-subset, and supersession
   cases, `:274-305`) with the new contract as a third arm.
2. **In-process embedder** — §5. Backend branch, text-prep reimplementation, parity gate arm.
3. **Interlocks** — the current ones actively prevent shipping this and must be reworked
   deliberately:
   - `DASEIN_BRAIN_DEV_RAW=1` required or the brain stays off (`brain.rs:130-140`)
   - a baked release URL forces v1, so a shipped binary can never speak raw text
     (`brain.rs:113`)
   - the startup log announcing an *"our-machines-only posture"* (`server.rs:274-278`)
   - make an unknown `DASEIN_EMBED_BACKEND` **fatal** — it currently falls through to `hash`
     (`embedding.py:61`), which serves confident garbage. The proxy already guards against this
     (`brain.rs:149-167`); the brain does not.
4. **Throttling + keys** — deploy `packages/platform`, have the brain call `/keys/validate`,
   add per-key quota + 429/`Retry-After`, and proxy-side backoff. Independently urgent: the
   live brain is **open by default** if `DASEIN_BRAIN_KEY` is unset (`app.py:211-220`), and it
   is baked into every released binary (`.github/workflows/release.yml:79`).
5. **nginx** — N single-threaded brain processes, round-robin, no stickiness.
6. **CI** — `scripts/parity_v1.sh` and `scripts/golden_replay.sh` are local-only today, and the
   brain's 53 tests are not in CI at all (`ci.yml` runs only Rust + `packages/platform`). With
   the contract changing underneath, this is the gate you most want running.

## 9. Fix while in there

- **`TraceScorer.cache` is unbounded** (`scorer.py:161`) — keyed on exact raw text,
  process-lifetime, shared across conversations. Its sibling `_nscaches` is LRU-bounded at 64
  (`scorer.py:56`). Under raw-text traffic this is both a memory leak and indefinite retention
  of raw customer code in process memory, which cuts against the retention posture being sold.
  Give it the same LRU treatment — far harder to retrofit later.
- **`scorer_fail_opens` is not aggregated process-wide.** It survives `Freezer::reset()`
  (`freeze.rs:351-355`) and reaches the ledger per-row, but never lands in an `AppState` atomic,
  so there is no process-wide brain-failure tally in the shutdown line. `DIRECTION.md` §8.3
  requires fail-open rate to be a first-class alertable metric; today it is not.

## 10. Open questions

- Deployment target for a GPU monolith: Cloud Run with L4 (keeps the current Cloud Run posture,
  no k8s) vs GKE (matches `docs/brain-serving-v0.md`, but no manifests exist in the repo yet).
- Whether the tier split in §3 is enforced by contract negotiation, by entitlement on the key,
  or both.
- Consent surface for Free/Pro: where the user is told text leaves the machine, and what the
  retention commitment actually is.
- Whether `dasein-embed` stays alive as a fallback during migration, or is retired at cutover.
