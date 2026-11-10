//! Cargo integration-test entry point for the PDF engine's advanced
//! operations suite. The test bodies live next to the engine sources
//! (single-crate layout has no per-module `tests/` discovery) at
//! `src/crates/pdf-engine/tests/advanced_ops.rs`; this shim makes them
//! runnable via `cargo test --test advanced_ops`.

include!("../src/crates/pdf-engine/tests/advanced_ops.rs");
