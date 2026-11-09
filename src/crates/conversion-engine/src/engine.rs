//! Conversion engine facade and `ProcessingEngine` impl.
//!
//! [`ConversionEngine`] holds an ordered list of
//! [`ConversionBackend`]s and dispatches each [`ConvertRequest`] to
//! the first available backend that supports the format pair,
//! falling back to later backends on retryable failures.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::crates::core::engine::{
    EngineCapabilities, EngineFuture, EngineHealth, EngineHealthStatus, EngineKind, EngineLimits,
    EngineOperation, ProcessingEngine,
};

use super::backend::{BackendAvailability, ConversionBackend};
use super::backends::external::ExternalBackendSet;
use super::backends::rust::RustBackend;
use super::error::{ConvertError, ConvertResult};
use super::model::{ConvertOutput, ConvertRequest, Format};

/// Maximum accepted input size: 256 MiB. Larger documents are
/// rejected before any backend runs — backends may enforce tighter
/// per-format limits on top of this.
pub const MAX_INPUT_BYTES: u64 = 256 * 1024 * 1024;

/// Human-readable form of [`MAX_INPUT_BYTES`] for error messages.
const MAX_INPUT_BYTES_LABEL: &str = "256 MiB";

/// All [`Format`] variants, for exhaustive capability/health scans.
/// Kept local because `Format` lives in `model.rs` (owned elsewhere).
const ALL_FORMATS: [Format; 8] = [
    Format::Pdf,
    Format::Txt,
    Format::Markdown,
    Format::Html,
    Format::Png,
    Format::Jpeg,
    Format::Docx,
    Format::Odt,
];

/// Format-conversion engine.
///
/// Backends are tried in registration order; [`ConversionEngine::new`]
/// prefers detected external tools (pandoc, libreoffice, poppler) for
/// fidelity and keeps the pure-Rust backend as the always-available
/// fallback.
pub struct ConversionEngine {
    backends: Vec<Arc<dyn ConversionBackend>>,
}

impl Default for ConversionEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ConversionEngine {
    /// Builds the production engine: detected external backends first
    /// (preferred for fidelity), then the Rust fallback.
    pub fn new() -> Self {
        let mut backends = ExternalBackendSet::detect();
        backends.push(Arc::new(RustBackend::new()));
        Self { backends }
    }

    /// Builds an engine with an explicit backend list, in dispatch
    /// order. Intended for tests and custom wiring.
    pub fn with_backends(backends: Vec<Arc<dyn ConversionBackend>>) -> Self {
        Self { backends }
    }

    /// Registered backends in dispatch order.
    pub fn backends(&self) -> &[Arc<dyn ConversionBackend>] {
        &self.backends
    }

