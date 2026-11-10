//! Cargo integration-test entry point for the PDF engine's performance
//! smoke suite. The test bodies live next to the engine sources at
//! `src/crates/pdf-engine/tests/pdf_perf.rs`; this shim makes them
//! runnable via `cargo test --test pdf_perf`.

include!("../src/crates/pdf-engine/tests/pdf_perf.rs");
