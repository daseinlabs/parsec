# parsec — Technical Direction

This is the architecture anchor for the repository. Source comments cite it by
section number (`DIRECTION.md §8.1`), so the numbering is stable even where a
section has been condensed. Business planning, sequencing, and the hosted
services' internals live outside this repository; the sections below are the
parts that shape the code you can read here.

---

## 1. The product in one paragraph

parsec is a coding-agent plugin plus a **local proxy** that compresses the
agent's context per turn. The free tier works with zero infrastructure
(exploration maps, the no-reread hook, the savings status line). The curated
tier routes model traffic through the local proxy, which chunks and
featurizes the conversation on the user's machine, asks a hosted scoring
service for keep/cut scores, and applies the decision itself. Model traffic
rides the user's own credentials from the user's own machine.

## 2. The one rule that shapes everything

**Data plane local, control plane remote.**

- **Data plane** (model requests → the provider): always leaves from the
  user's machine, with the user's credentials. The proxy never relays model
  traffic through anyone else's infrastructure. Subscription OAuth tokens are
  only ever used as the user's own local traffic.
- **Control plane** (scoring, savings ledger, rule updates, opt-in telemetry):
  a hosted service. The scoring model lives only there. The local proxy sends
  chunk text plus structural features and receives scores; it applies the
  keep/cut decision itself and the service never learns what was dropped.

Revised 2026-07-20: this rule previously read "the proxy sends vectors, never
raw text." Embedding now runs server-side, so chunk text (capped per chunk)
does cross the wire when scoring is enabled. See the README section "What
parsec sends" for the exact list.

## 3. Tiers

Condensed. What matters for the code: the free tier must work with no
account, no key, and no network beyond the provider itself; every curated
feature must degrade to that.

## 4. Architecture

```
┌─ user's machine ──────────────────────────────────────────────┐
│  coding agent ── plugin (agents, hooks, status line, settings)│
│      │  ANTHROPIC_BASE_URL (curated tier)                     │
│      ▼                                                        │
│  local proxy                                                  │
│    • chunking + structural featurization                      │
│    • applies keep/cut, tool-prune, cache-safe splicing        │
│    • deterministic quantized freezing (no session-state trap) │
│    • count_tokens counterfactual measurement                  │
│      │ text+features           │ curated request,             │
│      ▼                         ▼ user's own auth headers      │
│  [scoring API] ◄─────────   api.anthropic.com                 │
└───────│───────────────────────────────────────────────────────┘
        ▼  control plane (hosted, not in this repository)
   scoring API · savings ledger · opt-in telemetry intake
```

Key decisions:

1. **Plugin as the integration surface.** Installing a plugin beats "change
   your base URL"; agents, hooks, and MCP give map delivery, no-reread, and the
   stop-governor their seams natively. Maps come from a sanctioned
   `parsec:explore` agent backed by `mapgen` running locally.
2. **Cache stability by determinism, not state.** Served bytes are a pure
   function of (conversation prefix, checkpoint, config): quantized,
   append-only freezing instead of a per-process session dict. Recomputable ⇒
   restart-safe ⇒ the same engine runs as a laptop process or a replicated
   service without sticky routing.
3. **Honest measurement as a product surface.** A per-request `count_tokens`
   probe on the original body versus actually-billed usage, in the same event
   row. No modeled baseline, ever.
4. **The learned curator is served, not shipped.** Everything in this
   repository is the deterministic shell: chunking, freezing, splicing,
   measurement, maps, hooks. The scoring model is a service behind a
   versioned contract (`packages/contracts`).

## 5. Local inference — what runs on the user's machine

- **No local embedder.** The client ships no model and downloads nothing.
- **Chunking and structural featurization stay local.** The freezer must
  chunk to know what to drop and render.
- **Scoring is remote** by default, one round trip per turn, cacheable.
  Without a scoring endpoint the proxy runs passthrough curation.
- **Decisions are applied locally.** The service returns scores and a
  threshold; the proxy applies the coverage budget and the cut.
- **No local training.** Local adaptation only: per-config threshold
  calibration and per-repo localization indexes. Cheap, deterministic, cannot
  corrupt the model.

## 6. Training

