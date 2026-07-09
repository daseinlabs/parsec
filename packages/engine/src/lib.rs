//! dasein-engine — the client-side deterministic core.
//!
//! Ported from `adaptive-context-clean/adaptive_context/` minus the GNN.
//! Everything here must be a pure function of (conversation prefix, checkpoint
//! version, config): no wall clock, no RNG, no session state. That property is
//! what makes served bytes recomputable, restart-safe, and cache-stable.
//!
//! Definition of done for the port (DIRECTION.md §7b): the two cross-language
//! parity suites — byte-for-byte with the Python reference on freezing,
//! vector-for-vector on featurization.

pub mod chunking;
pub mod embed;
pub mod features;
pub mod freeze;
