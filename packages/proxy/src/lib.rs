//! parsec-proxy — Anthropic-wire /v1/messages proxy.
//!
//! Ported from `adaptive-context-clean/service`, absorbing the old gateway's
//! upstream bridge as provider adapters (anthropic-passthrough | vertex | byok).
//!
//! Invariants owned here (DIRECTION.md §8):
//! - Fail-open floor: every layer degrades to passthrough on error, and the
//!   fail-open rate is a first-class metric with an alert (§8.3).
//! - Measurement honesty: savings come only from the per-request free
//!   `count_tokens` probe on the original body vs actually-billed usage —
//!   never a modeled baseline (§8.4).
//! - Cloud topology carries BYOK API keys only; subscription OAuth tokens are
//!   never routed through our infrastructure (§3).

pub mod adjudicator;
pub mod apikey;
pub mod brain;
pub mod brand;
pub mod codex;
pub mod counterfact;
pub mod credentials;
pub mod featurize;
pub mod governor;
pub mod hook;
pub mod internal;
pub mod ledger_ship;
pub mod mcp_ext;
pub mod measure;
pub mod noreread;
pub mod openai;
pub mod protect;
pub mod providers;
pub mod responses;
pub mod server;
pub mod setup;
pub mod setup_codex;
pub mod setup_desktop;
pub mod setup_opencode;
pub mod splice;
pub mod statusline;
pub mod supervisor;
pub mod tray;
pub mod trim;
pub mod visibility;
