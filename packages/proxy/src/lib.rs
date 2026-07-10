//! dasein-proxy — Anthropic-wire /v1/messages proxy.
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

pub mod hook;
pub mod measure;
pub mod noreread;
pub mod providers;
pub mod server;
pub mod splice;
pub mod statusline;
