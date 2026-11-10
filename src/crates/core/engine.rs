//! Shared contracts for processing engines.
//!
//! Every engine (PDF, image, conversion, ...) implements
//! [`ProcessingEngine`]. The engine registry stores trait objects,
//! and the CLI/MCP layers discover capabilities through it — engines
//! never get called directly by adapters.

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

/// Broad category an engine belongs to. Used for filtering in
/// `engine list` / registry lookups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineKind {
    Pdf,
    Image,
    Conversion,
    Security,
    Pipeline,
    Video,
    Multimedia,
}

/// Operations an engine can perform.
///
/// Serialized as `snake_case` in JSON output so CLI/MCP responses are
/// stable and scriptable. Add variants here when a new operation is
/// introduced; engines advertise only the subset they implement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineOperation {
    Merge,
    Split,
    Rotate,
    Resize,
    Adjust,
    Flip,
    Thumbnail,
    ExtractPages,
    RemovePages,
    Reorder,
    Crop,
    Compress,
    Repair,
    Flatten,
    Overlay,
    Watermark,
    Stamp,
    DrawText,
    Redact,
    Sanitize,
    Sign,
    RemoveSignatures,
    VerifySignature,
    Validate,
    Inspect,
    PageCount,
    ExtractMetadata,
    ExtractImages,
    ExtractText,
    Render,
    Compare,
    Convert,
    Ocr,
    Encrypt,
    Decrypt,
    // -- video-engine operations ------------------------------------------
    Trim,
    Transcode,
    Concat,
    ExtractAudio,
    /// Change container without re-encoding streams (`-c copy`).
    Remux,
    /// Apply ffmpeg video/audio filters or a named preset.
    Filter,
    /// Produce a structured media analysis report (scenes, black
    /// frames, silence, loudness, stats).
    Analyze,
    /// Package media for delivery (HLS, DASH, faststart MP4).
    Package,
    /// Extract embedded streams (subtitles, attachments, cover art,
    /// chapters) into separate artifacts.
    ExtractStreams,
    /// Extract, convert, retime, or burn subtitles.
    Subtitles,
    /// Build a video from an image sequence / slideshow.
    Slideshow,
    /// Audio-domain processing: volume, loudness, mixing, fades,
    /// channel layout, noise reduction.
    AudioProcess,
    /// Edit container/stream metadata tags and cover art.
    MetadataEdit,
    /// Objective quality metrics between two media files
    /// (PSNR/SSIM/VMAF where the build supports them).
    Quality,
    /// Enumerate capture devices/sources/hardware accelerators.
    Capture,
    /// Encode with a professional/intermediate profile
    /// (ProRes, DNxHD, FFV1, ...).
    Encode,
    // -- pdf-engine extended operations -----------------------------------
    InsertBlank,
    DuplicatePages,
    RemoveBlankPages,
    Interleave,
    MergeMode,
    Nup,
    SinglePage,
    Posterize,
    Booklet,
    ScaleContent,
    SplitMode,
    SplitAtOutline,
    ListPortfolio,
    CreatePortfolio,
    DrawShapes,
    HeadersFooters,
    AddPageNumbers,
    AddPrintMarks,
    AddBates,
    RedactRegions,
    RedactPages,
    RemoveHiddenText,
    SignerCertificates,
    ParseCertificate,
    PermissionsReport,
    InspectFonts,
    InspectJavascript,
    GetDocumentDates,
    SetDocumentDates,
    ObjectReport,
    FontAudit,
    ContentAudit,
    ListOcMarkedContent,
    InspectStructure,
    SetPageLabels,
    ClearPageLabels,
    GetPageLabels,
    SetOutline,
    RemoveOutline,
    ListOutline,
    AddNamedDestinations,
    ListNamedDestinations,
    RemoveNamedDestination,
    BuildOutlineFromText,
    SetPageRotation,
    GetPageRotations,
    NormalizeRotations,
    SetViewCrop,
    ClearCrop,
    GetPageBoxes,
    SetPageBox,
    RemovePageBox,
    ApplyTrimPreset,
    GetTrimBoxes,
    GetTrimBoxesForPages,
    SetPageSize,
    GetPageSizes,
    DetectOrientation,
    NormalizeOrientation,
    Deskew,
    CleanScan,
    SetTransition,
    ClearTransitions,
    ListTransitions,
    PackageDocuments,
    Linearize,
    Grayscale,
    InvertColors,
    RemoveImages,
    ReplaceImage,
    DedupImages,
    RecompressStreams,
    GarbageCollect,
    MakeSearchable,
    AddComment,
    AddFreeText,
    ListAnnotations,
    RemoveAnnotations,
    ListArticles,
    RemoveArticles,
    AddInk,
    ListInk,
    RemoveInk,
    TextMarkup,
    EditText,
    ListFormFields,
    CreateTextField,
    DeleteFormField,
    FillForm,
    ClearForm,
    UnlockFields,
    LockFields,
    EditFieldProps,
    SetMetadata,
    ClearMetadata,
    SetXmp,
    GetXmp,
    ToPdfa,
    ToPdfua,
    ToPdfx,
    ListSignatureTimestamps,
    HasTimestamps,
    DocumentDigest,
    TimestampQuery,
    BuildTimestampRequest,
    RequestExternalTimestamp,
    TimestampDocument,
    ListLayers,
    SetLayerVisibility,
    RemoveOcProperties,
    AddLinks,
    ListLinks,
    RemoveLinks,
    ListAttachments,
    ExtractAttachment,
    AddAttachment,
    RemoveAttachment,
    RemoveNamedObjects,
    RemoveEmbeddedFiles,
    RemoveJavascriptNames,
}

