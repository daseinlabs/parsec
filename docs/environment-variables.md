# Environment variables

Every env var read anywhere in this repo, grouped by the process that reads
it. Source of truth is the read site (file:line drifts; the var names and
defaults below are asserted by tests where noted). Conventions:

- `DASEIN_*` — ours. Client proxy vars are set in the plugin/hook
  environment; brain vars are set on the Cloud Run service.
- `AC_*` — inherited from the `adaptive-context-clean` reference. On the
  brain these are **parity pins** (see §7): almost never set by hand.
- Unset ⇒ the listed default. "on/off" vars follow the repo idiom: any value
  except the literal `off` (or `0` where noted) means on.

## 1. Client proxy — runtime (`dasein` binary)

Read in `packages/proxy/src/server.rs`, `main.rs`, `hook.rs`.

| Var | Default | Effect |
|---|---|---|
| `DASEIN_PROXY_PORT` | `8082` | Port the proxy binds on 127.0.0.1. |
| `DASEIN_UPSTREAM` | `https://api.anthropic.com` | Upstream base URL (bench points this at the mock upstream / usage gateway). |
| `ANTHROPIC_BASE_URL` | unset | Read by the hook to find the proxy the plugin routed Claude Code at; autostart only engages when it is set. |
| `DASEIN_PROXY_AUTOSTART` | on | `0` disables the hook's proxy autostart. |
| `DASEIN_PROXY_IDLE_EXIT_S` | `0` (never) | Idle seconds before the proxy exits; the hook sets a default when it autostarts and the var is unset. |
| `DASEIN_SESSION_TTL_S` | `3600` | Per-conversation state TTL. |
| `DASEIN_SESSION_MAX` | `512` | Conversation-state cap (LRU beyond this). |
| `DASEIN_VERBOSE` | unset | `1` switches to the verbose tracing filter. |
| `DASEIN_RECORD_DIR` | unset | When set, every inbound request body is dumped verbatim to this dir (§8.1 capture seam; fail-open). |
| `DASEIN_FREEZE` | on | `off` is the master escape hatch: no brain config, passthrough curation. |
| `AC_CHUNK_MODE` | `fixed` | Engine chunking mode (`cst` opts into tree-sitter atoms). Client-side twin of the brain's pinned flag — leave alone in production; the ckpt was trained on `fixed`/10. Read in `packages/engine/src/chunking.rs`. |

## 2. Client proxy — brain connection

Read in `packages/proxy/src/brain.rs` (`BrainConfig::from_env`).

