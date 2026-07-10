# contracts

Single source of truth for every cross-package, cross-language schema:

- `schemas/tcv2-community.schema.json` — the versioned trace contribution
  contract (Tier 2 featurized traces). Evolves **only with re-consent**.
- `schemas/telemetry-events.schema.json` — Tier 1 metrics events.
- `schemas/savings-ledger.schema.json` — one row per request: count_tokens
  counterfactual + actually-billed usage. The billing basis (Team), the
  upsell proof (Free→Pro), the trust artifact (Enterprise).
- `schemas/brain-api.schema.json` — scoring request/response **v1, SERVED**:
  client-computed chunk vectors + structural features + opaque ids in,
  quantized keep/cut scores out. Raw text is unrepresentable in this schema
  by construction. The brain dispatches on the request `contract` field;
  packages/brain/tests/test_v1.py pins v1 == dev bit-exact scores (the
  cross-contract parity gate), and scripts/parity_v1.sh proves the full
  proxy-side v1 client (Rust featurization + local embedding) forwards
  byte-identical requests to the dev path on the recorded golden
  conversation. The proxy speaks either: `DASEIN_BRAIN_CONTRACT=v1`
  (no raw text leaves the machine) or the default dev contract (which
  still requires the explicit `DASEIN_BRAIN_DEV_RAW=1` opt-in).
- `schemas/brain-api-dev.schema.json` — the **v0 dev contract the proxy
  speaks today** (docs/brain-serving-v0.md): internal message view in (RAW
  TEXT — our own machines only, gated by `DASEIN_BRAIN_DEV_RAW=1`),
  grid-integer scores + calibrated tau out, with a per-request cross-language
  chunker-parity checksum. Dies when the client featurization port lands.

Rust (`engine`, `proxy`) and Python (`brain`, `trainer`) both generate/validate
against these files; CI fails on drift. Schema `$id`s carry the contract
version — a version bump on tcv2-community must trigger the consent re-prompt
in the plugin.