    /// Converts `req.input` from `req.input_format` to
    /// `req.output_format`.
    ///
    /// Dispatch rules:
    /// - input must be non-empty and within [`MAX_INPUT_BYTES`]
    /// - the first *available* backend supporting the pair is tried;
    ///   on `BackendFailed`/`InvalidInput` the next supporting
    ///   backend is tried (fallback chain)
    /// - `TooLarge`/`Timeout`/`Io` are deterministic and abort the
    ///   chain immediately
    /// - a successful result is sanity-checked: non-empty bytes and
    ///   `output_format` matching the request
    pub fn convert(&self, req: &ConvertRequest) -> ConvertResult<ConvertOutput> {
        if req.input.is_empty() {
            return Err(ConvertError::InvalidInput {
                reason: "input is empty".to_string(),
            });
        }
        let actual = req.input.len() as u64;
        if actual > MAX_INPUT_BYTES {
            return Err(ConvertError::TooLarge {
                limit: MAX_INPUT_BYTES_LABEL.to_string(),
                actual,
            });
        }

        // Partition supporting backends by availability, preserving
        // registration order within each group.
        let mut available: Vec<&Arc<dyn ConversionBackend>> = Vec::new();
        let mut first_unavailable: Option<(&Arc<dyn ConversionBackend>, String)> = None;
        for backend in &self.backends {
            if !backend.supports(req.input_format, req.output_format) {
                continue;
            }
            match backend.availability() {
                BackendAvailability::Available { .. } => available.push(backend),
                BackendAvailability::Unavailable { reason } => {
                    if first_unavailable.is_none() {
                        first_unavailable = Some((backend, reason));
                    }
                }
            }
        }

        if available.is_empty() {
            return match first_unavailable {
                Some((backend, reason)) => Err(ConvertError::BackendUnavailable {
                    backend: backend.name().to_string(),
                    reason,
                }),
                None => Err(ConvertError::UnsupportedConversion {
                    from: req.input_format,
                    to: req.output_format,
                }),
            };
        }

        // Fallback chain: retry on backend-reported failures, abort on
        // deterministic ones.
        let mut last_retryable: Option<ConvertError> = None;
        for backend in available {
            match backend.convert(req) {
                Ok(output) => return Self::validate_output(req, output),
                Err(err) => match err {
                    ConvertError::BackendFailed { .. } | ConvertError::InvalidInput { .. } => {
                        last_retryable = Some(err);
                    }
                    // Deterministic failures — retrying cannot help.
                    other => return Err(other),
                },
            }
        }

        Err(last_retryable.unwrap_or_else(|| ConvertError::BackendFailed {
            backend: "engine".to_string(),
            message: "no backend produced a result".to_string(),
        }))
    }

    /// Post-conversion sanity checks on a backend's output.
    fn validate_output(
        req: &ConvertRequest,
        output: ConvertOutput,
    ) -> ConvertResult<ConvertOutput> {
        if output.bytes.is_empty() {
            return Err(ConvertError::BackendFailed {
                backend: output.backend.clone(),
                message: "backend produced empty output".to_string(),
            });
        }
        if output.output_format != req.output_format {
            return Err(ConvertError::BackendFailed {
                backend: output.backend.clone(),
                message: format!(
                    "backend produced {} instead of requested {}",
                    output.output_format.as_str(),
                    req.output_format.as_str()
                ),
            });
        }
        Ok(output)
    }
}

impl ProcessingEngine for ConversionEngine {
    fn name(&self) -> &'static str {
        "convert"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn kind(&self) -> EngineKind {
        EngineKind::Conversion
    }

    fn capabilities(&self) -> EngineCapabilities {
        // Union of formats covered by AVAILABLE backends only — an
        // unavailable backend cannot honor the pair, so advertising it
        // would be dishonest.
        let mut inputs = BTreeSet::new();
        let mut outputs = BTreeSet::new();
        for backend in &self.backends {
            if !matches!(
                backend.availability(),
                BackendAvailability::Available { .. }
            ) {
                continue;
            }
            for &from in &ALL_FORMATS {
                for &to in &ALL_FORMATS {
                    if backend.supports(from, to) {
                        inputs.insert(from.as_str().to_string());
                        outputs.insert(to.as_str().to_string());
                    }
                }
            }
        }

        EngineCapabilities {
            operations: vec![EngineOperation::Convert],
            input_formats: inputs.into_iter().collect(),
            output_formats: outputs.into_iter().collect(),
            // Per-backend limits (external tools bound their own work);
            // the engine-level cap is enforced in `convert` and
            // surfaced via ConvertError::TooLarge.
            limits: EngineLimits::default(),
        }
    }