impl EngineOperation {
    /// Stable string form used in CLI output and job payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Split => "split",
            Self::Rotate => "rotate",
            Self::Resize => "resize",
            Self::Adjust => "adjust",
            Self::Flip => "flip",
            Self::Thumbnail => "thumbnail",
            Self::ExtractPages => "extract_pages",
            Self::RemovePages => "remove_pages",
            Self::Reorder => "reorder",
            Self::Crop => "crop",
            Self::Compress => "compress",
            Self::Repair => "repair",
            Self::Flatten => "flatten",
            Self::Overlay => "overlay",
            Self::Watermark => "watermark",
            Self::Stamp => "stamp",
            Self::DrawText => "draw_text",
            Self::Redact => "redact",
            Self::Sanitize => "sanitize",
            Self::Sign => "sign",
            Self::RemoveSignatures => "remove_signatures",
            Self::VerifySignature => "verify_signature",
            Self::Validate => "validate",
            Self::Inspect => "inspect",
            Self::PageCount => "page_count",
            Self::ExtractMetadata => "extract_metadata",
            Self::ExtractImages => "extract_images",
            Self::ExtractText => "extract_text",
            Self::Render => "render",
            Self::Compare => "compare",
            Self::Convert => "convert",
            Self::Ocr => "ocr",
            Self::Encrypt => "encrypt",
            Self::Decrypt => "decrypt",
    // -- video-engine operations ------------------------------------------
            Self::Trim => "trim",
            Self::Transcode => "transcode",
            Self::Concat => "concat",
            Self::ExtractAudio => "extract_audio",
            Self::Remux => "remux",
            Self::Filter => "filter",
            Self::Analyze => "analyze",
            Self::Package => "package",
            Self::ExtractStreams => "extract_streams",
            Self::Subtitles => "subtitles",
            Self::Slideshow => "slideshow",
            Self::AudioProcess => "audio_process",
            Self::MetadataEdit => "metadata_edit",
            Self::Quality => "quality",
            Self::Capture => "capture",
            Self::Encode => "encode",
            // -- pdf-engine extended operations -------------------------------
            Self::InsertBlank => "insert_blank",
            Self::DuplicatePages => "duplicate_pages",
            Self::RemoveBlankPages => "remove_blank_pages",
            Self::Interleave => "interleave",
            Self::MergeMode => "merge_mode",
            Self::Nup => "nup",
            Self::SinglePage => "single_page",
            Self::Posterize => "posterize",
            Self::Booklet => "booklet",
            Self::ScaleContent => "scale_content",
            Self::SplitMode => "split_mode",
            Self::SplitAtOutline => "split_at_outline",
            Self::ListPortfolio => "list_portfolio",
            Self::CreatePortfolio => "create_portfolio",
            Self::DrawShapes => "draw_shapes",
            Self::HeadersFooters => "headers_footers",
            Self::AddPageNumbers => "add_page_numbers",
            Self::AddPrintMarks => "add_print_marks",
            Self::AddBates => "add_bates",
            Self::RedactRegions => "redact_regions",
            Self::RedactPages => "redact_pages",
            Self::RemoveHiddenText => "remove_hidden_text",
            Self::SignerCertificates => "signer_certificates",
            Self::ParseCertificate => "parse_certificate",
            Self::PermissionsReport => "permissions_report",
            Self::InspectFonts => "inspect_fonts",
            Self::InspectJavascript => "inspect_javascript",
            Self::GetDocumentDates => "get_document_dates",
            Self::SetDocumentDates => "set_document_dates",
            Self::ObjectReport => "object_report",
            Self::FontAudit => "font_audit",
            Self::ContentAudit => "content_audit",
            Self::ListOcMarkedContent => "list_oc_marked_content",
            Self::InspectStructure => "inspect_structure",
            Self::SetPageLabels => "set_page_labels",
            Self::ClearPageLabels => "clear_page_labels",
            Self::GetPageLabels => "get_page_labels",
            Self::SetOutline => "set_outline",
            Self::RemoveOutline => "remove_outline",
            Self::ListOutline => "list_outline",
            Self::AddNamedDestinations => "add_named_destinations",
            Self::ListNamedDestinations => "list_named_destinations",
            Self::RemoveNamedDestination => "remove_named_destination",
            Self::BuildOutlineFromText => "build_outline_from_text",
            Self::SetPageRotation => "set_page_rotation",
            Self::GetPageRotations => "get_page_rotations",
            Self::NormalizeRotations => "normalize_rotations",
            Self::SetViewCrop => "set_view_crop",
            Self::ClearCrop => "clear_crop",
            Self::GetPageBoxes => "get_page_boxes",
            Self::SetPageBox => "set_page_box",
            Self::RemovePageBox => "remove_page_box",
            Self::ApplyTrimPreset => "apply_trim_preset",
            Self::GetTrimBoxes => "get_trim_boxes",
            Self::GetTrimBoxesForPages => "get_trim_boxes_for_pages",
            Self::SetPageSize => "set_page_size",
            Self::GetPageSizes => "get_page_sizes",
            Self::DetectOrientation => "detect_orientation",
            Self::NormalizeOrientation => "normalize_orientation",
            Self::Deskew => "deskew",
            Self::CleanScan => "clean_scan",
            Self::SetTransition => "set_transition",
            Self::ClearTransitions => "clear_transitions",
            Self::ListTransitions => "list_transitions",
            Self::PackageDocuments => "package_documents",
            Self::Linearize => "linearize",
            Self::Grayscale => "grayscale",
            Self::InvertColors => "invert_colors",
            Self::RemoveImages => "remove_images",
            Self::ReplaceImage => "replace_image",
            Self::DedupImages => "dedup_images",
            Self::RecompressStreams => "recompress_streams",
            Self::GarbageCollect => "garbage_collect",
            Self::MakeSearchable => "make_searchable",
            Self::AddComment => "add_comment",
            Self::AddFreeText => "add_freetext",
            Self::ListAnnotations => "list_annotations",
            Self::RemoveAnnotations => "remove_annotations",
            Self::ListArticles => "list_articles",
            Self::RemoveArticles => "remove_articles",
            Self::AddInk => "add_ink",
            Self::ListInk => "list_ink",
            Self::RemoveInk => "remove_ink",
            Self::TextMarkup => "text_markup",
            Self::EditText => "edit_text",
            Self::ListFormFields => "list_form_fields",
            Self::CreateTextField => "create_text_field",
            Self::DeleteFormField => "delete_form_field",
            Self::FillForm => "fill_form",
            Self::ClearForm => "clear_form",
            Self::UnlockFields => "unlock_fields",
            Self::LockFields => "lock_fields",
            Self::EditFieldProps => "edit_field_props",
            Self::SetMetadata => "set_metadata",
            Self::ClearMetadata => "clear_metadata",
            Self::SetXmp => "set_xmp",
            Self::GetXmp => "get_xmp",
            Self::ToPdfa => "to_pdfa",
            Self::ToPdfua => "to_pdfua",
            Self::ToPdfx => "to_pdfx",
            Self::ListSignatureTimestamps => "list_signature_timestamps",
            Self::HasTimestamps => "has_timestamps",
            Self::DocumentDigest => "document_digest",
            Self::TimestampQuery => "timestamp_query",
            Self::BuildTimestampRequest => "build_timestamp_request",
            Self::RequestExternalTimestamp => "request_external_timestamp",
            Self::TimestampDocument => "timestamp_document",
            Self::ListLayers => "list_layers",
            Self::SetLayerVisibility => "set_layer_visibility",
            Self::RemoveOcProperties => "remove_oc_properties",
            Self::AddLinks => "add_links",
            Self::ListLinks => "list_links",
            Self::RemoveLinks => "remove_links",
            Self::ListAttachments => "list_attachments",
            Self::ExtractAttachment => "extract_attachment",
            Self::AddAttachment => "add_attachment",
            Self::RemoveAttachment => "remove_attachment",
            Self::RemoveNamedObjects => "remove_named_objects",
            Self::RemoveEmbeddedFiles => "remove_embedded_files",
            Self::RemoveJavascriptNames => "remove_javascript_names",
        }
    }
}

