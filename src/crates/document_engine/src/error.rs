//! Error types for document domain validation.

/// Errors produced when constructing or validating document domain objects.
#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
    /// A value object failed validation (e.g. empty or over-long name).
    #[error("invalid {field}: {reason}")]
    Validation {
        field: &'static str,
        reason: String,
    },
}

impl DocumentError {
    pub(crate) fn validation(field: &'static str, reason: impl Into<String>) -> Self {
        Self::Validation {
            field,
            reason: reason.into(),
        }
    }
}