Server-side, outside this repository. The only contract with the client is
the opt-in trace-sharing schema in `packages/contracts` and the `/parsec:share`
preview, which shows the exact bytes before anything uploads.

## 7. Repository layout

```
parsec/
  DIRECTION.md            ← this doc
  packages/
    engine/               chunking, featurization, freezing, readout — pure Rust library
    proxy/                the `parsec` binary: local proxy, hooks, MCP server, status line,
                          installers' runtime, harness setup
    mapgen/               deterministic repo maps, outlines, symbol lookup (Explore contract)
    contracts/            JSON Schemas shared across languages: brain API, savings ledger,
                          telemetry events, install report
    plugin/               Claude Code plugin: markdown, JSON, launcher shims (binaries are
                          GitHub Release assets, never committed; the shims fetch them).
                          The marketplace is this repo: .claude-plugin/marketplace.json
    opencode-plugin/      OpenCode plugin shim
    pi-extension/         pi extension (single TS file, embedded into the binary by setup_pi)
    installer/            native macOS .pkg and Windows Inno Setup sources
    brain/                the scoring service (Python): GNN inference over a curator
                          checkpoint, self-validating bundle, calibrated tau. Self-hostable;
                          the released base model lives on the Hugging Face Hub
  docker-compose.yml      self-host the scoring service next to the host-process proxy
  scripts/                install scripts, release helper, dev helpers
```

Dependency direction (enforced): `proxy → engine`; `plugin → proxy` (manages
the process); `mapgen` is a leaf; `brain` imports `contracts` only and nothing
client-side imports `brain`. Training code is not in this repository.

**Licensing:** everything here is MIT, the scoring service included. The
training pipeline, the labeled trace corpus, and the account platform are
separate, closed components; the base curator checkpoint is published
separately on the Hugging Face Hub under its own model license.

## 7b. Stack decision

**Client side: Rust. Server side: Python. No TypeScript in client code.**

- Node is not guaranteed on machines running Claude Code; a compiled binary
  is not a runtime bet.
- Every plugin surface accepts arbitrary executables: MCP `command`, hook
  exec form, status line program. Agents and skills are markdown.
- **One `parsec` binary, subcommands for every role**: `parsec mcp`,
  `parsec hook <event>`, `parsec proxy`, `parsec statusline`, `parsec setup`,
  `parsec login`, `parsec trim`.
- A credential-handling proxy audits far better as one signed binary with a
  dozen crates than as a `node_modules` tree.
- **The port's definition of done is the two cross-language parity suites**
  (§8): Rust must reproduce the Python reference byte-for-byte on freezing
  (JSON key order, escaping, float formatting) and vector-for-vector on
  featurization. Divergence is the "silently wrong scores" failure mode.

## 7c. Platform layer

Hosted account, billing, and ledger services. Not in this repository. The
client's only coupling is the savings-ledger and install-report schemas in
`packages/contracts` and the `PARSEC_PLATFORM_URL` / `PARSEC_API_KEY` knobs.

## 8. Invariants to protect with CI

1. **Cache-stability golden test** (§8.1): replay a recorded multi-turn
   conversation; every previously served turn is byte-identical to what was
   served before, with one sanctioned exception: the observations of the
   previous call's current turn are served in full while the model acts on
   them and take their curated form exactly once, on the next call
   (`FreezeConfig::protect_current`). `packages/proxy/tests/golden_conversation.rs`.
2. **Curator parity**: the score the proxy computes for a chunk equals the
   reference implementation's score on identical input, at the pinned
   threshold. Fixtures under `packages/engine/parity` and `packages/proxy/parity`.
3. **Fail-open floor**: every layer degrades to passthrough on error, and
   fail-open events are counted so the rate can be alerted on.
4. **Measurement honesty**: savings come only from the per-request
   `count_tokens` counterfactual, never from a modeled baseline.

§8.1 is what "cache-stable" means throughout the code: an already-served
prefix must be re-emitted byte-for-byte on the next turn, or the provider's
prompt cache is invalidated and the user pays for the whole context again.
The current turn's re-fold is the deliberate exception: it invalidates the
cache only from the second-to-last message onward — the tail the next turn
appends to anyway — and buys the model a full view of the result it is
about to act on. Curation folds history, never the answer just returned.
