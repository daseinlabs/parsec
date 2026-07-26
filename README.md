# learner

Productization of the Parsec A3S stack: a Claude Code plugin that compresses
agent context per turn using a learned curator. **Data plane local, control
plane ours** — model traffic rides the user's own credentials from their
machine; only chunk vectors + structural features ever reach our cloud.

Read [DIRECTION.md](DIRECTION.md) first. It is the anchor for every decision
in this repo.

## Layout

| Package | Language | License | What |
|---|---|---|---|
| `packages/plugin` | markdown + JSON + CI-built binaries | OSS | Claude Code plugin: `parsec:explore` agent, no-reread hook, savings skills, status line |
| `packages/proxy` | Rust (`parsec` binary) | OSS | Local data-plane proxy; same binary deploys as the Team BYOK gateway |
| `packages/engine` | Rust | OSS | Deterministic core: chunking, ONNX embedder, quantized freezing, featurization |
| `packages/mapgen` | Rust | OSS | codescout exploration maps |
| `packages/contracts` | JSON Schema | OSS | Cross-language schemas (trace contract, telemetry, savings ledger, brain API) |
| `packages/brain` | Python | private | Hosted GNN scoring API |
| `packages/trainer` | Python | private | Training, eval gates, checkpoint promotion |
| `packages/platform` | Python | private | Accounts, billing, savings-ledger API, telemetry intake — FastAPI over Supabase + Stripe (DIRECTION.md §7c) |
| `packages/bench` | Python | private | cc-bench harness; drives the proxy as a black box |

Dependency direction (enforced): `bench → proxy → engine`; `plugin → proxy`
(manages the process); `brain`/`trainer` share `contracts` with everything but
import nothing client-side.

## Try the plugin (free tier v0)

```sh
make plugin   # build + place the gitignored local binary
claude --plugin-dir packages/plugin
```

In the session: re-`Read` a file you already read — the hook denies it with a
context-reuse pointer (re-issuing the same read once passes: the insist
valve). Run the same Bash command 4x — the loop-breaker fires. `/parsec:savings`
reports what was measured. Optional status line (user setting, not
plugin-settable) in `~/.claude/settings.json`:

```json
{ "statusLine": { "type": "command", "command": "<repo>/packages/plugin/bin/parsec statusline" } }
```

## Build

```sh
cargo build            # engine, proxy (parsec binary), mapgen
cargo test
```

Python packages are independent uv/pip projects under `packages/{brain,trainer,bench}`.

## License

The repo default is [MIT](LICENSE) (copyright 2026 Dasein Labs) — it covers
the OSS packages in the table above (`plugin`, `proxy`, `engine`, `mapgen`,
`contracts`). The private packages (`brain`, `trainer`, `platform`, plus
checkpoints and the trace corpus) are **not** open source and carry their own
`LICENSE` notice overriding the root; see DIRECTION.md §7 for the rationale
(open-source the deterministic shell; sell the brain).

## Invariants (CI-enforced from day one — DIRECTION.md §8)

1. Cache-stability golden test: replayed conversations byte-identical on every previously served turn.
2. Curator parity: brain-API score == trainer-forward score on identical chunks.
3. Fail-open floor: every layer degrades to passthrough, and fail-open rate is alerted on.
4. Measurement honesty: savings only from the count_tokens counterfactual, never a modeled baseline.