| Var | Default | Effect |
|---|---|---|
| `DASEIN_BRAIN_URL` | unset (dev builds); baked production URL (release builds) | **The enable switch.** Release binaries carry a compile-time default (`DASEIN_DEFAULT_BRAIN_URL` stamped by release.yml → the production Cloud Run URL); the runtime var always overrides it, and setting it EMPTY disables the brain even on a release build. Dev/CI builds bake nothing: unset ⇒ no brain. |
| `DASEIN_BRAIN_CONTRACT` | `dev` (env URL) / `v1` (baked URL) | Only the exact string `v1` selects the textless client-featurized contract. When the URL comes from the baked release default, the contract defaults to v1 instead — a released binary is data-plane-clean by default. |
| `DASEIN_BRAIN_DEV_RAW` | unset | `1` is the **required opt-in** for the dev contract (raw internal text rides to our cluster — dev machines only). Without it, a dev-contract brain URL is refused and curation stays passthrough. |
| `DASEIN_BRAIN_KEY` | unset | Bearer token sent to the brain (and checked by it, §6). |
| `DASEIN_BRAIN_TIMEOUT_MS` | `10000` | Per-request brain HTTP timeout. |
| `DASEIN_TARGET_COV` | `0.70` | Serving coverage operating point requested from the brain. |
| `DASEIN_EMBED_BACKEND` | `hash` | v1-contract client embedder: `hash` (deterministic test vectors — warns, NOT trained bge) \| `remote` \| `onnx`. With the baked release URL, `hash` keeps the brain OFF entirely (garbage scores don't fail open the way HTTP errors do) — the baked default activates only once a real embedder is configured. |
| `DASEIN_EMBED_URL` | unset | Required for the `remote` embed backend. |
| `DASEIN_ONNX_DIR` | unset | Local bge-large export dir for the `onnx` backend (needs the `onnx` cargo feature; errors if compiled out). |

## 3. Client proxy — tool prune

Read in `packages/proxy/src/brain.rs`, applied in `server.rs`.

| Var | Default | Effect |
|---|---|---|
| `DASEIN_TOOL_PRUNE` | on (when brain configured) | `off` disables tool-schema pruning entirely. |
| `DASEIN_TOOL_CUT` | `0.70` | Rank-to-target cut fraction of roster token mass (AC_TOOL_CUT equivalent). |
| `DASEIN_TOOL_STUB` | on | Pruned custom tools are served as name+note stubs so the model knows they exist and can call one to restore its full schema (reactive unfreeze). `off` restores the reference hard-drop. Provider-typed tools are never stubbed. |

## 4. Client proxy — governor

Read in `packages/proxy/src/governor.rs` (`GovernorConfig::from_env`). All
dials are inert while the mode is `off`.

| Var | Default | Effect |
|---|---|---|
| `DASEIN_GOVERNOR` | `off` | `off` \| `advise` (compute + ledger-record, wire untouched) \| `on` (directives injected). Also gates whether `gf` rides to the brain, i.e. whether the doomhead is scored at all. |
| `DASEIN_RULE_TAU` | `0.25` | Rule-head fire threshold (advisory calibration; bench-validated tau pending). |
| `DASEIN_DOOM_THRESH` | `0.5` | Doomhead flag threshold (reference proxy value). |
| `DASEIN_DOOM_K` | `3` | Consecutive dooms ≥ thresh required to flag. |
| `DASEIN_RUNAWAY_RATIO` | `3.25` | Billed-cum / neighbor-median kill knee (AC_RUNAWAY_RATIO). |
| `DASEIN_DOOMED_RATIO` | `2.0` | DOOMED advisory arm (AC_DOOMED_RATIO); advisory-only here. |
| `DASEIN_KILL_FLOOR_TOK` | `750000` | Billed input-token floor below which no kill arm may fire (token-denominated twin of AC_KILL_FLOOR_USD). |
| `DASEIN_HORIZON_STEP` | `0` (off) | Budget-horizon directive step (reference serve used 40). |

## 5. Client proxy — adjudicator (Stop hook)

Read in `packages/proxy/src/hook.rs`; logic in `adjudicator.rs`. Verdicts log
to `~/.dasein/adjudicator.jsonl`.

| Var | Default | Effect |
|---|---|---|
| `DASEIN_ADJUDICATOR` | `advise` | `off` \| `advise` (log only) \| `block` (may block a premature stop). Blocking is deliberately opt-in — the reference's block-on-CONTINUE overrode correct stops. |
| `DASEIN_ADJ_MAX_BLOCKS` | `2` | Per-session cap on blocked stops. |

## 6. Brain service (Python, Cloud Run)

Read in `packages/brain/src/dasein_brain/` (`bundle.py`, `scorer.py`,
`app.py`, `_log.py`, `vendored/dasein_embed.py`).

| Var | Default | Effect |
|---|---|---|
| `DASEIN_CKPT` | `~/.dasein/brain/curator_v4_prod.pt` | Checkpoint path. The bundle self-validates shapes (incl. doom_head presence) and refuses to start on mismatch. |
| `DASEIN_RULES_JSON` | `<package>/models/rules.json` | Rule-head roster. |
| `DASEIN_TARGET_COV` | `0.70` (falls back to `AC_TARGET_COV`) | Coverage the calibrated tau resolves. |
| `DASEIN_SERVE_TAU` | unset | Manual tau override — **demo/testing only** (compose demo pins 0.999); export empty for calibrated serving. |
| `DASEIN_HOODS_PKL` | unset | Neighborhood artifact for `/v1/neighbors`; unset ⇒ neighbors endpoint inert (governor runaway signal gets no median). |
| `DASEIN_NEIGHBORS` | `16` | Neighbors k. |
| `DASEIN_NEIGHBORS_X` | `2` | Trained expansion value — **do not change**. |
| `DASEIN_EMBED_BACKEND` | `dasein` | Brain-side embedder: `dasein` (remote bge service) \| `hash` (hermetic tests). Note the different default from the proxy's client-side var of the same name. |
| `DASEIN_EMBED_URL` | in-cluster dasein-embed `/embed` | Embedding service endpoint. |
| `DASEIN_EMBED_MODEL` | `bge-large-en-v1.5` | Model id sent to the embed service. |
| `DASEIN_EMBED_BATCH` | `512` | Embed batch size (GPU amortization). |
| `DASEIN_BRAIN_KEY` | unset | If set, bearer auth is required on every endpoint. |
| `DASEIN_BRAIN_LOG` | `INFO` | Log level. |
| `DASEIN_BRAIN_LOG_JSON` | unset | `1` switches to JSON log lines. |

## 7. Brain parity pins (`AC_*`) — do not set by hand

`packages/brain/src/dasein_brain/_flags.py` pins every graph-construction
flag `curator_v4_prod.pt` was trained under, **before** any vendored import
(a wrong flag is a silently-wrong-score bug, not a crash). The effective
state is exposed on `/v1/bundle` (capture seam).

Pinned via `setdefault` (deploy may override only non-shape-critical knobs;
the bundle revalidates chunk_mode/chunk_lines/hetgraph against the ckpt):

`AC_CHUNK_MODE=fixed`, `AC_CHUNK_LINES=10`, `AC_HETGRAPH=on`,
`AC_HUBPROPS=off`, `AC_ISSUEMATCH=on`, `AC_SCOUTFEAT=on`, `AC_FILECHAIN=on`,
`AC_KNN_XFILE=on`, `AC_HEAD=mlp`, `AC_NSCACHE=on`, `AC_TARGET_COV=0.70`.

Force-unset at import (any inherited value silently changes graph shape,
read-head width, or score scale): `AC_STEPGRAPH`, `AC_STRICTEDGE`,
`AC_PRUNE`, `AC_SKIP`, `AC_GATE`, `AC_RULE`, `FIXED_TAUS`, `AC_SERVE_TAU`,
`AC_TARGET_CUT`.

## 8. Bench (`packages/bench`)

Read in `cc_runner.py`, `grader.py`, `proxy_bin.py`, `usage_gateway.py`,
`prepare_repos.py`, `arms/dasein.py`. The bench also forwards the §1–§3
proxy vars verbatim to the `dasein` binary it launches.

| Var | Default | Effect |
|---|---|---|
| `DASEIN_BIN` | repo build | Path to the `dasein` binary the bench should run. |
| `DASEIN_BENCH_CKPT_SHA256` | unset | If set, the dasein arm asserts the brain's checkpoint id matches before running. |
| `MODEL` | `claude-sonnet-4-6` | Model the Claude Code runner uses. |
| `BENCH_UPSTREAM` | Anthropic API | Upstream for bench runs (usually the usage gateway). |
| `BENCH_SWEBENCH_DATASET` | `SWE-bench/SWE-bench_Verified` | Grader dataset. |
| `BENCH_SWEBENCH_SPLIT` | `test` | Grader split. |
| `AC_TASK_REPO` / `AC_TASK_PROBLEM` | unset | Single-task override (repo path / problem statement) instead of a dataset row. |
| `AC_SWEBENCH_DATASET_HF` / `AC_SWEBENCH_SPLIT` | unset | Reference-compat dataset override for replay. |
| `AC_NO_GRADE` | unset | Skip grading. |
| `CCB_NO_RESET_AFTER` | unset | `1` leaves the task repo dirty after a run. |
| `CCB_GRADE_CACHE` | `instance` | swebench grade cache level. |
| `CCB_GATEWAY_UPSTREAM` | Anthropic API | Usage-gateway upstream. |
| `CCB_GATEWAY_LOG_DIR` | `runs/usage` | Usage-gateway log dir. |
| `CCB_GATEWAY_RUN_ID` | `default` | Usage-gateway run tag. |
| `REPO_CACHE` | `~/repo_cache` | prepare_repos clone cache. |
| `UV_BIN` | on PATH | uv binary for env prep. |
| `PREP_JOBS` | `8` | prepare_repos parallelism. |

## 9. Platform service (`packages/platform`)

| Var | Default | Effect |
|---|---|---|
| `DASEIN_PLATFORM_DB_URL` | unset | Postgres/Supabase pooler URL; when set, pgstore is used. |
| `DASEIN_PLATFORM_DB` | `:memory:` | SQLite path fallback (dev). |
| `SUPABASE_JWKS_URL` | unset | Asymmetric JWT verification (preferred; keys cached, refetch on unknown kid). |
| `SUPABASE_JWT_SECRET` | unset | HS256 fallback verification. |
| `STRIPE_WEBHOOK_SECRET` | unset | Stripe webhook signature check. |

## Quick recipes

Default plugin install (free tier — no brain): nothing to set.

Pro-tier dev against a local brain (raw-text opt-in, dev machines only):

```sh
DASEIN_BRAIN_URL=http://127.0.0.1:8093 DASEIN_BRAIN_DEV_RAW=1
```

Data-plane-clean v1 with real embeddings:

```sh
DASEIN_BRAIN_URL=... DASEIN_BRAIN_CONTRACT=v1 DASEIN_EMBED_BACKEND=onnx DASEIN_ONNX_DIR=...
```

Governor shadow mode (score the doomhead, record, touch nothing):

```sh
DASEIN_GOVERNOR=advise
```

Reference-parity hard-drop tool prune (disable stubs):

```sh
DASEIN_TOOL_STUB=off
```
