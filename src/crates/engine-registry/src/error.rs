//! Registry error type.

/// Errors produced by [`crate::EngineRegistry`] operations.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// An engine with this name is already registered.
    #[error("engine name {0:?} is already registered")]
    DuplicateName(String),
    /// Attempted to register an engine reporting an empty `name()`.
    #[error("engine name must not be empty")]
    EmptyName,
    /// No engine is registered under the requested name.
    #[error("no engine registered under name {0:?}")]
    NotFound(String),
}
