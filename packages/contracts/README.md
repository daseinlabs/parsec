# contracts

Single source of truth for every cross-package, cross-language schema:

- `schemas/tcv2-community.schema.json` — the versioned trace contribution
  contract (Tier 2 featurized traces). Evolves **only with re-consent**.
- `schemas/telemetry-events.schema.json` — Tier 1 metrics events.
- `schemas/savings-ledger.schema.json` — one row per request: count_tokens
  counterfactual + actually-billed usage. The billing basis (Team), the
  upsell proof (Free→Pro), the trust artifact (Enterprise).
- `schemas/brain-api.schema.json` — scoring request/response: chunk vectors +
  structural features in, keep/cut decisions out. Raw text is unrepresentable
  in this schema by construction.

Rust (`engine`, `proxy`) and Python (`brain`, `trainer`) both generate/validate
against these files; CI fails on drift. Schema `$id`s carry the contract
version — a version bump on tcv2-community must trigger the consent re-prompt
in the plugin.
