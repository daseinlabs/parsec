# learner — working notes for Claude

`DIRECTION.md` is the product/architecture anchor; consult it before structural
decisions. Non-negotiables when writing code here:

- **Data plane local, control plane ours.** Raw code/text never leaves the
  user's machine; GNN weights never leave our cloud. The brain API contract
  must be unable to represent raw text. Subscription OAuth tokens must never
  be routed through our cloud (BYOK API keys only).
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
- **No TypeScript.** Client = Rust (`dasein` binary + plugin markdown/JSON);
  server = Python.
- **Parity is the port's definition of done**: Rust must match the Python
  reference byte-for-byte on freezing and vector-for-vector on featurization.

Source repos for ports: `adaptive-context-clean` (proxy/engine/mapgen/bench),
`adaptive-context` (trainer lineage), `dasein-compression-service` (test
suites), `dasein-serving-engine` (GKE CI/CD pattern).
