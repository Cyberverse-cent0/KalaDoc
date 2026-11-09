//! Engine registry — discovery and capability lookup for processing engines.
//!
//! [`EngineRegistry`] stores `Arc<dyn ProcessingEngine>` instances keyed
//! by engine name. Adapters (CLI, MCP) go through the registry rather
//! than calling engines directly, so capability discovery and health
//! checks live in one place.
//!
//! [`EngineRegistry::builtin`] returns a registry pre-populated with
//! the in-process engines shipped in this crate tree (`pdf`,
//! `security`).

mod error;
mod registry;

pub use error::RegistryError;
pub use registry::{EngineHealthEntry, EngineInfo, EngineRegistry};
