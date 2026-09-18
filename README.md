<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/parsec-mark-glow.svg">
    <img src="assets/brand/parsec-mark-light.svg" width="220" alt="parsec">
  </picture>
</p>

<h1 align="center">parsec</h1>

<p align="center"><b>2× the context. ½ the cost.</b><br>
<sub>Same model. Longer reach.</sub></p>

<p align="center">
  <a href="https://github.com/daseinlabs/parsec/releases/latest"><img alt="latest release" src="https://img.shields.io/github/v/release/daseinlabs/parsec?style=flat-square&color=2ea043"></a>
  <a href="https://github.com/daseinlabs/parsec/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/daseinlabs/parsec/ci.yml?branch=main&style=flat-square"></a>
  <a href="LICENSE"><img alt="MIT license" src="https://img.shields.io/badge/license-MIT-2ea043?style=flat-square"></a>
</p>

**Context savings for coding agents.** parsec is a local proxy and plugin for
Claude Code, OpenCode, Codex CLI, pi, and Claude Desktop that cuts the tokens your
agent spends per turn: it blocks wasteful re-reads, breaks command loops, and
(with scoring enabled) curates the conversation context before each request,
cache-safely. Savings are measured per request against the provider's own
token counter, never estimated.

Everything runs on your machine with your own credentials. Model traffic never
touches anyone else's infrastructure.

## Install

<table align="center">
  <tr>
    <td align="center" width="33%">
      <a href="#macos"><picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/os/macos-dark.svg">
    <img src="assets/brand/os/macos-light.svg" width="56" alt="macOS">
  </picture></a><br>
      <b>macOS</b><br><sub>Apple silicon</sub>
    </td>
    <td align="center" width="33%">
      <a href="#windows"><picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/os/windows-dark.svg">
    <img src="assets/brand/os/windows-light.svg" width="56" alt="Windows">
  </picture></a><br>
      <b>Windows</b><br><sub>x64</sub>
    </td>
    <td align="center" width="33%">
      <a href="#linux"><picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/os/linux-dark.svg">
    <img src="assets/brand/os/linux-light.svg" width="56" alt="Linux">
  </picture></a><br>
      <b>Linux</b><br><sub>x64</sub>
    </td>
  </tr>
  <tr>
    <td align="center"><a href="https://github.com/daseinlabs/parsec/releases/latest"><code>.pkg</code> installer</a></td>
    <td align="center"><a href="https://github.com/daseinlabs/parsec/releases/latest"><code>setup.exe</code> installer</a></td>
    <td align="center"><a href="#linux">one-line script</a></td>
  </tr>
</table>