/// Resource limits an engine enforces on a single operation.
///
/// `None` means the engine does not enforce that limit itself —
/// global application limits still apply upstream.
#[derive(Debug, Clone, Default)]
pub struct EngineLimits {
    /// Maximum size of a single input document in bytes.
    pub max_input_bytes: Option<u64>,
    /// Maximum page count the engine will process.
    pub max_pages: Option<u32>,
    /// Maximum decoded pixel count (width × height) the engine will
    /// materialize for a single image/raster input — a decompression
    /// bomb guard. Ignored by engines without raster inputs.
    pub max_pixels: Option<u64>,
    /// Maximum number of output artifacts produced by one operation.
    pub max_outputs: Option<u32>,
    /// Maximum media duration in seconds an engine will process
    /// (e.g. video/audio length). `None` = no limit. Ignored by
    /// engines without timed media inputs.
    pub max_duration_secs: Option<u64>,
}

/// Static description of what an engine supports.
#[derive(Debug, Clone)]
pub struct EngineCapabilities {
    /// Operations the engine implements. Must be non-empty for a
    /// usable engine.
    pub operations: Vec<EngineOperation>,
    /// Input format identifiers the engine accepts (e.g. `"pdf"`).
    pub input_formats: Vec<String>,
    /// Output format identifiers the engine produces.
    pub output_formats: Vec<String>,
    /// Per-operation resource limits.
    pub limits: EngineLimits,
}

