# learner

Productization of the Dasein A3S stack: a Claude Code plugin that compresses
agent context per turn using a learned curator. **Data plane local, control
plane ours** — model traffic rides the user's own credentials from their
machine; only chunk vectors + structural features ever reach our cloud.

Read [DIRECTION.md](DIRECTION.md) first. It is the anchor for every decision
in this repo.

## Layout

| Package | Language | License | What |
|---|---|---|---|
| `packages/plugin` | markdown + JSON + committed binaries | OSS | Claude Code plugin: `dasein:explore` agent, no-reread hook, savings skills, status line |
| `packages/proxy` | Rust (`dasein` binary) | OSS | Local data-plane proxy; same binary deploys as the Team BYOK gateway |
| `packages/engine` | Rust | OSS | Deterministic core: chunking, ONNX embedder, quantized freezing, featurization |
| `packages/mapgen` | Rust | OSS | codescout exploration maps |
| `packages/contracts` | JSON Schema | OSS | Cross-language schemas (trace contract, telemetry, savings ledger, brain API) |
| `packages/brain` | Python | private | Hosted GNN scoring API |
| `packages/trainer` | Python | private | Training, eval gates, checkpoint promotion |
| `packages/bench` | Python | private | cc-bench harness; drives the proxy as a black box |

Dependency direction (enforced): `bench → proxy → engine`; `plugin → proxy`
(manages the process); `brain`/`trainer` share `contracts` with everything but
import nothing client-side.

## Build

```sh
cargo build            # engine, proxy (dasein binary), mapgen
cargo test
```

Python packages are independent uv/pip projects under `packages/{brain,trainer,bench}`.

## Invariants (CI-enforced from day one — DIRECTION.md §8)

1. Cache-stability golden test: replayed conversations byte-identical on every previously served turn.
2. Curator parity: brain-API score == trainer-forward score on identical chunks.
3. Fail-open floor: every layer degrades to passthrough, and fail-open rate is alerted on.
4. Measurement honesty: savings only from the count_tokens counterfactual, never a modeled baseline.
