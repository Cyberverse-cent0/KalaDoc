//! Cargo integration-test entry point for the PDF engine round-trip
//! suite. The test bodies live next to the engine sources (single-crate
//! layout has no per-module `tests/` discovery) at
//! `src/crates/pdf-engine/tests/pdf_roundtrip.rs`; this shim makes them
//! runnable via `cargo test --test pdf_roundtrip`.

include!("../src/crates/pdf-engine/tests/pdf_roundtrip.rs");