/// Health of an engine at a point in time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineHealthStatus {
    Healthy,
    Degraded,
    Unavailable,
}

/// Result of an engine health check.
#[derive(Debug, Clone)]
pub struct EngineHealth {
    pub status: EngineHealthStatus,
    /// Human-readable detail when not fully healthy. Never contains
    /// secrets, paths outside the storage root, or stack traces.
    pub message: Option<String>,
}

/// Boxed future returned by trait methods — keeps [`ProcessingEngine`]
/// object-safe (`dyn ProcessingEngine`) without an async-trait crate.
/// Same pattern as `job_engine::JobHandler`.
pub type EngineFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Contract every processing engine implements.
///
/// Methods are synchronous where possible (`name`, `capabilities`)
/// because they describe static metadata. `health` is async to allow
/// engines to probe external resources (binaries, services, license
/// state) without blocking the caller.
///
/// Implementations must:
/// - be `Send + Sync` (stored as `Arc<dyn ProcessingEngine>`)
/// - never panic across the trait boundary — report failure via
///   [`EngineHealth`] or their own error type inside operations
/// - report capabilities honestly: an operation listed here must
///   actually work, and unsupported operations must be rejected by
///   the engine, not silently approximated
pub trait ProcessingEngine: Send + Sync {
    /// Stable engine identifier, e.g. `"pdf"`. Unique within a registry.
    fn name(&self) -> &'static str;

    /// Engine version string (semver-ish, informational).
    fn version(&self) -> &'static str;

    /// Category this engine belongs to.
    fn kind(&self) -> EngineKind;

    /// What the engine supports. Called often — keep it cheap and
    /// side-effect free.
    fn capabilities(&self) -> EngineCapabilities;