    fn health<'a>(&'a self) -> EngineFuture<'a, EngineHealth> {
        Box::pin(async move {
            let mut available = 0usize;
            let mut unavailable: Vec<String> = Vec::new();
            for backend in &self.backends {
                match backend.availability() {
                    BackendAvailability::Available { .. } => available += 1,
                    BackendAvailability::Unavailable { reason } => {
                        // Names only — reasons may contain tool-specific
                        // detail; keep the aggregate message path-free.
                        let _ = reason;
                        unavailable.push(backend.name().to_string());
                    }
                }
            }

            let total = self.backends.len();
            if total == 0 || available == 0 {
                return EngineHealth {
                    status: EngineHealthStatus::Unavailable,
                    message: Some(if total == 0 {
                        "no conversion backends registered".to_string()
                    } else {
                        format!("all {} conversion backends unavailable", total)
                    }),
                };
            }
            if unavailable.is_empty() {
                EngineHealth {
                    status: EngineHealthStatus::Healthy,
                    message: None,
                }
            } else {
                EngineHealth {
                    status: EngineHealthStatus::Degraded,
                    message: Some(format!(
                        "{} of {} conversion backends unavailable: {}",
                        unavailable.len(),
                        total,
                        unavailable.join(", ")
                    )),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::conversion_engine::model::ConvertOptions;

    /// Configurable fake backend for dispatch/fallback tests.
    struct MockBackend {
        name: &'static str,
        availability: BackendAvailability,
        /// Supported (from, to) pairs.
        pairs: Vec<(Format, Format)>,
        /// Scripted results, consumed in order; falls back to `default`.
        results: std::sync::Mutex<Vec<ConvertResult<ConvertOutput>>>,
        default: ConvertResult<ConvertOutput>,
        calls: std::sync::Mutex<u32>,
    }

    impl MockBackend {
        fn available(name: &'static str, pairs: Vec<(Format, Format)>) -> Self {
            Self {
                name,
                availability: BackendAvailability::Available { version: None },
                pairs,
                results: std::sync::Mutex::new(Vec::new()),
                default: Ok(ConvertOutput {
                    bytes: vec![1, 2, 3],
                    output_format: Format::Pdf,
                    backend: name.to_string(),
                    warnings: Vec::new(),
                }),
                calls: std::sync::Mutex::new(0),
            }
        }

        fn unavailable(name: &'static str, pairs: Vec<(Format, Format)>, reason: &str) -> Self {
            let mut b = Self::available(name, pairs);
            b.availability = BackendAvailability::Unavailable {
                reason: reason.to_string(),
            };
            b
        }

        fn failing(mut self, err: ConvertError) -> Self {
            self.default = Err(err);
            self
        }

        fn scripted(mut self, results: Vec<ConvertResult<ConvertOutput>>) -> Self {
            *self.results.lock().unwrap() = results;
            self
        }

        fn call_count(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }

    impl ConversionBackend for MockBackend {
        fn name(&self) -> &'static str {
            self.name
        }

        fn availability(&self) -> BackendAvailability {
            self.availability.clone()
        }

        fn supports(&self, from: Format, to: Format) -> bool {
            self.pairs.contains(&(from, to))
        }

        fn convert(&self, req: &ConvertRequest) -> ConvertResult<ConvertOutput> {
            *self.calls.lock().unwrap() += 1;
            if let Some(res) = self.results.lock().unwrap().pop() {
                return res;
            }
            // `default` is Clone-able data; rebuild the result each call.
            match &self.default {
                Ok(out) => Ok(ConvertOutput {
                    bytes: out.bytes.clone(),
                    output_format: out.output_format,
                    backend: out.backend.clone(),
                    warnings: out.warnings.clone(),
                }),
                Err(_) => Err(match &self.default {
                    Err(ConvertError::BackendFailed { backend, message }) => {
                        ConvertError::BackendFailed {
                            backend: backend.clone(),
                            message: message.clone(),
                        }
                    }
                    Err(ConvertError::InvalidInput { reason }) => ConvertError::InvalidInput {
                        reason: reason.clone(),
                    },
                    Err(ConvertError::TooLarge { limit, actual }) => ConvertError::TooLarge {
                        limit: limit.clone(),
                        actual: *actual,
                    },
                    Err(ConvertError::Timeout { backend, seconds }) => ConvertError::Timeout {
                        backend: backend.clone(),
                        seconds: *seconds,
                    },
                    Err(ConvertError::UnsupportedConversion { from, to }) => {
                        ConvertError::UnsupportedConversion {
                            from: *from,
                            to: *to,
                        }
                    }
                    Err(ConvertError::BackendUnavailable { backend, reason }) => {
                        ConvertError::BackendUnavailable {
                            backend: backend.clone(),
                            reason: reason.clone(),
                        }
                    }
                    Err(ConvertError::Io(e)) => ConvertError::Io(std::io::Error::new(
                        e.kind(),
                        e.to_string(),
                    )),
                    Ok(_) => unreachable!(),
                }),
            }
        }
    }

    fn req(input_format: Format, output_format: Format, input: &[u8]) -> ConvertRequest {
        ConvertRequest {
            input_format,
            output_format,
            input: input.to_vec(),
            options: ConvertOptions::default(),
        }
    }

    fn pdf_out(backend: &str) -> ConvertResult<ConvertOutput> {
        Ok(ConvertOutput {
            bytes: vec![0x25, 0x50, 0x44, 0x46],
            output_format: Format::Pdf,
            backend: backend.to_string(),
            warnings: Vec::new(),
        })
    }

    #[test]
    fn dispatch_prefers_first_available_supporting_backend() {
        let first = Arc::new(MockBackend::available(
            "first",
            vec![(Format::Markdown, Format::Pdf)],
        ));
        let second = Arc::new(MockBackend::available(
            "second",
            vec![(Format::Markdown, Format::Pdf)],
        ));
        let engine = ConversionEngine::with_backends(vec![first.clone(), second.clone()]);

        let out = engine
            .convert(&req(Format::Markdown, Format::Pdf, b"# hi"))
            .unwrap();
        assert_eq!(out.backend, "first");
        assert_eq!(first.call_count(), 1);
        assert_eq!(second.call_count(), 0);
    }

    #[test]
    fn unavailable_backend_is_skipped_for_available_one() {
        let dead = Arc::new(MockBackend::unavailable(
            "dead",
            vec![(Format::Markdown, Format::Pdf)],
            "tool missing",
        ));
        let live = Arc::new(MockBackend::available(
            "live",
            vec![(Format::Markdown, Format::Pdf)],
        ));
        let engine = ConversionEngine::with_backends(vec![dead, live.clone()]);

        let out = engine
            .convert(&req(Format::Markdown, Format::Pdf, b"# hi"))
            .unwrap();
        assert_eq!(out.backend, "live");
    }

    #[test]
    fn falls_back_to_next_backend_on_backend_failed() {
        let failing = Arc::new(
            MockBackend::available("failing", vec![(Format::Markdown, Format::Pdf)])
                .failing(ConvertError::BackendFailed {
                    backend: "failing".to_string(),
                    message: "boom".to_string(),
                }),
        );
        let ok = Arc::new(MockBackend::available(
            "ok",
            vec![(Format::Markdown, Format::Pdf)],
        ));
        let engine = ConversionEngine::with_backends(vec![failing.clone(), ok.clone()]);

        let out = engine
            .convert(&req(Format::Markdown, Format::Pdf, b"# hi"))
            .unwrap();
        assert_eq!(out.backend, "ok");
        assert_eq!(failing.call_count(), 1);
        assert_eq!(ok.call_count(), 1);
    }

    #[test]
    fn falls_back_on_invalid_input_too() {
        let bad = Arc::new(
            MockBackend::available("bad", vec![(Format::Markdown, Format::Pdf)]).failing(
                ConvertError::InvalidInput {
                    reason: "unparseable".to_string(),
                },
            ),
        );
        let ok = Arc::new(MockBackend::available(
            "ok",
            vec![(Format::Markdown, Format::Pdf)],
        ));
        let engine = ConversionEngine::with_backends(vec![bad, ok]);

        engine
            .convert(&req(Format::Markdown, Format::Pdf, b"# hi"))
            .unwrap();
    }

    #[test]
    fn does_not_fall_back_on_too_large() {
        let failing = Arc::new(
            MockBackend::available("failing", vec![(Format::Markdown, Format::Pdf)]).failing(
                ConvertError::TooLarge {
                    limit: "1 MiB".to_string(),
                    actual: 2 * 1024 * 1024,
                },
            ),
        );
        let ok = Arc::new(MockBackend::available(
            "ok",
            vec![(Format::Markdown, Format::Pdf)],
        ));
        let engine = ConversionEngine::with_backends(vec![failing.clone(), ok.clone()]);

        let err = engine
            .convert(&req(Format::Markdown, Format::Pdf, b"# hi"))
            .unwrap_err();
        assert!(matches!(err, ConvertError::TooLarge { .. }));
        assert_eq!(ok.call_count(), 0);
    }

    #[test]
    fn does_not_fall_back_on_timeout_or_io() {
        for err in [
            ConvertError::Timeout {
                backend: "b".to_string(),
                seconds: 5,
            },
            ConvertError::Io(std::io::Error::new(std::io::ErrorKind::Other, "disk")),
        ] {
            let failing = Arc::new(
                MockBackend::available("failing", vec![(Format::Markdown, Format::Pdf)])
                    .failing(err),
            );
            let ok = Arc::new(MockBackend::available(
                "ok",
                vec![(Format::Markdown, Format::Pdf)],
            ));
            let engine = ConversionEngine::with_backends(vec![failing, ok.clone()]);
            assert!(engine
                .convert(&req(Format::Markdown, Format::Pdf, b"# hi"))
                .is_err());
            assert_eq!(ok.call_count(), 0);
        }
    }

    #[test]
    fn unsupported_pair_reports_unsupported_conversion() {
        let engine = ConversionEngine::with_backends(vec![Arc::new(MockBackend::available(
            "b",
            vec![(Format::Markdown, Format::Pdf)],
        ))]);

        let err = engine
            .convert(&req(Format::Pdf, Format::Markdown, b"%PDF"))
            .unwrap_err();
        assert!(matches!(
            err,
            ConvertError::UnsupportedConversion {
                from: Format::Pdf,
                to: Format::Markdown
            }
        ));
    }

    #[test]
    fn supporting_but_all_unavailable_reports_backend_unavailable() {
        let engine = ConversionEngine::with_backends(vec![
            Arc::new(MockBackend::unavailable(
                "a",
                vec![(Format::Markdown, Format::Pdf)],
                "missing binary",
            )),
            Arc::new(MockBackend::unavailable(
                "b",
                vec![(Format::Markdown, Format::Pdf)],
                "also missing",
            )),
        ]);

        let err = engine
            .convert(&req(Format::Markdown, Format::Pdf, b"# hi"))
            .unwrap_err();
        match err {
            ConvertError::BackendUnavailable { backend, reason } => {
                assert_eq!(backend, "a");
                assert_eq!(reason, "missing binary");
            }
            other => panic!("expected BackendUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn empty_input_rejected_before_dispatch() {
        let backend = Arc::new(MockBackend::available(
            "b",
            vec![(Format::Markdown, Format::Pdf)],
        ));
        let engine = ConversionEngine::with_backends(vec![backend.clone()]);

        let err = engine
            .convert(&req(Format::Markdown, Format::Pdf, b""))
            .unwrap_err();
        assert!(matches!(err, ConvertError::InvalidInput { .. }));
        assert_eq!(backend.call_count(), 0);
    }

    #[test]
    fn oversized_input_rejected_before_dispatch() {
        let backend = Arc::new(MockBackend::available(
            "b",
            vec![(Format::Markdown, Format::Pdf)],
        ));
        let engine = ConversionEngine::with_backends(vec![backend.clone()]);

        let big = vec![0u8; (MAX_INPUT_BYTES + 1) as usize];
        let err = engine
            .convert(&req(Format::Markdown, Format::Pdf, &big))
            .unwrap_err();
        assert!(matches!(err, ConvertError::TooLarge { .. }));
        assert_eq!(backend.call_count(), 0);
    }

    #[test]
    fn empty_backend_output_is_backend_failed() {
        let bad = Arc::new(MockBackend {
            results: std::sync::Mutex::new(vec![Ok(ConvertOutput {
                bytes: Vec::new(),
                output_format: Format::Pdf,
                backend: "bad".to_string(),
                warnings: Vec::new(),
            })]),
            ..MockBackend::available("bad", vec![(Format::Markdown, Format::Pdf)])
        });
        let engine = ConversionEngine::with_backends(vec![bad]);

        let err = engine
            .convert(&req(Format::Markdown, Format::Pdf, b"# hi"))
            .unwrap_err();
        assert!(matches!(err, ConvertError::BackendFailed { .. }));
    }

    #[test]
    fn output_format_mismatch_is_backend_failed() {
        let bad = Arc::new(MockBackend {
            results: std::sync::Mutex::new(vec![Ok(ConvertOutput {
                bytes: vec![1],
                output_format: Format::Html, // requested Pdf
                backend: "bad".to_string(),
                warnings: Vec::new(),
            })]),
            ..MockBackend::available("bad", vec![(Format::Markdown, Format::Pdf)])
        });
        let engine = ConversionEngine::with_backends(vec![bad]);

        let err = engine
            .convert(&req(Format::Markdown, Format::Pdf, b"# hi"))
            .unwrap_err();
        assert!(matches!(err, ConvertError::BackendFailed { .. }));
    }

    #[test]
    fn capabilities_union_available_backends_only() {
        let engine = ConversionEngine::with_backends(vec![
            Arc::new(MockBackend::available(
                "a",
                vec![(Format::Markdown, Format::Pdf), (Format::Txt, Format::Pdf)],
            )),
            Arc::new(MockBackend::unavailable(
                "dead",
                vec![(Format::Pdf, Format::Png)],
                "gone",
            )),
        ]);

        let caps = engine.capabilities();
        assert_eq!(caps.operations, vec![EngineOperation::Convert]);
        assert_eq!(caps.input_formats, vec!["markdown", "txt"]);
        assert_eq!(caps.output_formats, vec!["pdf"]);
    }

    #[test]
    fn health_aggregation() {
        let healthy = ConversionEngine::with_backends(vec![Arc::new(MockBackend::available(
            "a",
            vec![],
        ))]);
        let h = futures_lite_block_on(healthy.health());
        assert_eq!(h.status, EngineHealthStatus::Healthy);

        let degraded = ConversionEngine::with_backends(vec![
            Arc::new(MockBackend::available("a", vec![])),
            Arc::new(MockBackend::unavailable("b", vec![], "gone")),
        ]);
        let h = futures_lite_block_on(degraded.health());
        assert_eq!(h.status, EngineHealthStatus::Degraded);
        assert!(h.message.unwrap().contains("b"));

        let down = ConversionEngine::with_backends(vec![Arc::new(MockBackend::unavailable(
            "a",
            vec![],
            "gone",
        ))]);
        let h = futures_lite_block_on(down.health());
        assert_eq!(h.status, EngineHealthStatus::Unavailable);
    }

    /// Minimal block_on for tests (avoids a dev-dependency on an
    /// executor crate).
    fn futures_lite_block_on<F: std::future::Future>(fut: F) -> F::Output {
        use std::sync::Arc;
        use std::task::{Context, Poll, Wake, Waker};

        struct Noop;
        impl Wake for Noop {
            fn wake(self: Arc<Self>) {}
        }

        let waker = Waker::from(Arc::new(Noop));
        let mut cx = Context::from_waker(&waker);
        let mut fut = Box::pin(fut);
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }
}
