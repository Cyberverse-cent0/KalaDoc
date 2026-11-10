//! Registry-facing [`ProcessingEngine`] adapter for the security engine.
//!
//! The security engine is not a document transformer — it validates and
//! gates. Exposing it through the engine registry lets the CLI/MCP layers
//! discover its capabilities (`validate` over the sniffable formats) and
//! report health uniformly with the real processing engines.

use std::future::Future;
use std::pin::Pin;

use crate::crates::core::engine::{
    EngineCapabilities, EngineHealth, EngineHealthStatus, EngineKind, EngineLimits,
    EngineOperation, ProcessingEngine,
};

/// Security engine exposed to the [`crate::crates::engine_registry`].
///
/// Stateless: all budgets come from [`crate::crates::security_engine::ResourceLimits`]
/// at the call site, so a single shared instance serves every request.
#[derive(Debug, Default)]
pub struct SecurityEngine;

impl SecurityEngine {
    /// Create the engine. No configuration needed — limits are enforced
    /// per-call by [`crate::crates::security_engine::ResourceLimits`].
    pub fn new() -> Self {
        Self
    }

    /// Validate untrusted bytes as a PDF under the given size cap.
    ///
    /// Equivalent to the free function
    /// [`validate_pdf`](crate::crates::security_engine::validate_pdf);
    /// provided as a method so callers holding a `SecurityEngine` (or
    /// the `ProcessingEngine` trait object) have a uniform entry point.
    pub fn validate(
        &self,
        data: &[u8],
        max_bytes: u64,
    ) -> crate::crates::security_engine::SecurityResult<()> {
        crate::crates::security_engine::validate_pdf(data, max_bytes)
    }
}

impl ProcessingEngine for SecurityEngine {
    fn name(&self) -> &'static str {
        "security"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn kind(&self) -> EngineKind {
        EngineKind::Security
    }

    fn capabilities(&self) -> EngineCapabilities {
        let limits = crate::crates::security_engine::ResourceLimits::default();
        EngineCapabilities {
            // `validate` is the only operation: this engine checks and
            // gates uploads, it does not transform documents.
            operations: vec![EngineOperation::Validate],
            input_formats: vec![
                "pdf".to_string(),
                "png".to_string(),
                "jpeg".to_string(),
                "zip".to_string(),
                "gif".to_string(),
                // Video containers recognized by `sniff_format`.
                "mp4".to_string(),
                "mkv".to_string(),
                "webm".to_string(),
                "avi".to_string(),
                "mpeg-ts".to_string(),
                "mpeg-ps".to_string(),
                "flv".to_string(),
                "asf".to_string(),
                "wmv".to_string(),
                "mpg".to_string(),
                "mpeg".to_string(),
                "m4v".to_string(),
                "ogv".to_string(),
                "mxf".to_string(),
                "f4v".to_string(),
                // Audio containers recognized by `sniff_format`.
                "mp3".to_string(),
                "wav".to_string(),
                "flac".to_string(),
                "ogg".to_string(),
                "aac".to_string(),
                "aiff".to_string(),
                "au".to_string(),
                "amr".to_string(),
                "caf".to_string(),
                "ac3".to_string(),
            ],
            output_formats: Vec::new(),
            limits: EngineLimits {
                max_input_bytes: Some(limits.max_upload_bytes),
                max_pages: limits.max_pages,
                max_pixels: None,
                max_outputs: Some(limits.max_outputs),
                // Security engine sniffs bytes only; it does not decode
                // timed media, so no duration budget applies here.
                max_duration_secs: None,
            },
        }
    }

    fn health<'a>(&'a self) -> Pin<Box<dyn Future<Output = EngineHealth> + Send + 'a>> {
        Box::pin(async move {
            // Pure in-process checks — healthy whenever the crate is
            // compiled in, which it always is.
            EngineHealth {
                status: EngineHealthStatus::Healthy,
                message: None,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_security_capabilities() {
        let e = SecurityEngine::new();
        assert_eq!(e.name(), "security");
        assert_eq!(e.kind(), EngineKind::Security);
        let caps = e.capabilities();
        assert_eq!(caps.operations, vec![EngineOperation::Validate]);
        assert!(caps.input_formats.contains(&"pdf".to_string()));
        assert!(caps.limits.max_input_bytes.is_some());
    }
}