    /// Point-in-time health check. Must not mutate engine state and
    /// must complete promptly; long probes belong in a background task.
    fn health<'a>(&'a self) -> EngineFuture<'a, EngineHealth>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn engine_limits_default_is_unbounded() {
        let limits = EngineLimits::default();
        assert_eq!(limits.max_input_bytes, None);
        assert_eq!(limits.max_pages, None);
        assert_eq!(limits.max_pixels, None);
        assert_eq!(limits.max_outputs, None);
    }

    #[test]
    fn engine_limits_fields_are_accessible() {
        let limits = EngineLimits {
            max_input_bytes: Some(10 * 1024 * 1024),
            max_pages: Some(500),
            max_pixels: Some(40_000_000),
            max_outputs: Some(8),
            max_duration_secs: None,
        };
        assert_eq!(limits.max_input_bytes, Some(10_485_760));
        assert_eq!(limits.max_pages, Some(500));
        assert_eq!(limits.max_pixels, Some(40_000_000));
        assert_eq!(limits.max_outputs, Some(8));

        // Clone keeps the values (engines share limits via capabilities).
        let cloned = limits.clone();
        assert_eq!(cloned.max_pages, limits.max_pages);
    }

    #[test]
    fn engine_operation_as_str_matches_serde_name() {
        // `as_str` is the stable string form; serde uses snake_case too.
        for op in [
            EngineOperation::Merge,
            EngineOperation::ExtractPages,
            EngineOperation::PageCount,
            EngineOperation::ToPdfa,
            EngineOperation::RemoveJavascriptNames,
        ] {
            let json = serde_json::to_value(op).unwrap();
            assert_eq!(json, json!(op.as_str()));
            // And it round-trips back through Deserialize.
            let back: EngineOperation = serde_json::from_value(json).unwrap();
            assert_eq!(back, op);
        }
    }

    #[test]
    fn engine_kind_and_health_status_serde() {
        assert_eq!(
            serde_json::to_value(EngineKind::Multimedia).unwrap(),
            json!("multimedia")
        );
        let kind: EngineKind = serde_json::from_value(json!("pdf")).unwrap();
        assert_eq!(kind, EngineKind::Pdf);

        assert_eq!(
            serde_json::to_value(EngineHealthStatus::Degraded).unwrap(),
            json!("degraded")
        );
        let status: EngineHealthStatus = serde_json::from_value(json!("unavailable")).unwrap();
        assert_eq!(status, EngineHealthStatus::Unavailable);
    }

    /// Minimal engine proving `ProcessingEngine` is object-safe and
    /// usable as `Arc<dyn ProcessingEngine>`.
    struct DummyEngine;

    impl ProcessingEngine for DummyEngine {
        fn name(&self) -> &'static str {
            "dummy"
        }
        fn version(&self) -> &'static str {
            "0.0.0-test"
        }
        fn kind(&self) -> EngineKind {
            EngineKind::Conversion
        }
        fn capabilities(&self) -> EngineCapabilities {
            EngineCapabilities {
                operations: vec![EngineOperation::Convert],
                input_formats: vec!["in".to_string()],
                output_formats: vec!["out".to_string()],
                limits: EngineLimits {
                    max_input_bytes: Some(1024),
                    ..EngineLimits::default()
                },
            }
        }
        fn health<'a>(&'a self) -> EngineFuture<'a, EngineHealth> {
            Box::pin(async move {
                EngineHealth {
                    status: EngineHealthStatus::Healthy,
                    message: None,
                }
            })
        }
    }

    #[tokio::test]
    async fn processing_engine_is_object_safe() {
        // Compile-time proof the trait is dyn-compatible; the registry
        // stores `Arc<dyn ProcessingEngine>`.
        let engine: Arc<dyn ProcessingEngine> = Arc::new(DummyEngine);

        assert_eq!(engine.name(), "dummy");
        assert_eq!(engine.version(), "0.0.0-test");
        assert_eq!(engine.kind(), EngineKind::Conversion);

        let caps = engine.capabilities();
        assert_eq!(caps.operations, vec![EngineOperation::Convert]);
        assert_eq!(caps.input_formats, vec!["in".to_string()]);
        assert_eq!(caps.limits.max_input_bytes, Some(1024));
        assert_eq!(caps.limits.max_pages, None);

        let health = engine.health().await;
        assert_eq!(health.status, EngineHealthStatus::Healthy);
        assert_eq!(health.message, None);

        // Also works behind a plain Box<dyn ...>.
        let boxed: Box<dyn ProcessingEngine> = Box::new(DummyEngine);
        assert_eq!(boxed.name(), "dummy");
    }
}
