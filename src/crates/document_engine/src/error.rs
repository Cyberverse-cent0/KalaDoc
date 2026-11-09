use thiserror::Error;

#[derive(Error, Debug)]
pub enum DocumentError {
    #[error("Document not found")]
    NotFound,
    #[error("Document already exists")]
    AlreadyExists,
    #[error("Invalid document ID")]
    InvalidId,
    #[error("Invalid document metadata")]
    InvalidMetadata,
    #[error("Invalid document content")]
    InvalidContent,
    #[error("Invalid document version")]
    InvalidVersion,
    #[error("Invalid document state")]
    InvalidState,
    #[error("Invalid document operation")]
    InvalidOperation,
    #[error("invalid {field}: {message}")]
    Validation { field: String, message: String },
    #[error("Document storage error: {0}")]
    Storage(#[from] std::io::Error),
}

impl DocumentError {
    /// Builds a [`DocumentError::Validation`] for a rejected field value.
    pub fn validation(field: &str, message: impl Into<String>) -> Self {
        Self::Validation {
            field: field.to_string(),
            message: message.into(),
        }
    }
}
