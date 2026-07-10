# dasein-bench

Port of the cc-bench harness (`adaptive-context-clean/bench`, ~4.4k lines) to
the learner repo. One fixed agent — headless Claude Code — runs against task
repos; only the compression layer (the "arm") differs between runs. The bench
sits at the TOP of the dependency graph (`bench → proxy → engine`, CLAUDE.md):
it drives the `dasein` proxy as a **black-box binary** and is imported by
nothing.

```
packages/bench/
  src/dasein_bench/
    cc_runner.py      the runner (real `claude` CLI via the Claude Agent SDK)
    replay.py         REPLAY mode — recorded conversations, no credits
    arm.py, arms/     arm contract + registry; baseline + dasein arms
    usage_gateway.py  passthrough bottom bridge; per-run usage JSONL
    ledger.py         savings-ledger accounting (§8.4 counterfactual math)
    pricing.py        cache-aware price frames (reference table, as-is)
    schema.py         RunRecord / CallUsage / AggResult
    grader.py         official SWE-bench grading seam (Docker harness)
    prepare_repos.py  per-instance worktrees + isolated test envs
    proxy_bin.py      locate/spawn the dasein binary
    mock_upstream.py  in-process Anthropic-shaped mock (replay + tests)
  tests/              pytest suite (venv: .venv, python 3.12)
```

## ⚠️ CHECKPOINT CONTAMINATION — read before quoting any number

**`curator_v4_prod.pt` is SERVING-ONLY.** It was trained on ALL data,
*including the eval/benchmark tasks* (`AC_EXCL` empty) — correct for
production serving, but **any benchmark number produced with it is
contaminated and invalid** (rulehead `PRODUCTION_SERVING_HANDOFF.md`).
Benchmarks must point the brain at the **eval-excluded twin `curator_v4.pt`**
(or `curator_v3.pt`): `gsutil cp gs://dasein-473321-ac-learning/rulehead/curator_v4.pt …`
and start `packages/brain` with `DASEIN_CKPT` set to it.

Guard rails in this harness:
* the dasein arm records the brain's `checkpoint_id` (sha256 of the `.pt`)
  from `/v1/bundle` into every `RunRecord`, so contamination is auditable;
* set `DASEIN_BENCH_CKPT_SHA256=<sha256 of curator_v4.pt>` and the arm's
  `ready()` REFUSES to run against any other bundle.

## Measurement honesty (§8.4)

Savings numbers come **only** from the proxy's per-request `count_tokens`
counterfactual (the `savings-ledger/v0` rows the proxy writes to
`$HOME/.dasein/ledger.jsonl`): per probed request,
`counterfactual_input_tokens − (billed_input + cache_read + cache_write)`.
Rows whose probe failed (`counterfactual_input_tokens: null`) are **excluded**
from `tokens_saved` — never estimated (`dasein_bench.ledger`). The
`freeze_cut_tokens` field is a chars/4 diagnostic, never a savings claim.
Dollar frames (`dasein_bench.pricing`) price what was *billed* and are never a
savings baseline.

## Replay mode — savings estimate without Anthropic credits

Replays a RECORDED conversation ({"turns": [/v1/messages bodies…]}, the
`packages/proxy/parity/fixtures/golden_conversation.json` shape) through a
locally-spawned arm proxy + an in-process mock upstream, and reports per turn:
forwarded-size delta, tool-prune delta, and the counterfactual-vs-served
estimate off the proxy's own ledger. The mock's `count_tokens` and billed
usage sit on one chars/4 scale, so the wiring is exactly the honest one — the
numbers are clearly labeled estimates, not billed tokens.

```bash
# passthrough machinery (no brain): deltas ≈ 0 by design
.venv/bin/python -m dasein_bench.replay --fixture tests/fixtures/replay_2turn.json

# the real thing: brain-scored trimming + tool pruning (start packages/brain
# first — see scripts/e2e_smoke.sh for the local brain recipe)
.venv/bin/python -m dasein_bench.replay \
    --fixture ../proxy/parity/fixtures/golden_conversation.json \
    --brain-url http://127.0.0.1:8090 --json report.json
```

This mode runs end-to-end in the test suite (`tests/test_replay.py` spawns the
real proxy binary + mock upstream) — no credits, no brain, no network.

## Running a REAL bench (documented, not run here)

Needs: the `claude` CLI on PATH, **your own Anthropic credits**
(`ANTHROPIC_API_KEY`; the key rides the chain untouched — the gateway and
proxy forward auth headers verbatim and never store them), Docker (grading),
`uv`, and disk for task repos. Install the run extras into the venv:

```bash
cd packages/bench
uv venv --python 3.12 .venv
uv pip install --python .venv/bin/python -e '.[test,run]'
```

1. **Provision task repos + per-instance test envs** (one-time, slow):

   ```bash
   AC_REPO_ROOT=~/task_repos .venv/bin/python -m dasein_bench.prepare_repos \
       --tasks tasks.json            # add --arms baseline,dasein for per-arm trees
   ```

2. **Start the brain** with the eval-excluded checkpoint (see the warning
   above) and export `DASEIN_BRAIN_URL` (+ optionally
   `DASEIN_BENCH_CKPT_SHA256`). The dasein arm spawns `dasein proxy` per solve
   (env `DASEIN_BIN` or `target/{release,debug}/dasein`) with its upstream
   pointed at the run's usage gateway; chain per arm:

   ```
   baseline: Claude Code → gateway → api.anthropic.com
   dasein  : Claude Code → dasein proxy (curates; writes the savings ledger)
                         → gateway → api.anthropic.com
   ```

3. **Run** (real spend — start with `--limit 1`):

   ```bash
   .venv/bin/python -m dasein_bench.cc_runner --list-arms   # readiness check
   .venv/bin/python -m dasein_bench.cc_runner \
       --tasks tasks.json --arms baseline,dasein \
       --repo-root ~/task_repos --out runs --limit 1
   ```

Outputs under `--out`: `ledger.jsonl` (one RunRecord per solve, resume-safe),
`runs/<iid>__<arm>.json`, `traj/*.traj.json` + `*.outcome.json` sidecars,
`usage/<run_id>.usage.jsonl` (per-call cache split). Grading shells out to the
official `swebench.harness.run_evaluation` (Docker); `AC_NO_GRADE=1` skips it
for bring-your-own-repo runs (`AC_TASK_REPO` / `AC_TASK_PROBLEM`).

## Cut vs the reference (clearly-marked seams)

* **GCS trace-bus rsync** (`--bus`/gsutil) and remote-instance orchestration —
  local single-machine runs only (seam note in `cc_runner.py`).
* **Vertex bridge** (gateway `MODE_VERTEX`: AnthropicVertex + ADC + beta
  filtering) — the chain bottoms out at `api.anthropic.com` (seam note in
  `usage_gateway.py`).
* **Shared standalone gateway** (`CCB_GATEWAY_URL`) — per-solve ephemeral
  gateways only (seam note in `cc_runner.py`).
* **ToolArm / plugin loading** (the reference's woz arm) — no consumer here
  (seam note in `arm.py`).

## Tests

```bash
cd packages/bench && .venv/bin/python -m pytest -q
```

Covers config parsing, ledger accounting math over synthetic savings-ledger
rows (incl. the §8.4 null-probe exclusion), pricing math, gateway usage
extraction/relay, and the replay mode end-to-end against a 2-turn synthetic
fixture with the real proxy binary + mock upstream.
