//! Deterministic quantized freezing.
//!
//! Replaces the in-process `served_folds` session dict: keep/cut decisions are
//! append-only and quantized so that served bytes are a pure function of
//! (conversation prefix, checkpoint, config). Byte-identical replay of every
//! previously served turn is the §8.1 golden test.
