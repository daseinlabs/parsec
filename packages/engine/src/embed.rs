//! Local embedder (ONNX via ort; CPU/MPS, no torch).
//!
//! ⚠️ Checkpoints are matched to the embedder that produced their training
//! vectors. Until the central retrain against the chosen local embedder lands
//! (DIRECTION.md §5, the Pro long pole), this module cannot ship.
