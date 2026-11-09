//! Document format conversion engine.
//!
//! Converts documents between formats in both directions — office
//! formats and markup to PDF, and PDF back out to text/markdown/
//! images — through pluggable [`ConversionBackend`]s: a pure-Rust
//! backend (lopdf) plus detected external tools (pandoc, libreoffice,
//! poppler). [`ConversionEngine`] implements
//! [`ProcessingEngine`](crate::crates::core::engine::ProcessingEngine)
//! with `kind = Conversion` and the `convert` operation.

mod backend;
mod backends;
mod engine;
mod error;
mod model;

pub use backend::{BackendAvailability, ConversionBackend};
pub use engine::ConversionEngine;
pub use error::{ConvertError, ConvertResult};
pub use model::{ConvertOptions, ConvertOutput, ConvertRequest, Format};
