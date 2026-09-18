# parsec — working notes for Claude

`DIRECTION.md` is the architecture anchor; consult it before structural
decisions. Non-negotiables when writing code here:

- **Data plane local, control plane remote.** Model traffic always leaves
  from the user's machine with the user's own credentials. Subscription OAuth
  tokens are never routed through any hosted service. Chunk text (capped) does
  cross the wire to the scoring API when scoring is enabled; the README's
  "What parsec sends" section is the disclosure and must stay accurate.
- **The proxy is the determinism boundary.** Chunking, freezing, rendering,
  and splicing stay client-side. Do not move freezing server-side.
- **Determinism over state.** Served bytes are a pure function of
  (conversation prefix, checkpoint, config). No wall clock, RNG, or session
  dicts on the serving path. `serde_json` must keep `preserve_order`.
- **Fail open, but measured.** Every layer degrades to passthrough on error;
  fail-open events are counted and alertable. No `unwrap`/`expect`/`panic!`
  on the serving path.
- **Measurement honesty.** Savings numbers come only from the per-request
  `count_tokens` counterfactual, never a modeled baseline.
- **Dependency direction**: `proxy → engine`; `mapgen` is a leaf. `brain`
  (the scoring service) imports `contracts` only, never a client crate, and
  no client code imports `brain`. Training code is not in this repository
  and must never become a runtime dependency of serving.
- **The brain serves the vendored reference path.** `packages/brain/src/
  parsec_brain/vendored/` is the exact code the checkpoints were trained
  under; parity with the trainer forward is by construction. Do not
  "clean it up" — every struct column and env flag there is score-affecting.
  The bundle self-validates at startup and refuses a mismatched checkpoint.
- **No TypeScript in client/plugin code**, with two named exceptions. Client =
  Rust (`parsec` binary + plugin markdown/JSON); server = Python. The OpenCode
  plugin shim (`packages/opencode-plugin/index.js`) and the pi extension
  (`packages/pi-extension/parsec.ts`) are the only JS/TS files: each is a
  single dependency-free file, embedded into the binary with `include_str!`,
  that only routes, revives the proxy, and bridges hooks. Curation logic never
  lives there.
- **Parity fixtures are frozen goldens.** They were generated from a Python
  reference implementation that is not in this repository. Do not regenerate
  or hand-edit them; a change in behaviour that breaks a fixture needs a
  maintainer to regenerate it.
- **Every new env var** gets a doc comment where it is read, in the same
  change.