Every installer signs you in when it opens the browser, then installs the
`parsec` binary and the Claude Code plugin. The native installers are on the
[releases page](https://github.com/daseinlabs/parsec/releases/latest) next to
the raw binaries the scripts download.

### macOS

Download `parsec-<version>-macos-arm64.pkg` from the
[releases page](https://github.com/daseinlabs/parsec/releases/latest) and
open it, or run:

```sh
curl -fsSL https://raw.githubusercontent.com/daseinlabs/parsec/main/scripts/install.sh | bash
```

### Windows

Download `parsec-<version>-windows-x64-setup.exe` from the
[releases page](https://github.com/daseinlabs/parsec/releases/latest) and run
it, or in PowerShell:

```powershell
irm https://raw.githubusercontent.com/daseinlabs/parsec/main/scripts/install.ps1 | iex
```

### Linux

```sh
curl -fsSL https://raw.githubusercontent.com/daseinlabs/parsec/main/scripts/install.sh | bash
```

The script fetches the `parsec-linux-x64` binary for the latest release and
installs the plugin.

### From the Claude Code CLI

```sh
claude plugin marketplace add https://github.com/daseinlabs/parsec
claude plugin install parsec@parsec-marketplace
```

The plugin fetches its binary from this repository's GitHub Releases on first
use (sha256-verified against the release's `manifest.json`), so a marketplace
install needs nothing else. Installed from the old `daseinlabs/plugins`
marketplace? That repository is frozen at its last release. Switch once:

```sh
claude plugin marketplace remove parsec-marketplace
claude plugin marketplace add https://github.com/daseinlabs/parsec
claude plugin install parsec@parsec-marketplace
```

Then in a session: `/parsec:setup` routes your agent through the proxy,
`/parsec:savings` shows what it saved, `/parsec:share --preview` shows exactly
what opt-in telemetry would upload before anything leaves.

## What parsec sends

parsec is a proxy, so this section is the contract. Everything below can be
verified in `packages/proxy/src` and the schemas in `packages/contracts`.

| When | What leaves your machine | Where | Off switch |
|---|---|---|---|
| Always | Your model requests, with your own auth headers | The provider you already use (`api.anthropic.com` by default) | n/a — this is your agent's own traffic |
| Always, no key needed | An anonymous **install ping**: random install id, parsec version, OS, arch, list of configured harnesses. Sent at setup, on key changes, and every 6 hours while the proxy runs. Never your API key. | parsec platform | `PARSEC_INSTALL_REPORT=0` or `DO_NOT_TRACK=1` |
| With scoring enabled | **Chunk text** (each chunk capped at 2000 chars) plus structural features, for keep/cut scoring. The response is scores; the service does not learn what was dropped. | parsec scoring API | `PARSEC_FREEZE=off`, or run with no scoring endpoint |
| With a parsec key | **Savings-ledger rows**: token counts per request, model, harness, conversation and request ids. No prompt text. | parsec platform | remove the key (`/parsec:key`) or unset `PARSEC_PLATFORM_URL` |
| Opt-in only | Featurized trace sharing (`/parsec:share`) | parsec platform | Off by default; `--preview` shows the exact bytes first |

`~/.parsec/` holds local state: the savings ledger, score memo (hashes and
quantized scores only), and recordings if you enable `PARSEC_RECORD_DIR`.
Delete it whenever you like.

## Layout

| Package | Language | What |
|---|---|---|
| `packages/engine` | Rust | Chunking, featurization, deterministic freezing, readout. Pure library. |
| `packages/proxy` | Rust | The `parsec` binary: local proxy, hooks, MCP server, status line, harness setup. |
| `packages/mapgen` | Rust | Deterministic repo maps, outlines, symbol lookup for the explore agent. |
| `packages/contracts` | JSON Schema | Cross-language schemas: scoring API, savings ledger, telemetry, install report. |
| `packages/plugin` | Markdown/JSON | The Claude Code plugin (agents, skills, hooks, launcher shims). |
| `packages/opencode-plugin` | JS | OpenCode plugin shim. |
| `packages/installer` | Shell/Inno | Native macOS and Windows installer sources. |
| `packages/brain` | Python | The scoring service: GNN inference over a curator checkpoint, self-validating bundle, calibrated tau. Self-hostable. |

The training pipeline and the account platform are separate, closed
components. Everything here talks to them only over the contracts in
`packages/contracts`, and runs without them.

## Self-host the scoring service

The proxy runs without any scoring service (no-reread hook, loop breaker,
savings ledger). Curation needs one, and you can run it yourself: it is
`packages/brain`, one Python process holding the curator checkpoint and the
bge-large encoder.

The released curator checkpoints live on the Hugging Face Hub at
[huggingface.co/parsecai/curator](https://huggingface.co/parsecai/curator).
Use `curator_v7-9_10nn_prod.pt`: it is self-contained and needs no neighbor
store at serve time. The model card lists the other checkpoint and its
calibration tables.

```sh
# 1. a curator checkpoint — fetched and cached via huggingface_hub, or a local path
pip install huggingface_hub
export PARSEC_CKPT=hf://parsecai/curator/curator_v7-9_10nn_prod.pt
# 2. the service (in-process bge-large; CPU works, a GPU is faster)
docker compose up -d                                   # http://127.0.0.1:8090
# 3. point the proxy at it
export PARSEC_BRAIN_URL=http://127.0.0.1:8090
```

`packages/brain/README.md` has the bare-uvicorn form, the auth options, and
a Cloud Run recipe. A self-hosted brain is where the chunk text in "What
parsec sends" goes; with your own host, nothing leaves your infrastructure.

## Build

```sh
cargo build --release   # engine, proxy (the `parsec` binary), mapgen
make check              # fmt + clippy + tests, same flags CI runs
make plugin             # build and install a local plugin binary for `claude --plugin-dir`
```

Rust 1.98 is pinned in `rust-toolchain.toml`. No model download, no network
needed for the Rust test suite. The scoring service's tests
(`make brain-test`) run their hermetic subset without a checkpoint. See
`CONTRIBUTING.md` for the dev loop.

## Invariants

These are what the test suite protects; see `DIRECTION.md §8`.

1. **Cache stability**: replayed conversations are byte-identical on every
   previously served turn.
2. **Parity**: the Rust port matches the reference implementation
   byte-for-byte on freezing and vector-for-vector on featurization.
3. **Fail open, but measured**: every layer degrades to passthrough, and
   fail-open events are counted.
4. **Measurement honesty**: savings only from the `count_tokens`
   counterfactual, never a modeled baseline.

## License

MIT, see `LICENSE`. Third-party notices and trademark note in
`THIRD_PARTY.md`. Security reports: `SECURITY.md`.
