# learner — working notes for Claude

`DIRECTION.md` is the product/architecture anchor; consult it before structural
decisions. Non-negotiables when writing code here:

- **Data plane local, control plane ours.** Model traffic always leaves from
  the user's machine with the user's own credentials; GNN weights never leave
  our cloud. Subscription OAuth tokens must never be routed through our cloud
  (BYOK API keys only). ⚠️ Revised 2026-07-20: embedding moved server-side, so
  chunk text *does* cross the wire and the brain API contract is no longer
  required to make raw text unrepresentable. See `docs/server-side-embedding.md`.
- **The proxy is the determinism boundary.** Chunking, freezing, rendering, and
  splicing stay client-side. This is what keeps brain replicas stateless and
  round-robin-safe — do not move freezing server-side just because the text is
  already there.
- **Determinism over state.** Served bytes are a pure function of
  (conversation prefix, checkpoint, config). No wall clock, RNG, or session
  dicts on the serving path. `serde_json` must keep `preserve_order`.
- **Fail open, but measured.** Every layer degrades to passthrough on error;
  fail-open events are counted and alertable.
- **Measurement honesty.** Savings numbers come only from the per-request
  count_tokens counterfactual — never a modeled baseline.
- **Dependency direction**: `bench → proxy → engine`; `brain`/`trainer` import
  `contracts` only, never client crates. Training code must not be a runtime
  dependency of serving.
- **No TypeScript in client/plugin code.** Client = Rust (`parsec` binary +
  plugin markdown/JSON); server = Python. The §7b rationale (runtime
  guarantee, binary auditability) is client-only — the web dashboard
  (`packages/frontend`, Next.js) is exempt.
- **Parity is the port's definition of done**: Rust must match the Python
  reference byte-for-byte on freezing and vector-for-vector on featurization.

Source repos for ports: `adaptive-context-clean` (proxy/engine/mapgen/bench),
`adaptive-context` (trainer lineage), `dasein-compression-service` (test
suites), `dasein-serving-engine` (GKE CI/CD pattern).
