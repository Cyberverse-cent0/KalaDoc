//! Built-in job handlers registered on the engine at startup.
//!
//! Long-running entry points (e.g. `kaladoc jobs worker`) call
//! [`register_builtin`] before starting the run loop so every known
//! job `kind` has a handler. Add new handlers here as they come
//! online — job kinds are the contract between producers and
//! consumers.
//!
//! # Registered kinds
//!
//! | Kind                    | Payload contract                                              | Purpose |
//! |-------------------------|---------------------------------------------------------------|---------|
//! | `system.ping`           | any (ignored)                                                 | Health check / queue smoke test; always succeeds |
//! | `document.ingest`       | `{document_id, content_b64` or `content`, `file_name?`, `mime_type?}` | Write payload bytes into storage + register artifact (see [`DocumentIngestHandler`]) |
//! | `document.metadata_extract` | `{document_id, artifact_id?}`                             | Sniff format + extract PDF metadata into document metadata (see [`DocumentMetadataExtractHandler`]) |
//! | `document.index`        | `{document_id, artifact_id?}`                                 | Extract searchable text into an `index` metadata field (see [`DocumentIndexHandler`]) |
//! | `thumbnail.generate`    | `{document_id, artifact_id?}`                                 | Extract an embedded PDF thumbnail image into storage (see [`ThumbnailGenerateHandler`]) |
//! | `pdf.inspect`           | `{path, max_bytes?}`                                          | Validate + inspect a PDF under the storage root; writes `<path>.inspect.json` |
//! | `video.probe`           | `{path, max_bytes?}`                                          | Probe a video under the storage root; writes `<path>.probe.json` |
//! | `video.thumbnail`       | `{path, at_secs?, max_bytes?}`                                | Extract a PNG thumbnail frame; writes `<path>.thumb.png` |
//! | `video.transcode`       | `{path, target_ext, max_bytes?}`                              | Transcode to `target_ext`; writes `<path>.<target_ext>` |
//! | `video.trim`            | `{path, start_secs, end_secs?, reencode?, max_bytes?}`        | Trim a video segment; writes `<path>.trim<ext>` |
//! | `video.remux`           | `{path, target_ext, max_bytes?}`                              | Stream-copy remux to `target_ext`; writes `<path>.<target_ext>` |
//! | `video.extract_frames`  | `{path, every_secs?, fps?, all?, keyframes_only?, format?, max_bytes?}` | Extract frames; writes files under `<path>.frames/` |
//! | `video.subtitles_extract` | `{path, track?, format?, max_bytes?}`                       | Extract a subtitle track; writes `<path>.subs.<fmt>` |
//! | `video.analyze`         | `{path, max_bytes?}`                                          | Full media analysis; writes `<path>.analysis.json` |
//! | `video.package_hls`     | `{path, max_bytes?}`                                          | HLS package; writes files under `<path>.hls/` |
//! | `video.metadata_set`    | `{path, tags:{k:v}, max_bytes?}`                              | Set container tags; writes `<path>.tagged<ext>` |
//! | `video.metadata_strip`  | `{path, max_bytes?}`                                          | Strip all metadata; writes `<path>.stripped<ext>` |
//! | `video.extract_streams` | `{path, subtitles?, attachments?, cover_art?, chapters?, data?, subtitle_format?, max_bytes?}` | Extract embedded streams; writes files under `<path>.streams/` |
//! | `video.audio_volume`    | `{path, gain_db, max_bytes?}`                                 | Apply gain in dB; writes `<path>.vol<ext>` |
//! | `video.contact_sheet`   | `{path, cols?, rows?, thumb_width?, interval_secs?, max_bytes?}` | Build a contact-sheet PNG; writes `<path>.sheet.png` |
//! | `video.speed`           | `{path, factor, max_bytes?}`                                  | Change playback speed; writes `<path>.speed<ext>` |
//! | `video.reverse`         | `{path, max_bytes?}`                                          | Reverse the clip; writes `<path>.reverse<ext>` |
//! | `video.loop`            | `{path, count, max_bytes?}`                                   | Loop the clip; writes `<path>.loop<ext>` |
//! | `video.freeze`          | `{path, at_secs, duration_secs, max_bytes?}`                  | Freeze a frame; writes `<path>.freeze<ext>` |
//! | `video.resize`          | `{path, width, height, fit?, max_bytes?}`                     | Resize per fit mode; writes `<path>.resize<ext>` |
//! | `video.crop`            | `{path, x, y, w, h, max_bytes?}`                              | Crop to a rect; writes `<path>.crop<ext>` |
//! | `video.rotate`          | `{path, degrees, max_bytes?}`                                 | Rotate; writes `<path>.rotate<ext>` |
//! | `video.flip`            | `{path, horizontal?, max_bytes?}`                             | Mirror; writes `<path>.flip<ext>` |
//! | `video.gif`             | `{path, fps?, width?, max_bytes?}`                            | Video → animated GIF; writes `<path>.gif` |
//! | `video.slideshow`       | `{paths:[...], fps?, duration_per_image?, target_ext, max_bytes?}` | Images → video; writes `slideshow.<target_ext>` next to the first input |
//! | `video.normalize`       | `{path, target_lufs?, max_bytes?}`                            | EBU R128 loudness normalize; writes `<path>.norm<ext>` |
//! | `video.loudness`        | `{path, max_bytes?}`                                          | Measure loudness; writes `<path>.loudness.json` |
//! | `video.scene_frames`    | `{path, threshold?, max_bytes?}`                              | Scene-change keyframes; writes files under `<path>.scenes/` |
//! | `video.merge_av`        | `{video_path, audio_path, target_ext?, max_bytes?}`           | Merge audio into video; writes `<video_path>.merged<ext>` |
//! | `video.validate`        | `{path, max_bytes?}`                                          | Full validation report; writes `<path>.validate.json` |
//! | `video.chapters_list`   | `{path, max_bytes?}`                                          | List embedded chapters; writes `<path>.chapters.json` |
//! | `video.chapters_set`    | `{path, chapters:[{start_ms,end_ms,title?}], max_bytes?}`     | Replace chapter table; writes `<path>.chapters<ext>` |
//! | `video.chapters_shift`  | `{path, offset_ms, max_bytes?}`                               | Shift all chapters; writes `<path>.chapters_shift<ext>` |
//! | `video.chapters_remove` | `{path, max_bytes?}`                                          | Drop all chapters; writes `<path>.chapters_remove<ext>` |
//! | `video.hdr_detect`      | `{path, max_bytes?}`                                          | HDR probe; writes `<path>.hdr.json` |
//! | `video.hdr_tone_map`    | `{path, desat?, peak?, max_bytes?}`                           | HDR→SDR tone map; writes `<path>.tonemap<ext>` |
//! | `video.segment_split`   | `{path, mode?, duration_secs?, threshold?, parts?, format?, max_bytes?}` | Split into segments (`duration`/`scenes`/`equal`); writes files under `<path>.segments/` |
//! | `video.segment_extract` | `{path, start_ms, end_ms, format?, max_bytes?}`               | Cut `[start_ms,end_ms)`; writes `<path>.extract.<format>` |
//! | `video.waveform`        | `{path, width?, height?, color?, scale?, max_bytes?}`         | Audio waveform PNG; writes `<path>.waveform.png` |
//! | `video.spectrogram`     | `{path, width?, height?, color?, scale?, max_bytes?}`         | Spectrogram PNG; writes `<path>.spectrogram.png` |
//! | `video.vectorscope`     | `{path, width?, height?, color?, scale?, max_bytes?}`         | Stereo vectorscope PNG; writes `<path>.vectorscope.png` |
//! | `video.sync_report`     | `{path, max_bytes?}`                                          | A/V start-time report; writes `<path>.sync.json` |
//! | `video.sync_fix`        | `{path, audio_offset_ms?, video_offset_ms?, max_bytes?}`      | Manual A/V shift; writes `<path>.syncfix<ext>` |
//! | `video.sync_auto`       | `{path, max_bytes?}`                                          | Automatic A/V align; writes `<path>.syncauto<ext>` |
//! | `video.stabilize`       | `{path, shakiness?, smoothing?, algo?, auto?, max_bytes?}`    | Deshake/vidstab stabilize; writes `<path>.stabilize<ext>` |
//! | `video.preview_gif`     | `{path, max_duration_secs?, fps?, width?, max_bytes?}`        | Animated GIF preview; writes `<path>.preview.gif` |
//! | `video.preview_webp`    | `{path, max_duration_secs?, fps?, width?, max_bytes?}`        | Animated WebP preview; writes `<path>.preview.webp` |
//! | `video.proxy_generate`  | `{path, kind?, target_height?, crf?, preset?, max_bytes?}`    | Proxy/mezzanine/archival derive; writes `<path>.<kind>.<ext>` |
//! | `video.stream_select`   | `{path, keep_video?, audio_ordinals?, subtitle_ordinals?, drop_attachments?, target_ext?, max_bytes?}` | Map/select streams; writes `<path>.select<ext>` |
//! | `video.stream_cover_art`| `{path, cover_path, max_bytes?}`                              | Attach cover art (audio containers); writes `<path>.cover<ext>` |
//! | `video.filters_apply`   | `{path, preset?, video_filter?, audio_filter?, max_bytes?}`   | Apply named preset or raw `-vf`/`-af`; writes `<path>.filters<ext>` |
//! | `storage.gc_staging`    | `{retention_secs?}`                                           | Delete abandoned entries under `<storage_root>/staging/` and `<storage_root>/.staging/` older than the retention window |
//!
//! `document.metadata_extract` / `document.index` / `thumbnail.generate`
//! payload details: `document_id` is required; `artifact_id` is
//! optional and selects a specific artifact — when omitted the
//! **most recently created** artifact of the document is used
//! (`list_artifacts` order, last entry). Artifact bytes are read
//! through the streaming backend with a hard cap of
//! [`MAX_PROCESSING_BYTES`] (64 MiB); larger objects are recorded as
//! `skipped_too_large` rather than read into memory. All three
//! handlers are idempotent: they overwrite deterministic metadata
//! fields and storage object names, so job retries and crash-recovery
//! replays are safe.
//!
//! # Processing status tracking
//!
//! These handlers report per-document progress through **document
//! metadata custom fields** (queryable via
//! `store::metadata::get_document_metadata`), backed by audit events:
//!
//! - `processing_status` — status of the *most recently run*
//!   processing job: `queued` (set by the `enqueue_*` helpers,
//!   best-effort), `running`, then a terminal value: `succeeded`,
//!   `failed`, `unsupported`, `unavailable`, or `skipped_too_large`.
//! - `{op}_status` — the same terminal vocabulary scoped to one
//!   operation: `metadata_extract_status`, `index_status`,
//!   `thumbnail_status`. This is the field to check for "did indexing
//!   work on this document".
//! - Audit events: [`DocumentAuditEventType::ProcessingStarted`] when
//!   a handler begins, `ProcessingCompleted` on success,
//!   `ProcessingFailed` only when the job exhausts `max_attempts`
//!   (intermediate retries stay `running` — the job row in
//!   `jobs_tbl` remains the authoritative retry state).
//!
//! The `jobs_tbl` row (`pending`/`running`/`succeeded`/`failed`) is
//! the source of truth for *execution*; the metadata fields above are
//! the source of truth for *document-level outcome* and survive job
//! row cleanup.
//!
//! Use the [`enqueue_ingest`], [`enqueue_metadata_extract`],
//! [`enqueue_index`], and [`enqueue_thumbnail`] helpers to produce
//! jobs — they build the documented payload contracts and stamp
//! `processing_status = "queued"` (best-effort) so producers never
//! hand-craft JSON.
//!
//! `pdf.inspect` payload details: `path` is a **relative** path under
//! the configured storage root (`[storage].root_dir` /
//! `KALADOC_STORAGE_DIR`); absolute paths and `..` components are
//! rejected. `max_bytes` defaults to 100 MiB and bounds both the
//! on-disk size check (before reading) and `security_engine`'s
//! validation. The inspection report (`PdfInfo` as pretty JSON) is
//! written atomically to `<root>/<path>.inspect.json` via a `*.tmp`
//! sibling + rename.
//!
//! `video.*` payload details: `path` is a **relative** path under the
//! configured storage root, confined exactly like `pdf.inspect`.
//! `max_bytes` defaults to 4 GiB and is checked against the on-disk
//! size before the file is read (and again after, as a TOCTOU guard);
//! it is also passed to the engine as `EngineLimits::max_input_bytes`.
//! `video.probe` writes the `VideoInfo` report as pretty JSON to
//! `<root>/<path>.probe.json`; `video.thumbnail` writes PNG bytes to
//! `<path>.thumb.png` (`at_secs` selects the frame, `None` = default);
//! `video.transcode` writes `<path>.<target_ext>` where `target_ext`
//! is a bare alphanumeric extension and is rejected if the output
//! name would equal the input; `video.trim` writes
//! `<path>.trim.<input-ext>` (`end_secs`/`reencode` optional). All
//! outputs are written atomically via `*.tmp` + rename and all four
//! kinds are idempotent — deterministic output names make retries
//! safe.
//!
//! The extended `video.*` kinds follow the same contract. Single-file
//! transforms write `<path>.<op>.<input-ext>` (e.g. `.vol.mp4`,
//! `.speed.mp4`); `video.remux` writes `<path>.<target_ext>` like
//! `video.transcode`; report jobs write pretty JSON
//! (`.analysis.json`, `.loudness.json`); multi-file jobs write a
//! directory of sanitized relative names (`<path>.frames/`,
//! `<path>.hls/`, `<path>.streams/`, `<path>.scenes/`). `video.gif`
//! writes `<path>.gif`, `video.subtitles_extract` writes
//! `<path>.subs.<fmt>`, and `video.slideshow` writes
//! `slideshow.<target_ext>` in the first input's directory.
//! `video.merge_av` reads two confined paths (`video_path`,
//! `audio_path`) and writes `<video_path>.merged<ext>`.
//!
//! `storage.gc_staging` payload details: `retention_secs` defaults to
//! 86 400 (24 h). Every entry directly under `staging/` **and** the
//! streaming backend's reserved `.staging/` directory whose
//! `modified()` time is older than the window is removed —
//! directories via `remove_dir_all`, everything else via
//! `remove_file`. Entries whose metadata cannot be read are skipped;
//! a missing `staging/` directory is a no-op success.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::config::AppConfig;
use crate::crates::document_engine::artifact::{
    ChecksumAlgorithm, DocumentArtifact, DocumentChecksum,
};
use crate::crates::document_engine::audit::DocumentAuditEventType;
use crate::crates::document_engine::ids::DocumentId;
use crate::crates::document_engine::metadata::DocumentMetadata;
use crate::crates::document_engine::store::artifacts::{
    NewArtifact, download_artifact, list_artifacts, upload_artifact,
};
use crate::crates::document_engine::store::documents::get_document;
use crate::crates::document_engine::store::history::record_event;
use crate::crates::document_engine::store::metadata::{
    get_document_metadata, upsert_document_metadata,
};
use crate::crates::job_engine::{
    Job, JobContext, JobEngine, JobError, JobHandler, NewJob, enqueue,
};
use crate::crates::security_engine::{SniffedFormat, sniff_format};
use crate::crates::storage::local::config::StorageConfig;
use crate::crates::storage::local::debug_when_full::{FaultInjectingFs, StorageError};
use crate::crates::storage::stream::{StreamUpload, StreamingBackend};

/// Job kind for [`DocumentIngestHandler`].
pub const DOCUMENT_INGEST_KIND: &str = "document.ingest";

/// Job kind for [`SystemPingHandler`] — health check / smoke test.
pub const SYSTEM_PING_KIND: &str = "system.ping";

/// Job kind for [`PdfInspectHandler`].
pub const PDF_INSPECT_KIND: &str = "pdf.inspect";

/// Job kind for [`StorageGcStagingHandler`].
pub const STORAGE_GC_STAGING_KIND: &str = "storage.gc_staging";

/// Job kind for [`DocumentMetadataExtractHandler`].
pub const DOCUMENT_METADATA_EXTRACT_KIND: &str = "document.metadata_extract";

/// Job kind for [`DocumentIndexHandler`].
pub const DOCUMENT_INDEX_KIND: &str = "document.index";

/// Job kind for [`ThumbnailGenerateHandler`].
pub const THUMBNAIL_GENERATE_KIND: &str = "thumbnail.generate";

/// Job kind for [`VideoProbeHandler`].
pub const VIDEO_PROBE_KIND: &str = "video.probe";

/// Job kind for [`VideoThumbnailHandler`].
pub const VIDEO_THUMBNAIL_KIND: &str = "video.thumbnail";

/// Job kind for [`VideoTranscodeHandler`].
pub const VIDEO_TRANSCODE_KIND: &str = "video.transcode";

/// Job kind for [`VideoTrimHandler`].
pub const VIDEO_TRIM_KIND: &str = "video.trim";

/// Job kind for [`VideoRemuxHandler`].
pub const VIDEO_REMUX_KIND: &str = "video.remux";

/// Job kind for [`VideoExtractFramesHandler`].
pub const VIDEO_EXTRACT_FRAMES_KIND: &str = "video.extract_frames";

/// Job kind for [`VideoSubtitlesExtractHandler`].
pub const VIDEO_SUBTITLES_EXTRACT_KIND: &str = "video.subtitles_extract";

/// Job kind for [`VideoAnalyzeHandler`].
pub const VIDEO_ANALYZE_KIND: &str = "video.analyze";

/// Job kind for [`VideoPackageHlsHandler`].
pub const VIDEO_PACKAGE_HLS_KIND: &str = "video.package_hls";

/// Job kind for [`VideoMetadataSetHandler`].
pub const VIDEO_METADATA_SET_KIND: &str = "video.metadata_set";

/// Job kind for [`VideoMetadataStripHandler`].
pub const VIDEO_METADATA_STRIP_KIND: &str = "video.metadata_strip";

/// Job kind for [`VideoExtractStreamsHandler`].
pub const VIDEO_EXTRACT_STREAMS_KIND: &str = "video.extract_streams";

/// Job kind for [`VideoAudioVolumeHandler`].
pub const VIDEO_AUDIO_VOLUME_KIND: &str = "video.audio_volume";

/// Job kind for [`VideoContactSheetHandler`].
pub const VIDEO_CONTACT_SHEET_KIND: &str = "video.contact_sheet";

/// Job kind for [`VideoSpeedHandler`].
pub const VIDEO_SPEED_KIND: &str = "video.speed";

/// Job kind for [`VideoReverseHandler`].
pub const VIDEO_REVERSE_KIND: &str = "video.reverse";

/// Job kind for [`VideoLoopHandler`].
pub const VIDEO_LOOP_KIND: &str = "video.loop";

/// Job kind for [`VideoFreezeHandler`].
pub const VIDEO_FREEZE_KIND: &str = "video.freeze";

/// Job kind for [`VideoResizeHandler`].
pub const VIDEO_RESIZE_KIND: &str = "video.resize";

/// Job kind for [`VideoCropHandler`].
pub const VIDEO_CROP_KIND: &str = "video.crop";

/// Job kind for [`VideoRotateHandler`].
pub const VIDEO_ROTATE_KIND: &str = "video.rotate";

/// Job kind for [`VideoFlipHandler`].
pub const VIDEO_FLIP_KIND: &str = "video.flip";

/// Job kind for [`VideoGifHandler`].
pub const VIDEO_GIF_KIND: &str = "video.gif";

/// Job kind for [`VideoSlideshowHandler`].
pub const VIDEO_SLIDESHOW_KIND: &str = "video.slideshow";

/// Job kind for [`VideoNormalizeHandler`].
pub const VIDEO_NORMALIZE_KIND: &str = "video.normalize";

/// Job kind for [`VideoLoudnessHandler`].
pub const VIDEO_LOUDNESS_KIND: &str = "video.loudness";

/// Job kind for [`VideoSceneFramesHandler`].
pub const VIDEO_SCENE_FRAMES_KIND: &str = "video.scene_frames";

/// Job kind for [`VideoMergeAvHandler`].
pub const VIDEO_MERGE_AV_KIND: &str = "video.merge_av";

/// Job kind for [`VideoValidateHandler`].
pub const VIDEO_VALIDATE_KIND: &str = "video.validate";

/// Job kind for [`VideoChaptersListHandler`].
pub const VIDEO_CHAPTERS_LIST_KIND: &str = "video.chapters_list";

/// Job kind for [`VideoChaptersSetHandler`].
pub const VIDEO_CHAPTERS_SET_KIND: &str = "video.chapters_set";

/// Job kind for [`VideoChaptersShiftHandler`].
pub const VIDEO_CHAPTERS_SHIFT_KIND: &str = "video.chapters_shift";

/// Job kind for [`VideoChaptersRemoveHandler`].
pub const VIDEO_CHAPTERS_REMOVE_KIND: &str = "video.chapters_remove";

/// Job kind for [`VideoHdrDetectHandler`].
pub const VIDEO_HDR_DETECT_KIND: &str = "video.hdr_detect";

/// Job kind for [`VideoHdrToneMapHandler`].
pub const VIDEO_HDR_TONE_MAP_KIND: &str = "video.hdr_tone_map";

/// Job kind for [`VideoSegmentSplitHandler`].
pub const VIDEO_SEGMENT_SPLIT_KIND: &str = "video.segment_split";

/// Job kind for [`VideoSegmentExtractHandler`].
pub const VIDEO_SEGMENT_EXTRACT_KIND: &str = "video.segment_extract";

/// Job kind for [`VideoWaveformHandler`].
pub const VIDEO_WAVEFORM_KIND: &str = "video.waveform";

/// Job kind for [`VideoSpectrogramHandler`].
pub const VIDEO_SPECTROGRAM_KIND: &str = "video.spectrogram";

/// Job kind for [`VideoVectorscopeHandler`].
pub const VIDEO_VECTORSCOPE_KIND: &str = "video.vectorscope";

/// Job kind for [`VideoSyncReportHandler`].
pub const VIDEO_SYNC_REPORT_KIND: &str = "video.sync_report";

/// Job kind for [`VideoSyncFixHandler`].
pub const VIDEO_SYNC_FIX_KIND: &str = "video.sync_fix";

/// Job kind for [`VideoSyncAutoHandler`].
pub const VIDEO_SYNC_AUTO_KIND: &str = "video.sync_auto";

/// Job kind for [`VideoStabilizeHandler`].
pub const VIDEO_STABILIZE_KIND: &str = "video.stabilize";

/// Job kind for [`VideoPreviewGifHandler`].
pub const VIDEO_PREVIEW_GIF_KIND: &str = "video.preview_gif";

/// Job kind for [`VideoPreviewWebpHandler`].
pub const VIDEO_PREVIEW_WEBP_KIND: &str = "video.preview_webp";

/// Job kind for [`VideoProxyGenerateHandler`].
pub const VIDEO_PROXY_GENERATE_KIND: &str = "video.proxy_generate";

/// Job kind for [`VideoStreamSelectHandler`].
pub const VIDEO_STREAM_SELECT_KIND: &str = "video.stream_select";

/// Job kind for [`VideoStreamCoverArtHandler`].
pub const VIDEO_STREAM_COVER_ART_KIND: &str = "video.stream_cover_art";

/// Job kind for [`VideoFiltersApplyHandler`].
pub const VIDEO_FILTERS_APPLY_KIND: &str = "video.filters_apply";

/// Default `max_bytes` for `pdf.inspect` (100 MiB).
const DEFAULT_INSPECT_MAX_BYTES: u64 = 100 * 1024 * 1024;

/// Default `max_bytes` for the `video.*` handlers (4 GiB).
const DEFAULT_VIDEO_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Hard cap on artifact bytes read for `document.metadata_extract`,
/// `document.index`, and `thumbnail.generate`. Larger objects are
/// recorded as `skipped_too_large` instead of being buffered.
const MAX_PROCESSING_BYTES: u64 = 64 * 1024 * 1024;

/// Bytes read first when sniffing an artifact's format.
const SNIFF_BYTES: usize = 4096;

/// Maximum characters of extracted text kept in the `index` metadata
/// field's `excerpt` member (the full-text hash/counts are always
/// recorded, so the excerpt is a preview, not the index itself).
const INDEX_EXCERPT_CHARS: usize = 50_000;

/// Chunk size for streaming uploads (`document.ingest`, thumbnails).
const UPLOAD_CHUNK_BYTES: usize = 64 * 1024;

/// Maximum decoded thumbnail raster (width × height pixels) — 16 MP
/// bounds decoded RGB samples at ~48 MiB.
const MAX_THUMBNAIL_PIXELS: u64 = 16_000_000;

/// Maximum decoded thumbnail payload accepted from a PDF image stream.
const MAX_THUMBNAIL_DECODED_BYTES: usize = 64 * 1024 * 1024;

/// Default `max_attempts` used by the `enqueue_*` helpers.
const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// Default `retention_secs` for `storage.gc_staging` (24 h).
const DEFAULT_GC_RETENTION_SECS: u64 = 86_400;

/// Registers all built-in job handlers on `engine`.
///
/// Handlers that need application config are constructed here; a
/// config that fails to load logs a warning and skips that handler so
/// the engine can still start (its jobs will fail with `NoHandler`).
/// Config-dependent handlers (`document.ingest`) resolve storage at
/// construction; `pdf.inspect` / `video.*` / `storage.gc_staging`
/// resolve it per job so a fixed storage root can be picked up
/// without a restart.
pub fn register_builtin(engine: &mut JobEngine) {
    match DocumentIngestHandler::from_app_config() {
        Ok(handler) => engine.register(DOCUMENT_INGEST_KIND, Arc::new(handler)),
        Err(e) => tracing::warn!(
            error = %e,
            "skipping document.ingest handler: storage config unavailable"
        ),
    }
    engine.register(SYSTEM_PING_KIND, Arc::new(SystemPingHandler));
    engine.register(PDF_INSPECT_KIND, Arc::new(PdfInspectHandler));
    engine.register(VIDEO_PROBE_KIND, Arc::new(VideoProbeHandler));
    engine.register(VIDEO_THUMBNAIL_KIND, Arc::new(VideoThumbnailHandler));
    engine.register(VIDEO_TRANSCODE_KIND, Arc::new(VideoTranscodeHandler));
    engine.register(VIDEO_TRIM_KIND, Arc::new(VideoTrimHandler));
    engine.register(VIDEO_REMUX_KIND, Arc::new(VideoRemuxHandler));
    engine.register(
        VIDEO_EXTRACT_FRAMES_KIND,
        Arc::new(VideoExtractFramesHandler),
    );
    engine.register(
        VIDEO_SUBTITLES_EXTRACT_KIND,
        Arc::new(VideoSubtitlesExtractHandler),
    );
    engine.register(VIDEO_ANALYZE_KIND, Arc::new(VideoAnalyzeHandler));
    engine.register(VIDEO_PACKAGE_HLS_KIND, Arc::new(VideoPackageHlsHandler));
    engine.register(VIDEO_METADATA_SET_KIND, Arc::new(VideoMetadataSetHandler));
    engine.register(
        VIDEO_METADATA_STRIP_KIND,
        Arc::new(VideoMetadataStripHandler),
    );
    engine.register(
        VIDEO_EXTRACT_STREAMS_KIND,
        Arc::new(VideoExtractStreamsHandler),
    );
    engine.register(VIDEO_AUDIO_VOLUME_KIND, Arc::new(VideoAudioVolumeHandler));
    engine.register(VIDEO_CONTACT_SHEET_KIND, Arc::new(VideoContactSheetHandler));
    engine.register(VIDEO_SPEED_KIND, Arc::new(VideoSpeedHandler));
    engine.register(VIDEO_REVERSE_KIND, Arc::new(VideoReverseHandler));
    engine.register(VIDEO_LOOP_KIND, Arc::new(VideoLoopHandler));
    engine.register(VIDEO_FREEZE_KIND, Arc::new(VideoFreezeHandler));
    engine.register(VIDEO_RESIZE_KIND, Arc::new(VideoResizeHandler));
    engine.register(VIDEO_CROP_KIND, Arc::new(VideoCropHandler));
    engine.register(VIDEO_ROTATE_KIND, Arc::new(VideoRotateHandler));
    engine.register(VIDEO_FLIP_KIND, Arc::new(VideoFlipHandler));
    engine.register(VIDEO_GIF_KIND, Arc::new(VideoGifHandler));
    engine.register(VIDEO_SLIDESHOW_KIND, Arc::new(VideoSlideshowHandler));
    engine.register(VIDEO_NORMALIZE_KIND, Arc::new(VideoNormalizeHandler));
    engine.register(VIDEO_LOUDNESS_KIND, Arc::new(VideoLoudnessHandler));
    engine.register(VIDEO_SCENE_FRAMES_KIND, Arc::new(VideoSceneFramesHandler));
    engine.register(VIDEO_MERGE_AV_KIND, Arc::new(VideoMergeAvHandler));
    engine.register(VIDEO_VALIDATE_KIND, Arc::new(VideoValidateHandler));
    engine.register(VIDEO_CHAPTERS_LIST_KIND, Arc::new(VideoChaptersListHandler));
    engine.register(VIDEO_CHAPTERS_SET_KIND, Arc::new(VideoChaptersSetHandler));
    engine.register(
        VIDEO_CHAPTERS_SHIFT_KIND,
        Arc::new(VideoChaptersShiftHandler),
    );
    engine.register(
        VIDEO_CHAPTERS_REMOVE_KIND,
        Arc::new(VideoChaptersRemoveHandler),
    );
    engine.register(VIDEO_HDR_DETECT_KIND, Arc::new(VideoHdrDetectHandler));
    engine.register(VIDEO_HDR_TONE_MAP_KIND, Arc::new(VideoHdrToneMapHandler));
    engine.register(VIDEO_SEGMENT_SPLIT_KIND, Arc::new(VideoSegmentSplitHandler));
    engine.register(
        VIDEO_SEGMENT_EXTRACT_KIND,
        Arc::new(VideoSegmentExtractHandler),
    );
    engine.register(VIDEO_WAVEFORM_KIND, Arc::new(VideoWaveformHandler));
    engine.register(VIDEO_SPECTROGRAM_KIND, Arc::new(VideoSpectrogramHandler));
    engine.register(VIDEO_VECTORSCOPE_KIND, Arc::new(VideoVectorscopeHandler));
    engine.register(VIDEO_SYNC_REPORT_KIND, Arc::new(VideoSyncReportHandler));
    engine.register(VIDEO_SYNC_FIX_KIND, Arc::new(VideoSyncFixHandler));
    engine.register(VIDEO_SYNC_AUTO_KIND, Arc::new(VideoSyncAutoHandler));
    engine.register(VIDEO_STABILIZE_KIND, Arc::new(VideoStabilizeHandler));
    engine.register(VIDEO_PREVIEW_GIF_KIND, Arc::new(VideoPreviewGifHandler));
    engine.register(VIDEO_PREVIEW_WEBP_KIND, Arc::new(VideoPreviewWebpHandler));
    engine.register(
        VIDEO_PROXY_GENERATE_KIND,
        Arc::new(VideoProxyGenerateHandler),
    );
    engine.register(VIDEO_STREAM_SELECT_KIND, Arc::new(VideoStreamSelectHandler));
    engine.register(
        VIDEO_STREAM_COVER_ART_KIND,
        Arc::new(VideoStreamCoverArtHandler),
    );
    engine.register(VIDEO_FILTERS_APPLY_KIND, Arc::new(VideoFiltersApplyHandler));
    engine.register(STORAGE_GC_STAGING_KIND, Arc::new(StorageGcStagingHandler));
    match DocumentMetadataExtractHandler::from_app_config() {
        Ok(handler) => engine.register(DOCUMENT_METADATA_EXTRACT_KIND, Arc::new(handler)),
        Err(e) => tracing::warn!(
            error = %e,
            "skipping document.metadata_extract handler: storage config unavailable"
        ),
    }
    match DocumentIndexHandler::from_app_config() {
        Ok(handler) => engine.register(DOCUMENT_INDEX_KIND, Arc::new(handler)),
        Err(e) => tracing::warn!(
            error = %e,
            "skipping document.index handler: storage config unavailable"
        ),
    }
    match ThumbnailGenerateHandler::from_app_config() {
        Ok(handler) => engine.register(THUMBNAIL_GENERATE_KIND, Arc::new(handler)),
        Err(e) => tracing::warn!(
            error = %e,
            "skipping thumbnail.generate handler: storage config unavailable"
        ),
    }
}

/// `document.ingest` — writes job payload bytes into the configured
/// storage backend and registers the result as a document artifact.
///
/// Payload contract (`jobs_tbl.payload` JSON):
///
/// | Field          | Required | Meaning                                            |
/// |----------------|----------|----------------------------------------------------|
/// | `document_id`  | yes      | Existing document the artifact belongs to          |
/// | `content_b64`  | one of   | Base64-encoded file bytes                          |
/// | `content`      | one of   | UTF-8 text stored as the file bytes                |
/// | `file_name`    | no       | Stored name, default `"upload.bin"`                |
/// | `mime_type`    | no       | Recorded on the artifact row                       |
///
/// Bytes land at `documents/{document_id}/{file_name}` under the
/// storage root; `storage_uri` on the artifact row is
/// `local://documents/{document_id}/{file_name}`.
///
/// Idempotent: the storage object name is deterministic (re-writing
/// replaces content) and an artifact row with the same `storage_uri`
/// is not duplicated on retry.
pub struct DocumentIngestHandler {
    storage: StorageConfig,
}

impl DocumentIngestHandler {
    /// Builds the handler from the application's storage configuration
    /// (`AppConfig::load_lenient`, so missing DB secrets don't matter —
    /// only the `[storage]` section and `KALADOC_STORAGE_*` env vars).
    pub fn from_app_config() -> Result<Self, crate::config::ConfigError> {
        Ok(Self::with_storage_config(
            AppConfig::load_lenient()?.to_storage_config(),
        ))
    }

    /// Builds the handler around an explicit storage config (tests).
    pub fn with_storage_config(storage: StorageConfig) -> Self {
        Self { storage }
    }
}

impl JobHandler for DocumentIngestHandler {
    fn handle<'a>(
        &'a self,
        ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let payload = IngestPayload::parse(&job.payload)?;
            let document_id = DocumentId::new(payload.document_id.clone());

            // The document must already exist — ingest attaches bytes
            // to a registered document rather than creating rows
            // implicitly (document creation is a separate concern).
            get_document(&ctx.pool, &document_id)
                .await
                .map_err(store_err)?
                .ok_or_else(|| JobError::Failed(format!("document not found: {}", document_id)))?;

            // Deterministic object name → retries rewrite the same
            // bytes instead of accumulating orphans.
            let object_name = format!("documents/{}/{}", document_id, payload.file_name);
            let storage_uri = format!("local://{object_name}");

            // Skip the store write entirely when a previous attempt
            // already completed: the artifact row below is the source
            // of truth for "ingested".
            let already_ingested = list_artifacts(&ctx.pool, &document_id)
                .await
                .map_err(store_err)?
                .iter()
                .any(|a| a.storage_uri == storage_uri);

            if !already_ingested {
                // Stream the payload through the staging/commit path so
                // the SHA-256 and byte count come from the `CommitReceipt`
                // and a crash mid-upload never publishes a partial object.
                // The payload is already fully in memory (JSON), so we
                // chunk it rather than changing the wire contract.
                let mut fs = FaultInjectingFs::new(self.storage.clone())
                    .map_err(|e| JobError::Failed(format!("storage init failed: {e}")))?;
                let mut upload = fs
                    .begin_upload(&object_name)
                    .map_err(|e| JobError::Failed(format!("storage upload failed: {e}")))?;
                for chunk in payload.bytes.chunks(UPLOAD_CHUNK_BYTES) {
                    upload
                        .write_chunk(chunk)
                        .map_err(|e| JobError::Failed(format!("storage upload failed: {e}")))?;
                }
                let receipt = upload
                    .commit()
                    .map_err(|e| JobError::Failed(format!("storage commit failed: {e}")))?;

                let checksum = DocumentChecksum {
                    algorithm: ChecksumAlgorithm::Sha256,
                    value: receipt.sha256.clone(),
                };
                let artifact = upload_artifact(
                    &ctx.pool,
                    &NewArtifact {
                        id: None,
                        document_id: document_id.clone(),
                        storage_uri: storage_uri.clone(),
                        file_name: payload.file_name.clone(),
                        mime_type: payload.mime_type.clone(),
                        size_bytes: receipt.bytes,
                        checksum: Some(checksum),
                    },
                )
                .await
                .map_err(store_err)?;

                record_event(
                    &ctx.pool,
                    &document_id,
                    None,
                    DocumentAuditEventType::Imported,
                    Some(&json!({
                        "artifact_id": artifact.id,
                        "storage_uri": storage_uri,
                        "size_bytes": artifact.size_bytes,
                        "job_id": job.id,
                    })),
                )
                .await
                .map_err(store_err)?;
            }

            Ok(())
        })
    }
}

/// Parsed and validated `document.ingest` payload.
struct IngestPayload {
    document_id: String,
    file_name: String,
    mime_type: Option<String>,
    bytes: Vec<u8>,
}

impl IngestPayload {
    fn parse(payload: &serde_json::Value) -> Result<Self, JobError> {
        let document_id = payload["document_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                JobError::Failed("payload.document_id must be a non-empty string".into())
            })?
            .to_string();

        let bytes = match (payload["content_b64"].as_str(), payload["content"].as_str()) {
            (Some(b64), None) => B64.decode(b64.trim()).map_err(|e| {
                JobError::Failed(format!("payload.content_b64 is not valid base64: {e}"))
            })?,
            (None, Some(text)) => text.as_bytes().to_vec(),
            _ => {
                return Err(JobError::Failed(
                    "payload must contain exactly one of 'content_b64' or 'content'".into(),
                ));
            }
        };

        let file_name = match payload["file_name"].as_str() {
            Some(name) => sanitize_file_name(name)?,
            None => "upload.bin".to_string(),
        };

        let mime_type = payload["mime_type"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        Ok(Self {
            document_id,
            file_name,
            mime_type,
            bytes,
        })
    }
}

/// Rejects names that could escape the `documents/{id}/` prefix or
/// break path confinement inside the storage backend.
fn sanitize_file_name(name: &str) -> Result<String, JobError> {
    let trimmed = name.trim();
    let bad = trimmed.is_empty()
        || trimmed == "."
        || trimmed == ".."
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.chars().any(|c| c.is_control());
    if bad {
        return Err(JobError::Failed(format!(
            "payload.file_name is not a safe file name: {name:?}"
        )));
    }
    Ok(trimmed.to_string())
}

/// Lowercase hex encoding without pulling in a `hex` crate.
fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Store errors become retryable job failures with context attached.
fn store_err(e: crate::crates::document_engine::store::StoreError) -> JobError {
    JobError::Failed(format!("document store error: {e}"))
}

// ---------------------------------------------------------------------------
// system.ping
// ---------------------------------------------------------------------------

/// `system.ping` — trivial handler that always succeeds.
///
/// Exists so operators and tests can verify the queue end-to-end
/// (enqueue → claim → dispatch → `succeeded`) without touching real
/// storage or document state.
struct SystemPingHandler;

impl JobHandler for SystemPingHandler {
    fn handle<'a>(
        &'a self,
        _ctx: &'a JobContext,
        _job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move { Ok(()) })
    }
}

// ---------------------------------------------------------------------------
// pdf.inspect
// ---------------------------------------------------------------------------

/// `pdf.inspect` — validates a PDF stored under the storage root and
/// writes the inspection report next to it.
///
/// See the module docs for the payload contract. All failures are
/// `JobError::Failed` so the engine's retry policy applies uniformly.
struct PdfInspectHandler;

/// Parsed `pdf.inspect` payload.
#[derive(Debug, Deserialize)]
struct InspectPayload {
    /// Path relative to the storage root. Must not be absolute and
    /// must not contain `..` components.
    path: String,
    /// Size cap applied before reading (bytes). Defaults to 100 MiB.
    #[serde(default = "default_inspect_max_bytes")]
    max_bytes: u64,
}

fn default_inspect_max_bytes() -> u64 {
    DEFAULT_INSPECT_MAX_BYTES
}

/// Parsed `storage.gc_staging` payload.
#[derive(Debug, Deserialize)]
struct GcStagingPayload {
    /// Entries older than this many seconds are deleted. Defaults to
    /// 24 h.
    #[serde(default = "default_gc_retention_secs")]
    retention_secs: u64,
}

fn default_gc_retention_secs() -> u64 {
    DEFAULT_GC_RETENTION_SECS
}

impl JobHandler for PdfInspectHandler {
    fn handle<'a>(
        &'a self,
        _ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let payload: InspectPayload = serde_json::from_value(job.payload.clone())?;
            let root = storage_root()?;
            inspect_pdf_at(&root, &payload.path, payload.max_bytes)
        })
    }
}

/// Resolves the configured storage root for handlers that need it.
fn storage_root() -> Result<PathBuf, JobError> {
    AppConfig::load_lenient()
        .map(|cfg| cfg.to_storage_config().root_dir)
        .map_err(|e| JobError::Failed(format!("cannot load storage config: {e}")))
}

/// Confines `rel_path` to `root`: rejects empty, absolute, and any
/// path containing `..` (or other non-normal components such as a
/// Windows prefix). Returns the joined path on success.
fn resolve_under_root(root: &Path, rel_path: &str) -> Result<PathBuf, JobError> {
    let rel = Path::new(rel_path);
    let confined = !rel_path.trim().is_empty()
        && !rel.is_absolute()
        && rel
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if !confined {
        return Err(JobError::Failed(format!(
            "payload.path must be a relative path inside the storage root: {rel_path:?}"
        )));
    }
    Ok(root.join(rel))
}

/// Validates `rel_path`'s file under `root`, inspects it, and writes
/// `<root>/<rel_path>.inspect.json` atomically (tmp + rename).
fn inspect_pdf_at(root: &Path, rel_path: &str, max_bytes: u64) -> Result<(), JobError> {
    let file = resolve_under_root(root, rel_path)?;

    // Bound memory before reading: reject on metadata size first, then
    // re-check the actual bytes read (TOCTOU guard).
    let meta = std::fs::metadata(&file)
        .map_err(|e| JobError::Failed(format!("cannot stat {}: {e}", file.display())))?;
    if !meta.is_file() {
        return Err(JobError::Failed(format!(
            "payload.path is not a regular file: {rel_path:?}"
        )));
    }
    if meta.len() > max_bytes {
        return Err(JobError::Failed(format!(
            "file {} is {} bytes, exceeding max_bytes {max_bytes}",
            rel_path,
            meta.len()
        )));
    }
    let data = std::fs::read(&file)
        .map_err(|e| JobError::Failed(format!("cannot read {}: {e}", file.display())))?;
    if data.len() as u64 > max_bytes {
        return Err(JobError::Failed(format!(
            "file {} grew past max_bytes {max_bytes} while reading",
            rel_path
        )));
    }

    crate::crates::security_engine::validate_pdf(&data, max_bytes)
        .map_err(|e| JobError::Failed(format!("pdf validation failed for {rel_path:?}: {e}")))?;

    let info = crate::crates::pdf_engine::PdfEngine::new()
        .inspect(&data)
        .map_err(|e| JobError::Failed(format!("pdf inspect failed for {rel_path:?}: {e}")))?;

    // Report lands at `<root>/<path>.inspect.json` — a sibling of the
    // input, never overwriting it.
    let mut out_os = file.clone().into_os_string();
    out_os.push(".inspect.json");
    let out = PathBuf::from(out_os);
    let json = serde_json::to_string_pretty(&info)?;

    // Write-then-rename so a crash mid-write can't leave a truncated
    // report that looks authoritative.
    let mut tmp_os = out.clone().into_os_string();
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);
    if let Some(parent) = tmp.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| JobError::Failed(format!("cannot create {}: {e}", parent.display())))?;
    }
    std::fs::write(&tmp, json.as_bytes())
        .map_err(|e| JobError::Failed(format!("cannot write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, &out).map_err(|e| {
        JobError::Failed(format!(
            "cannot rename {} -> {}: {e}",
            tmp.display(),
            out.display()
        ))
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// video.probe / video.thumbnail / video.transcode / video.trim
// ---------------------------------------------------------------------------
//
// ffmpeg-backed media operations over files stored under the storage
// root. All four kinds share the `pdf.inspect` confinement model:
// `path` is relative to the storage root, size is checked before
// reading, and outputs are deterministic sibling files written
// atomically (`*.tmp` + rename) so retries are idempotent.

/// `video.probe` — probes a video file and writes the `VideoInfo`
/// report next to it as pretty JSON.
struct VideoProbeHandler;

/// `video.thumbnail` — extracts a PNG thumbnail frame from a video.
struct VideoThumbnailHandler;

/// `video.transcode` — transcodes a video to `target_ext`.
struct VideoTranscodeHandler;

/// `video.trim` — cuts a video to `[start_secs, end_secs)`.
struct VideoTrimHandler;

/// Shared `path` + `max_bytes` fields for the `video.*` payloads.
#[derive(Debug, Deserialize)]
struct VideoPathPayload {
    /// Path relative to the storage root. Must not be absolute and
    /// must not contain `..` components.
    path: String,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.thumbnail` payload.
#[derive(Debug, Deserialize)]
struct VideoThumbnailPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Timestamp (seconds) of the frame to thumbnail. `None` = engine
    /// default (typically the first frame).
    #[serde(default)]
    at_secs: Option<f64>,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.transcode` payload.
#[derive(Debug, Deserialize)]
struct VideoTranscodePayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Bare output extension (e.g. `"webm"`, `"mp4"`). Must be
    /// alphanumeric; the output is written to `<path>.<target_ext>`.
    target_ext: String,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.trim` payload.
#[derive(Debug, Deserialize)]
struct VideoTrimPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Start of the kept segment, in seconds.
    start_secs: f64,
    /// End of the kept segment, in seconds. `None` = to the end.
    #[serde(default)]
    end_secs: Option<f64>,
    /// Whether to re-encode (frame-accurate) instead of stream-copy.
    #[serde(default)]
    reencode: bool,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

fn default_video_max_bytes() -> u64 {
    DEFAULT_VIDEO_MAX_BYTES
}

impl JobHandler for VideoProbeHandler {
    fn handle<'a>(
        &'a self,
        _ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let payload: VideoPathPayload = serde_json::from_value(job.payload.clone())?;
            let root = storage_root()?;
            probe_video_at(&root, &payload.path, payload.max_bytes)
        })
    }
}

impl JobHandler for VideoThumbnailHandler {
    fn handle<'a>(
        &'a self,
        _ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let payload: VideoThumbnailPayload = serde_json::from_value(job.payload.clone())?;
            let root = storage_root()?;
            thumbnail_video_at(&root, &payload)
        })
    }
}

impl JobHandler for VideoTranscodeHandler {
    fn handle<'a>(
        &'a self,
        _ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let payload: VideoTranscodePayload = serde_json::from_value(job.payload.clone())?;
            let root = storage_root()?;
            transcode_video_at(&root, &payload)
        })
    }
}

impl JobHandler for VideoTrimHandler {
    fn handle<'a>(
        &'a self,
        _ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let payload: VideoTrimPayload = serde_json::from_value(job.payload.clone())?;
            let root = storage_root()?;
            trim_video_at(&root, &payload)
        })
    }
}

/// [`EngineLimits`] handed to video-engine calls: the caller's byte
/// cap as `max_input_bytes`, everything else left to engine defaults.
fn video_limits(max_bytes: u64) -> crate::crates::core::engine::EngineLimits {
    crate::crates::core::engine::EngineLimits {
        max_input_bytes: Some(max_bytes),
        ..Default::default()
    }
}

/// Maps a video-engine failure into a retryable job failure.
fn video_err(context: &str, e: crate::crates::video_engine::VideoError) -> JobError {
    JobError::Failed(format!("{context}: {e}"))
}

/// Confines `rel_path` under `root`, checks the file size against
/// `max_bytes` **before** reading (then again after, as a TOCTOU
/// guard), and returns the resolved path plus the file bytes.
fn read_video_input(
    root: &Path,
    rel_path: &str,
    max_bytes: u64,
) -> Result<(PathBuf, Vec<u8>), JobError> {
    let file = resolve_under_root(root, rel_path)?;
    let meta = std::fs::metadata(&file)
        .map_err(|e| JobError::Failed(format!("cannot stat {}: {e}", file.display())))?;
    if !meta.is_file() {
        return Err(JobError::Failed(format!(
            "payload.path is not a regular file: {rel_path:?}"
        )));
    }
    if meta.len() > max_bytes {
        return Err(JobError::Failed(format!(
            "file {} is {} bytes, exceeding max_bytes {max_bytes}",
            rel_path,
            meta.len()
        )));
    }
    let data = std::fs::read(&file)
        .map_err(|e| JobError::Failed(format!("cannot read {}: {e}", file.display())))?;
    if data.len() as u64 > max_bytes {
        return Err(JobError::Failed(format!(
            "file {} grew past max_bytes {max_bytes} while reading",
            rel_path
        )));
    }
    Ok((file, data))
}

/// Appends `suffix` to a path's file name (operating on the whole
/// path string, like the `.inspect.json` report naming).
fn path_with_suffix(file: &Path, suffix: &str) -> PathBuf {
    let mut os = file.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

/// Writes `bytes` to `out` atomically: a `*.tmp` sibling is written
/// first, then renamed over `out`, so a crash mid-write can't leave a
/// truncated output that looks authoritative.
fn write_output_atomic(out: &Path, bytes: &[u8]) -> Result<(), JobError> {
    let mut tmp_os = out.as_os_str().to_os_string();
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(tmp_os);
    if let Some(parent) = tmp.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| JobError::Failed(format!("cannot create {}: {e}", parent.display())))?;
    }
    std::fs::write(&tmp, bytes)
        .map_err(|e| JobError::Failed(format!("cannot write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, out).map_err(|e| {
        JobError::Failed(format!(
            "cannot rename {} -> {}: {e}",
            tmp.display(),
            out.display()
        ))
    })?;
    Ok(())
}

/// Validates `target_ext` as a bare output extension: alphanumeric
/// only, no leading dot required, case-normalized to lowercase.
fn sanitize_target_ext(ext: &str) -> Result<String, JobError> {
    sanitize_ext_field("target_ext", ext)
}

/// `video.probe` implementation: probe `rel_path` under `root` and
/// write the pretty-JSON `VideoInfo` report to
/// `<root>/<rel_path>.probe.json`.
fn probe_video_at(root: &Path, rel_path: &str, max_bytes: u64) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, rel_path, max_bytes)?;
    let info = crate::crates::video_engine::VideoEngine::new()
        .probe(&data)
        .map_err(|e| video_err(&format!("video probe failed for {rel_path:?}"), e))?;
    let json = serde_json::to_string_pretty(&info)?;
    write_output_atomic(&path_with_suffix(&file, ".probe.json"), json.as_bytes())
}

/// `video.thumbnail` implementation: extract a PNG frame from
/// `rel_path` and write it to `<root>/<rel_path>.thumb.png`.
fn thumbnail_video_at(root: &Path, payload: &VideoThumbnailPayload) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let png = crate::crates::video_engine::VideoEngine::new()
        .thumbnail(&data, payload.at_secs, &video_limits(payload.max_bytes))
        .map_err(|e| video_err(&format!("video thumbnail failed for {:?}", payload.path), e))?;
    write_output_atomic(&path_with_suffix(&file, ".thumb.png"), &png)
}

/// `video.transcode` implementation: transcode `rel_path` to
/// `target_ext` and write the result to `<root>/<rel_path>.<ext>`.
/// Fails rather than overwrite the input if the names collide.
fn transcode_video_at(root: &Path, payload: &VideoTranscodePayload) -> Result<(), JobError> {
    let ext = sanitize_target_ext(&payload.target_ext)?;
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let out = path_with_suffix(&file, &format!(".{ext}"));
    if out == file {
        return Err(JobError::Failed(format!(
            "video.transcode output would overwrite the input: {:?}",
            payload.path
        )));
    }
    let bytes = crate::crates::video_engine::VideoEngine::new()
        .transcode(&data, &ext, &video_limits(payload.max_bytes))
        .map_err(|e| video_err(&format!("video transcode failed for {:?}", payload.path), e))?;
    write_output_atomic(&out, &bytes)
}

/// `video.trim` implementation: cut `rel_path` to
/// `[start_secs, end_secs)` and write the result to
/// `<root>/<rel_path>.trim.<input-ext>` (`.trim` alone when the input
/// has no extension).
fn trim_video_at(root: &Path, payload: &VideoTrimPayload) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let out = path_with_suffix(&file, &format!(".trim{ext}"));
    if out == file {
        return Err(JobError::Failed(format!(
            "video.trim output would overwrite the input: {:?}",
            payload.path
        )));
    }
    let bytes = crate::crates::video_engine::VideoEngine::new()
        .trim(
            &data,
            payload.start_secs,
            payload.end_secs,
            payload.reencode,
            &video_limits(payload.max_bytes),
        )
        .map_err(|e| video_err(&format!("video trim failed for {:?}", payload.path), e))?;
    write_output_atomic(&out, &bytes)
}

// ---------------------------------------------------------------------------
// Extended video.* handlers (remux / frames / subs / analyze / packaging /
// metadata / streams / audio / geometry / tempo / gif / slideshow / merge)
// ---------------------------------------------------------------------------
//
// Same confinement + atomic-output model as the four kinds above. The
// `video_transform_at` helper covers the common "read → engine op →
// `<path>.<op><ext>`" shape; report and multi-file ops have dedicated
// bodies.

/// `video.remux` — stream-copy remux to `target_ext`.
struct VideoRemuxHandler;

/// `video.extract_frames` — pull frames per a [`FramesSpec`].
struct VideoExtractFramesHandler;

/// `video.subtitles_extract` — dump one subtitle track as text.
struct VideoSubtitlesExtractHandler;

/// `video.analyze` — full media analysis report.
struct VideoAnalyzeHandler;

/// `video.package_hls` — HLS package (`index.m3u8` + segments).
struct VideoPackageHlsHandler;

/// `video.metadata_set` — write container tags.
struct VideoMetadataSetHandler;

/// `video.metadata_strip` — remove all container metadata.
struct VideoMetadataStripHandler;

/// `video.extract_streams` — pull embedded streams as files.
struct VideoExtractStreamsHandler;

/// `video.audio_volume` — gain/attenuate all audio.
struct VideoAudioVolumeHandler;

/// `video.contact_sheet` — tiled PNG of sampled frames.
struct VideoContactSheetHandler;

/// `video.speed` — change playback speed.
struct VideoSpeedHandler;

/// `video.reverse` — reverse the clip.
struct VideoReverseHandler;

/// `video.loop` — loop the clip `count` extra times.
struct VideoLoopHandler;

/// `video.freeze` — freeze the frame at `at_secs`.
struct VideoFreezeHandler;

/// `video.resize` — resize to `width`×`height` per `fit`.
struct VideoResizeHandler;

/// `video.crop` — crop to `(x, y, w, h)`.
struct VideoCropHandler;

/// `video.rotate` — rotate by `degrees`.
struct VideoRotateHandler;

/// `video.flip` — mirror horizontally or vertically.
struct VideoFlipHandler;

/// `video.gif` — video → animated GIF.
struct VideoGifHandler;

/// `video.slideshow` — images → video.
struct VideoSlideshowHandler;

/// `video.normalize` — EBU R128 loudness normalization.
struct VideoNormalizeHandler;

/// `video.loudness` — loudness measurement report.
struct VideoLoudnessHandler;

/// `video.scene_frames` — one frame per detected scene change.
struct VideoSceneFramesHandler;

/// `video.merge_av` — merge an audio file into a video.
struct VideoMergeAvHandler;

/// `video.validate` — full media validation report.
struct VideoValidateHandler;

/// `video.chapters_list` — list embedded chapters as JSON.
struct VideoChaptersListHandler;

/// `video.chapters_set` — replace the chapter table.
struct VideoChaptersSetHandler;

/// `video.chapters_shift` — shift every chapter by `offset_ms`.
struct VideoChaptersShiftHandler;

/// `video.chapters_remove` — drop every embedded chapter.
struct VideoChaptersRemoveHandler;

/// `video.hdr_detect` — HDR characteristics report.
struct VideoHdrDetectHandler;

/// `video.hdr_tone_map` — HDR→SDR tone mapping.
struct VideoHdrToneMapHandler;

/// `video.segment_split` — split into `seg-NNN.<format>` files.
struct VideoSegmentSplitHandler;

/// `video.segment_extract` — cut a `[start_ms, end_ms)` range.
struct VideoSegmentExtractHandler;

/// `video.waveform` — audio waveform PNG.
struct VideoWaveformHandler;

/// `video.spectrogram` — audio spectrogram PNG.
struct VideoSpectrogramHandler;

/// `video.vectorscope` — stereo vectorscope PNG.
struct VideoVectorscopeHandler;

/// `video.sync_report` — A/V start-time report.
struct VideoSyncReportHandler;

/// `video.sync_fix` — manual A/V stream shift.
struct VideoSyncFixHandler;

/// `video.sync_auto` — automatic A/V alignment.
struct VideoSyncAutoHandler;

/// `video.stabilize` — deshake/vidstab stabilization.
struct VideoStabilizeHandler;

/// `video.preview_gif` — animated GIF preview.
struct VideoPreviewGifHandler;

/// `video.preview_webp` — animated WebP preview.
struct VideoPreviewWebpHandler;

/// `video.proxy_generate` — proxy/mezzanine/archival derivative.
struct VideoProxyGenerateHandler;

/// `video.stream_select` — map/select streams into `target_ext`.
struct VideoStreamSelectHandler;

/// `video.stream_cover_art` — attach cover art to an audio file.
struct VideoStreamCoverArtHandler;

/// `video.filters_apply` — named preset or raw `-vf`/`-af` filters.
struct VideoFiltersApplyHandler;

/// Parsed `video.extract_frames` payload.
#[derive(Debug, Deserialize)]
struct VideoExtractFramesPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// One frame every N seconds. Mutually exclusive with `fps`/`all`/
    /// `keyframes_only` at the engine level — the engine resolves
    /// precedence.
    #[serde(default)]
    every_secs: Option<f64>,
    /// Frames per second to sample at.
    #[serde(default)]
    fps: Option<f64>,
    /// Emit every frame.
    #[serde(default)]
    all: bool,
    /// Only keyframes/I-frames.
    #[serde(default)]
    keyframes_only: bool,
    /// Frame image format (`"png"` or `"jpg"`). Defaults to `"png"`.
    #[serde(default)]
    format: Option<String>,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.subtitles_extract` payload.
#[derive(Debug, Deserialize)]
struct VideoSubtitlesExtractPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Subtitle stream ordinal (`None` → first subtitle stream).
    #[serde(default)]
    track: Option<u32>,
    /// Output subtitle format (`"srt"`, `"vtt"`, or `"ass"`).
    /// Defaults to `"srt"`.
    #[serde(default)]
    format: Option<String>,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.metadata_set` payload.
#[derive(Debug, Deserialize)]
struct VideoMetadataSetPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Container tags to write (`title`, `artist`, ...). Keys/values
    /// must be strings.
    tags: BTreeMap<String, String>,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.extract_streams` payload.
#[derive(Debug, Deserialize)]
struct VideoExtractStreamsPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Extract subtitle streams (as `subtitle_format` text).
    #[serde(default)]
    subtitles: bool,
    /// Extract binary attachments.
    #[serde(default)]
    attachments: bool,
    /// Extract attached cover art.
    #[serde(default)]
    cover_art: bool,
    /// Export chapters as a JSON sidecar.
    #[serde(default)]
    chapters: bool,
    /// Extract data/other streams (best effort).
    #[serde(default)]
    data: bool,
    /// Subtitle text format (`"srt"`, `"vtt"`, or `"ass"`).
    /// Defaults to `"srt"`.
    #[serde(default)]
    subtitle_format: Option<String>,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.audio_volume` payload.
#[derive(Debug, Deserialize)]
struct VideoAudioVolumePayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Gain in decibels (positive = louder, negative = quieter).
    gain_db: f64,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.contact_sheet` payload.
#[derive(Debug, Deserialize)]
struct VideoContactSheetPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Grid columns. Defaults to 4.
    #[serde(default)]
    cols: Option<u32>,
    /// Grid rows. Defaults to 4.
    #[serde(default)]
    rows: Option<u32>,
    /// Thumbnail cell width in pixels. Defaults to 320.
    #[serde(default)]
    thumb_width: Option<u32>,
    /// Seconds between sampled frames (`None` → engine default,
    /// duration / cells).
    #[serde(default)]
    interval_secs: Option<f64>,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.speed` payload.
#[derive(Debug, Deserialize)]
struct VideoSpeedPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Playback speed multiplier (`2.0` = twice as fast).
    factor: f64,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.loop` payload.
#[derive(Debug, Deserialize)]
struct VideoLoopPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Additional times the clip is appended after the first play.
    count: u32,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.freeze` payload.
#[derive(Debug, Deserialize)]
struct VideoFreezePayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Timestamp (seconds) of the frame to freeze.
    at_secs: f64,
    /// How long the frozen frame is held, in seconds.
    duration_secs: f64,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.resize` payload.
#[derive(Debug, Deserialize)]
struct VideoResizePayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Target width in pixels.
    width: u32,
    /// Target height in pixels.
    height: u32,
    /// Fit mode: `"fit"` (default), `"cover"`, `"contain"`, or
    /// `"stretch"` — case-insensitive.
    #[serde(default)]
    fit: Option<String>,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.crop` payload.
#[derive(Debug, Deserialize)]
struct VideoCropPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Crop origin X in pixels.
    x: u32,
    /// Crop origin Y in pixels.
    y: u32,
    /// Crop width in pixels.
    w: u32,
    /// Crop height in pixels.
    h: u32,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.rotate` payload.
#[derive(Debug, Deserialize)]
struct VideoRotatePayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Rotation angle in degrees.
    degrees: f64,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.flip` payload.
#[derive(Debug, Deserialize)]
struct VideoFlipPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// `true` (default) mirrors horizontally; `false` vertically.
    #[serde(default)]
    horizontal: Option<bool>,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.gif` payload.
#[derive(Debug, Deserialize)]
struct VideoGifPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// GIF frame rate (`None` → engine default).
    #[serde(default)]
    fps: Option<f64>,
    /// Output width in pixels (`None` → engine default scaling).
    #[serde(default)]
    width: Option<u32>,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.slideshow` payload.
#[derive(Debug, Deserialize)]
struct VideoSlideshowPayload {
    /// Image paths relative to the storage root, in order. Must not
    /// be empty; each entry is confined like `path`.
    paths: Vec<String>,
    /// Output frame rate (`None` → 10 fps).
    #[serde(default)]
    fps: Option<f64>,
    /// Seconds each image is shown (`None` → engine default).
    #[serde(default)]
    duration_per_image: Option<f64>,
    /// Bare output extension (e.g. `"mp4"`). The output is written to
    /// `slideshow.<target_ext>` in the first input's directory.
    target_ext: String,
    /// Size cap applied to **each** input before reading (bytes).
    /// Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.normalize` payload.
#[derive(Debug, Deserialize)]
struct VideoNormalizePayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Target integrated loudness in LUFS (`None` → engine default,
    /// typically -16 or -23 LUFS).
    #[serde(default)]
    target_lufs: Option<f64>,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.scene_frames` payload.
#[derive(Debug, Deserialize)]
struct VideoSceneFramesPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Scene-change score threshold 0.0–1.0 (`None` → 0.4, matching
    /// the engine's analysis default).
    #[serde(default)]
    threshold: Option<f64>,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.merge_av` payload.
#[derive(Debug, Deserialize)]
struct VideoMergeAvPayload {
    /// Video path relative to the storage root.
    video_path: String,
    /// Audio path relative to the storage root.
    audio_path: String,
    /// Bare output extension (`None` → the video input's extension,
    /// or `"mp4"` when it has none).
    #[serde(default)]
    target_ext: Option<String>,
    /// Size cap applied to **each** input before reading (bytes).
    /// Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// One chapter entry in a `video.chapters_set` payload, in
/// milliseconds (the engine's editing currency).
#[derive(Debug, Deserialize)]
struct VideoChapterEdit {
    /// Chapter start time in milliseconds.
    start_ms: i64,
    /// Chapter end time in milliseconds.
    end_ms: i64,
    /// Chapter title (`None`/empty → untitled chapter).
    #[serde(default)]
    title: Option<String>,
}

/// Parsed `video.chapters_set` payload.
#[derive(Debug, Deserialize)]
struct VideoChaptersSetPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Replacement chapter table. Must be non-empty, sorted by
    /// `start_ms`, non-overlapping — the engine validates.
    chapters: Vec<VideoChapterEdit>,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.chapters_shift` payload.
#[derive(Debug, Deserialize)]
struct VideoChaptersShiftPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Offset applied to every chapter bound, in milliseconds.
    /// Positive shifts later.
    offset_ms: i64,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.hdr_tone_map` payload.
#[derive(Debug, Deserialize)]
struct VideoHdrToneMapPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Desaturation strength for the Hable curve (`None` → ffmpeg
    /// default).
    #[serde(default)]
    desat: Option<f64>,
    /// Signal peak override in nits (`None` → stream/container
    /// metadata or ffmpeg default).
    #[serde(default)]
    peak: Option<u32>,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.segment_split` payload.
#[derive(Debug, Deserialize)]
struct VideoSegmentSplitPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Split strategy: `"duration"` (default), `"scenes"`, or
    /// `"equal"`.
    #[serde(default)]
    mode: Option<String>,
    /// Segment length in seconds — required for `mode = "duration"`.
    #[serde(default)]
    duration_secs: Option<f64>,
    /// Scene-change score threshold 0.0–1.0 for `mode = "scenes"`
    /// (`None` → 0.4).
    #[serde(default)]
    threshold: Option<f64>,
    /// Number of equal parts for `mode = "equal"` (required then;
    /// engine accepts 2–100).
    #[serde(default)]
    parts: Option<u32>,
    /// Output container (`"mp4"`, `"mkv"`, `"webm"`, or `"avi"`).
    /// Defaults to the input's extension when it is one of those,
    /// else `"mp4"`.
    #[serde(default)]
    format: Option<String>,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.segment_extract` payload.
#[derive(Debug, Deserialize)]
struct VideoSegmentExtractPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Range start in milliseconds.
    start_ms: u64,
    /// Range end in milliseconds (exclusive).
    end_ms: u64,
    /// Output container — same allowlist/default as
    /// [`VideoSegmentSplitPayload::format`].
    #[serde(default)]
    format: Option<String>,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed payload shared by `video.waveform`, `video.spectrogram`,
/// and `video.vectorscope` (the kind is fixed per job kind).
#[derive(Debug, Deserialize)]
struct VideoWaveformPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Canvas width in pixels (`None` → 800).
    #[serde(default)]
    width: Option<u32>,
    /// Canvas height in pixels (`None` → 400).
    #[serde(default)]
    height: Option<u32>,
    /// Optional color override (per-kind filter option).
    #[serde(default)]
    color: Option<String>,
    /// Optional scale/display override (per-kind filter option).
    #[serde(default)]
    scale: Option<String>,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.sync_fix` payload.
#[derive(Debug, Deserialize)]
struct VideoSyncFixPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Audio shift in milliseconds: positive = audio starts later,
    /// negative = earlier (`None` → 0).
    #[serde(default)]
    audio_offset_ms: Option<i64>,
    /// Video shift in milliseconds: positive = video starts later,
    /// negative = earlier (`None` → 0).
    #[serde(default)]
    video_offset_ms: Option<i64>,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.stabilize` payload.
#[derive(Debug, Deserialize)]
struct VideoStabilizePayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Shakiness 1–10 (`None` → engine default 5).
    #[serde(default)]
    shakiness: Option<f64>,
    /// Smoothing 1–100 (`None` → engine default 15).
    #[serde(default)]
    smoothing: Option<f64>,
    /// Backend: `"vidstab"` (default) or `"deshake"`. Ignored when
    /// `auto` is `true`.
    #[serde(default)]
    algo: Option<String>,
    /// `true` → [`VideoEngine::stabilize_auto`] picks the best
    /// available backend.
    #[serde(default)]
    auto: bool,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed payload shared by `video.preview_gif` and
/// `video.preview_webp` (the format is fixed per job kind).
#[derive(Debug, Deserialize)]
struct VideoPreviewPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Seconds of source to animate (`None` → 5).
    #[serde(default)]
    max_duration_secs: Option<f64>,
    /// Output frame rate (`None` → 10).
    #[serde(default)]
    fps: Option<f64>,
    /// Output width in pixels (`None` → 320).
    #[serde(default)]
    width: Option<u32>,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.proxy_generate` payload.
#[derive(Debug, Deserialize)]
struct VideoProxyGeneratePayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Derivative flavour: `"proxy"` (default), `"mezzanine"`, or
    /// `"archival"`.
    #[serde(default)]
    kind: Option<String>,
    /// Max output height for `kind = "proxy"` (`None` → 720).
    #[serde(default)]
    target_height: Option<u32>,
    /// H.264 CRF for `kind = "proxy"` (`None` → 23).
    #[serde(default)]
    crf: Option<u32>,
    /// x264 preset name for `kind = "proxy"` (`None` → `veryfast`).
    #[serde(default)]
    preset: Option<String>,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.stream_select` payload.
#[derive(Debug, Deserialize)]
struct VideoStreamSelectPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Keep the video stream (`None`/`true` → keep; `false` →
    /// video-less output).
    #[serde(default)]
    keep_video: Option<bool>,
    /// 0-based audio stream ordinals to keep (`None` → all).
    #[serde(default)]
    audio_ordinals: Option<Vec<u32>>,
    /// 0-based subtitle stream ordinals to keep (`None` → all).
    #[serde(default)]
    subtitle_ordinals: Option<Vec<u32>>,
    /// Drop attached pictures/data streams (`None` → `false`).
    #[serde(default)]
    drop_attachments: Option<bool>,
    /// Bare output extension (`None` → the input's extension, or
    /// `"mp4"` when it has none/unsafe).
    #[serde(default)]
    target_ext: Option<String>,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.stream_cover_art` payload.
#[derive(Debug, Deserialize)]
struct VideoStreamCoverArtPayload {
    /// Media path relative to the storage root (see
    /// [`VideoPathPayload`]).
    path: String,
    /// Cover image path relative to the storage root — confined and
    /// size-checked like `path`.
    cover_path: String,
    /// Size cap applied to **each** input before reading (bytes).
    /// Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Parsed `video.filters_apply` payload.
#[derive(Debug, Deserialize)]
struct VideoFiltersApplyPayload {
    /// Path relative to the storage root (see [`VideoPathPayload`]).
    path: String,
    /// Named preset from [`VideoEngine::filter_presets`] (e.g.
    /// `"grayscale"`, `"denoise"`). Supplies `video_filter`/
    /// `audio_filter` defaults; explicit fields override per-chain.
    #[serde(default)]
    preset: Option<String>,
    /// Raw ffmpeg `-vf` chain.
    #[serde(default)]
    video_filter: Option<String>,
    /// Raw ffmpeg `-af` chain.
    #[serde(default)]
    audio_filter: Option<String>,
    /// Size cap applied before reading (bytes). Defaults to 4 GiB.
    #[serde(default = "default_video_max_bytes")]
    max_bytes: u64,
}

/// Implements [`JobHandler`] for every extended `video.*` handler:
/// parse the payload, resolve the storage root, run the named
/// operation. Identical contract to the four hand-written impls above.
macro_rules! impl_video_handler {
    ($handler:ident, $payload:ty, $op:ident) => {
        impl JobHandler for $handler {
            fn handle<'a>(
                &'a self,
                _ctx: &'a JobContext,
                job: &'a Job,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>,
            > {
                Box::pin(async move {
                    let payload: $payload = serde_json::from_value(job.payload.clone())?;
                    let root = storage_root()?;
                    $op(&root, &payload)
                })
            }
        }
    };
}

impl_video_handler!(VideoRemuxHandler, VideoTranscodePayload, remux_video_at);
impl_video_handler!(
    VideoExtractFramesHandler,
    VideoExtractFramesPayload,
    extract_frames_video_at
);
impl_video_handler!(
    VideoSubtitlesExtractHandler,
    VideoSubtitlesExtractPayload,
    subtitles_extract_video_at
);
impl_video_handler!(VideoAnalyzeHandler, VideoPathPayload, analyze_video_at);
impl_video_handler!(
    VideoPackageHlsHandler,
    VideoPathPayload,
    package_hls_video_at
);
impl_video_handler!(
    VideoMetadataSetHandler,
    VideoMetadataSetPayload,
    metadata_set_video_at
);
impl_video_handler!(
    VideoMetadataStripHandler,
    VideoPathPayload,
    metadata_strip_video_at
);
impl_video_handler!(
    VideoExtractStreamsHandler,
    VideoExtractStreamsPayload,
    extract_streams_video_at
);
impl_video_handler!(
    VideoAudioVolumeHandler,
    VideoAudioVolumePayload,
    audio_volume_video_at
);
impl_video_handler!(
    VideoContactSheetHandler,
    VideoContactSheetPayload,
    contact_sheet_video_at
);
impl_video_handler!(VideoSpeedHandler, VideoSpeedPayload, speed_video_at);
impl_video_handler!(VideoReverseHandler, VideoPathPayload, reverse_video_at);
impl_video_handler!(VideoLoopHandler, VideoLoopPayload, loop_video_at);
impl_video_handler!(VideoFreezeHandler, VideoFreezePayload, freeze_video_at);
impl_video_handler!(VideoResizeHandler, VideoResizePayload, resize_video_at);
impl_video_handler!(VideoCropHandler, VideoCropPayload, crop_video_at);
impl_video_handler!(VideoRotateHandler, VideoRotatePayload, rotate_video_at);
impl_video_handler!(VideoFlipHandler, VideoFlipPayload, flip_video_at);
impl_video_handler!(VideoGifHandler, VideoGifPayload, gif_video_at);
impl_video_handler!(
    VideoSlideshowHandler,
    VideoSlideshowPayload,
    slideshow_video_at
);
impl_video_handler!(
    VideoNormalizeHandler,
    VideoNormalizePayload,
    normalize_video_at
);
impl_video_handler!(VideoLoudnessHandler, VideoPathPayload, loudness_video_at);
impl_video_handler!(
    VideoSceneFramesHandler,
    VideoSceneFramesPayload,
    scene_frames_video_at
);
impl_video_handler!(VideoMergeAvHandler, VideoMergeAvPayload, merge_av_video_at);
impl_video_handler!(VideoValidateHandler, VideoPathPayload, validate_video_at);
impl_video_handler!(
    VideoChaptersListHandler,
    VideoPathPayload,
    chapters_list_video_at
);
impl_video_handler!(
    VideoChaptersSetHandler,
    VideoChaptersSetPayload,
    chapters_set_video_at
);
impl_video_handler!(
    VideoChaptersShiftHandler,
    VideoChaptersShiftPayload,
    chapters_shift_video_at
);
impl_video_handler!(
    VideoChaptersRemoveHandler,
    VideoPathPayload,
    chapters_remove_video_at
);
impl_video_handler!(VideoHdrDetectHandler, VideoPathPayload, hdr_detect_video_at);
impl_video_handler!(
    VideoHdrToneMapHandler,
    VideoHdrToneMapPayload,
    hdr_tone_map_video_at
);
impl_video_handler!(
    VideoSegmentSplitHandler,
    VideoSegmentSplitPayload,
    segment_split_video_at
);
impl_video_handler!(
    VideoSegmentExtractHandler,
    VideoSegmentExtractPayload,
    segment_extract_video_at
);
impl_video_handler!(
    VideoWaveformHandler,
    VideoWaveformPayload,
    waveform_video_at
);
impl_video_handler!(
    VideoSpectrogramHandler,
    VideoWaveformPayload,
    spectrogram_video_at
);
impl_video_handler!(
    VideoVectorscopeHandler,
    VideoWaveformPayload,
    vectorscope_video_at
);
impl_video_handler!(
    VideoSyncReportHandler,
    VideoPathPayload,
    sync_report_video_at
);
impl_video_handler!(VideoSyncFixHandler, VideoSyncFixPayload, sync_fix_video_at);
impl_video_handler!(VideoSyncAutoHandler, VideoPathPayload, sync_auto_video_at);
impl_video_handler!(
    VideoStabilizeHandler,
    VideoStabilizePayload,
    stabilize_video_at
);
impl_video_handler!(
    VideoPreviewGifHandler,
    VideoPreviewPayload,
    preview_gif_video_at
);
impl_video_handler!(
    VideoPreviewWebpHandler,
    VideoPreviewPayload,
    preview_webp_video_at
);
impl_video_handler!(
    VideoProxyGenerateHandler,
    VideoProxyGeneratePayload,
    proxy_generate_video_at
);
impl_video_handler!(
    VideoStreamSelectHandler,
    VideoStreamSelectPayload,
    stream_select_video_at
);
impl_video_handler!(
    VideoStreamCoverArtHandler,
    VideoStreamCoverArtPayload,
    stream_cover_art_video_at
);
impl_video_handler!(
    VideoFiltersApplyHandler,
    VideoFiltersApplyPayload,
    filters_apply_video_at
);

/// `.<ext>` of `file` (leading dot included), or `""` when the file
/// has no extension — used to build `<path>.<op><ext>` output names.
fn input_ext_suffix(file: &Path) -> String {
    file.extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default()
}

/// Shared body for single-file `video.*` transforms that return
/// `Vec<u8>`: read `path` under `root`, run `op` with
/// [`video_limits`], and write the result to
/// `<file>.<suffix><input-ext>` atomically. `kind` names the job kind
/// in error messages.
fn video_transform_at<F>(
    root: &Path,
    path: &str,
    max_bytes: u64,
    kind: &str,
    suffix: &str,
    op: F,
) -> Result<(), JobError>
where
    F: FnOnce(
        &[u8],
        &crate::crates::core::engine::EngineLimits,
    ) -> crate::crates::video_engine::VideoResult<Vec<u8>>,
{
    let (file, data) = read_video_input(root, path, max_bytes)?;
    let out = path_with_suffix(&file, &format!("{suffix}{}", input_ext_suffix(&file)));
    if out == file {
        return Err(JobError::Failed(format!(
            "{kind} output would overwrite the input: {path:?}"
        )));
    }
    let bytes = op(&data, &video_limits(max_bytes))
        .map_err(|e| video_err(&format!("{kind} failed for {path:?}"), e))?;
    write_output_atomic(&out, &bytes)
}

/// `video.remux` implementation: stream-copy remux of `path` to
/// `target_ext`, written to `<path>.<target_ext>` (same naming as
/// `video.transcode`). Shares the transcode payload contract; the
/// output never overwrites the input because the `.<ext>` suffix is
/// always appended.
fn remux_video_at(root: &Path, payload: &VideoTranscodePayload) -> Result<(), JobError> {
    let ext = sanitize_target_ext(&payload.target_ext)?;
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let out = path_with_suffix(&file, &format!(".{ext}"));
    if out == file {
        return Err(JobError::Failed(format!(
            "video.remux output would overwrite the input: {:?}",
            payload.path
        )));
    }
    let bytes = crate::crates::video_engine::VideoEngine::new()
        .remux(&data, &ext, &video_limits(payload.max_bytes))
        .map_err(|e| video_err(&format!("video remux failed for {:?}", payload.path), e))?;
    write_output_atomic(&out, &bytes)
}

/// `video.extract_frames` implementation: extract frames per the
/// payload spec and write the [`MediaOutput`] set under
/// `<path>.frames/`.
fn extract_frames_video_at(
    root: &Path,
    payload: &VideoExtractFramesPayload,
) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let format = sanitize_ext_field("format", payload.format.as_deref().unwrap_or("png"))?;
    let spec = crate::crates::video_engine::FramesSpec {
        all: payload.all,
        every_secs: payload.every_secs,
        fps: payload.fps,
        keyframes_only: payload.keyframes_only,
        format,
    };
    let out = crate::crates::video_engine::VideoEngine::new()
        .extract_frames(&data, &spec, &video_limits(payload.max_bytes))
        .map_err(|e| {
            video_err(
                &format!("video extract_frames failed for {:?}", payload.path),
                e,
            )
        })?;
    write_media_output(&path_with_suffix(&file, ".frames"), &out)
}

/// `video.subtitles_extract` implementation: extract subtitle track
/// `track` (`None` → first) as `format` text, written to
/// `<path>.subs.<fmt>`.
fn subtitles_extract_video_at(
    root: &Path,
    payload: &VideoSubtitlesExtractPayload,
) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let format = sanitize_ext_field("format", payload.format.as_deref().unwrap_or("srt"))?;
    let bytes = crate::crates::video_engine::VideoEngine::new()
        .extract_subtitles(
            &data,
            payload.track,
            &format,
            &video_limits(payload.max_bytes),
        )
        .map_err(|e| {
            video_err(
                &format!("video subtitles_extract failed for {:?}", payload.path),
                e,
            )
        })?;
    write_output_atomic(&path_with_suffix(&file, &format!(".subs.{format}")), &bytes)
}

/// `video.analyze` implementation: run the full analysis pipeline and
/// write the pretty-JSON [`MediaAnalysis`] report to
/// `<path>.analysis.json`.
fn analyze_video_at(root: &Path, payload: &VideoPathPayload) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let analysis = crate::crates::video_engine::VideoEngine::new()
        .analyze(&data, &video_limits(payload.max_bytes))
        .map_err(|e| video_err(&format!("video analyze failed for {:?}", payload.path), e))?;
    let json = serde_json::to_string_pretty(&analysis)?;
    write_output_atomic(&path_with_suffix(&file, ".analysis.json"), json.as_bytes())
}

/// `video.package_hls` implementation: package the input as HLS and
/// write the [`MediaOutput`] set (`index.m3u8` + segments) under
/// `<path>.hls/`.
fn package_hls_video_at(root: &Path, payload: &VideoPathPayload) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let out = crate::crates::video_engine::VideoEngine::new()
        .package_hls(&data, &video_limits(payload.max_bytes))
        .map_err(|e| {
            video_err(
                &format!("video package_hls failed for {:?}", payload.path),
                e,
            )
        })?;
    write_media_output(&path_with_suffix(&file, ".hls"), &out)
}

/// `video.metadata_set` implementation: write `tags` into the
/// container and save the result to `<path>.tagged<ext>`.
fn metadata_set_video_at(root: &Path, payload: &VideoMetadataSetPayload) -> Result<(), JobError> {
    video_transform_at(
        root,
        &payload.path,
        payload.max_bytes,
        VIDEO_METADATA_SET_KIND,
        ".tagged",
        |data, limits| {
            crate::crates::video_engine::VideoEngine::new().set_metadata(
                data,
                &payload.tags,
                limits,
            )
        },
    )
}

/// `video.metadata_strip` implementation: strip all container
/// metadata, writing `<path>.stripped<ext>`. The engine needs a target
/// extension — the input's own extension is reused (sanitized), or
/// `"mp4"` when the input has none/unsafe.
fn metadata_strip_video_at(root: &Path, payload: &VideoPathPayload) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let ext = derived_container_ext(&file);
    let out = path_with_suffix(&file, &format!(".stripped.{ext}"));
    let bytes = crate::crates::video_engine::VideoEngine::new()
        .strip_metadata(&data, &ext, &video_limits(payload.max_bytes))
        .map_err(|e| {
            video_err(
                &format!("video metadata_strip failed for {:?}", payload.path),
                e,
            )
        })?;
    write_output_atomic(&out, &bytes)
}

/// `video.extract_streams` implementation: pull the selected embedded
/// streams and write the [`MediaOutput`] set under
/// `<path>.streams/`.
fn extract_streams_video_at(
    root: &Path,
    payload: &VideoExtractStreamsPayload,
) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let selection = crate::crates::video_engine::StreamSelection {
        subtitles: payload.subtitles,
        attachments: payload.attachments,
        cover_art: payload.cover_art,
        chapters: payload.chapters,
        data: payload.data,
    };
    let subtitle_format = sanitize_ext_field(
        "subtitle_format",
        payload.subtitle_format.as_deref().unwrap_or("srt"),
    )?;
    let out = crate::crates::video_engine::VideoEngine::new()
        .extract_streams(
            &data,
            &selection,
            &subtitle_format,
            &video_limits(payload.max_bytes),
        )
        .map_err(|e| {
            video_err(
                &format!("video extract_streams failed for {:?}", payload.path),
                e,
            )
        })?;
    write_media_output(&path_with_suffix(&file, ".streams"), &out)
}

/// `video.audio_volume` implementation: apply `gain_db` to all audio
/// and write `<path>.vol<ext>`.
fn audio_volume_video_at(root: &Path, payload: &VideoAudioVolumePayload) -> Result<(), JobError> {
    video_transform_at(
        root,
        &payload.path,
        payload.max_bytes,
        VIDEO_AUDIO_VOLUME_KIND,
        ".vol",
        |data, limits| {
            crate::crates::video_engine::VideoEngine::new().audio_volume(
                data,
                payload.gain_db,
                limits,
            )
        },
    )
}

/// `video.contact_sheet` implementation: build a tiled PNG of sampled
/// frames and write it to `<path>.sheet.png`. Grid defaults: 4×4
/// cells of 320 px-wide thumbnails.
fn contact_sheet_video_at(root: &Path, payload: &VideoContactSheetPayload) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let png = crate::crates::video_engine::VideoEngine::new()
        .contact_sheet(
            &data,
            payload.cols.unwrap_or(4),
            payload.rows.unwrap_or(4),
            payload.thumb_width.unwrap_or(320),
            payload.interval_secs,
            &video_limits(payload.max_bytes),
        )
        .map_err(|e| {
            video_err(
                &format!("video contact_sheet failed for {:?}", payload.path),
                e,
            )
        })?;
    write_output_atomic(&path_with_suffix(&file, ".sheet.png"), &png)
}

/// `video.speed` implementation: retime by `factor` and write
/// `<path>.speed<ext>`.
fn speed_video_at(root: &Path, payload: &VideoSpeedPayload) -> Result<(), JobError> {
    video_transform_at(
        root,
        &payload.path,
        payload.max_bytes,
        VIDEO_SPEED_KIND,
        ".speed",
        |data, limits| {
            crate::crates::video_engine::VideoEngine::new().set_speed(data, payload.factor, limits)
        },
    )
}

/// `video.reverse` implementation: reverse the clip and write
/// `<path>.reverse<ext>`.
fn reverse_video_at(root: &Path, payload: &VideoPathPayload) -> Result<(), JobError> {
    video_transform_at(
        root,
        &payload.path,
        payload.max_bytes,
        VIDEO_REVERSE_KIND,
        ".reverse",
        |data, limits| crate::crates::video_engine::VideoEngine::new().reverse(data, limits),
    )
}

/// `video.loop` implementation: append `count` extra plays and write
/// `<path>.loop<ext>`.
fn loop_video_at(root: &Path, payload: &VideoLoopPayload) -> Result<(), JobError> {
    video_transform_at(
        root,
        &payload.path,
        payload.max_bytes,
        VIDEO_LOOP_KIND,
        ".loop",
        |data, limits| {
            crate::crates::video_engine::VideoEngine::new().loop_clip(data, payload.count, limits)
        },
    )
}

/// `video.freeze` implementation: freeze the frame at `at_secs` for
/// `duration_secs` and write `<path>.freeze<ext>`.
fn freeze_video_at(root: &Path, payload: &VideoFreezePayload) -> Result<(), JobError> {
    video_transform_at(
        root,
        &payload.path,
        payload.max_bytes,
        VIDEO_FREEZE_KIND,
        ".freeze",
        |data, limits| {
            crate::crates::video_engine::VideoEngine::new().freeze_frame(
                data,
                payload.at_secs,
                payload.duration_secs,
                limits,
            )
        },
    )
}

/// `video.resize` implementation: resize to `width`×`height` per `fit`
/// and write `<path>.resize<ext>`.
fn resize_video_at(root: &Path, payload: &VideoResizePayload) -> Result<(), JobError> {
    let fit = parse_fit_mode(payload.fit.as_deref())?;
    video_transform_at(
        root,
        &payload.path,
        payload.max_bytes,
        VIDEO_RESIZE_KIND,
        ".resize",
        |data, limits| {
            crate::crates::video_engine::VideoEngine::new().resize_video(
                data,
                payload.width,
                payload.height,
                fit,
                limits,
            )
        },
    )
}

/// `video.crop` implementation: crop to `(x, y, w, h)` and write
/// `<path>.crop<ext>`.
fn crop_video_at(root: &Path, payload: &VideoCropPayload) -> Result<(), JobError> {
    video_transform_at(
        root,
        &payload.path,
        payload.max_bytes,
        VIDEO_CROP_KIND,
        ".crop",
        |data, limits| {
            crate::crates::video_engine::VideoEngine::new()
                .crop_video(data, payload.x, payload.y, payload.w, payload.h, limits)
        },
    )
}

/// `video.rotate` implementation: rotate by `degrees` and write
/// `<path>.rotate<ext>`.
fn rotate_video_at(root: &Path, payload: &VideoRotatePayload) -> Result<(), JobError> {
    video_transform_at(
        root,
        &payload.path,
        payload.max_bytes,
        VIDEO_ROTATE_KIND,
        ".rotate",
        |data, limits| {
            crate::crates::video_engine::VideoEngine::new().rotate_video(
                data,
                payload.degrees,
                limits,
            )
        },
    )
}

/// `video.flip` implementation: mirror horizontally (`horizontal`
/// unset/`true`) or vertically (`false`) and write `<path>.flip<ext>`.
fn flip_video_at(root: &Path, payload: &VideoFlipPayload) -> Result<(), JobError> {
    let horizontal = payload.horizontal.unwrap_or(true);
    video_transform_at(
        root,
        &payload.path,
        payload.max_bytes,
        VIDEO_FLIP_KIND,
        ".flip",
        |data, limits| {
            crate::crates::video_engine::VideoEngine::new().flip_video(data, horizontal, limits)
        },
    )
}

/// `video.gif` implementation: convert the video to an animated GIF
/// and write `<path>.gif`.
fn gif_video_at(root: &Path, payload: &VideoGifPayload) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let bytes = crate::crates::video_engine::VideoEngine::new()
        .video_to_gif(
            &data,
            payload.fps,
            payload.width,
            &video_limits(payload.max_bytes),
        )
        .map_err(|e| video_err(&format!("video gif failed for {:?}", payload.path), e))?;
    write_output_atomic(&path_with_suffix(&file, ".gif"), &bytes)
}

/// `video.slideshow` implementation: read every path in `paths`
/// (each confined + size-checked), stitch them into a video at `fps`
/// (`None` → 10), and write `slideshow.<target_ext>` in the first
/// input's directory.
fn slideshow_video_at(root: &Path, payload: &VideoSlideshowPayload) -> Result<(), JobError> {
    if payload.paths.is_empty() {
        return Err(JobError::Failed(
            "payload.paths must contain at least one image".to_string(),
        ));
    }
    let ext = sanitize_target_ext(&payload.target_ext)?;
    let mut inputs = Vec::with_capacity(payload.paths.len());
    let mut first_file: Option<PathBuf> = None;
    for rel in &payload.paths {
        let (file, data) = read_video_input(root, rel, payload.max_bytes)?;
        if first_file.is_none() {
            first_file = Some(file);
        }
        inputs.push(data);
    }
    let file = first_file.expect("paths is non-empty");
    let fps = payload.fps.unwrap_or(10.0);
    let bytes = crate::crates::video_engine::VideoEngine::new()
        .images_to_video(
            &inputs,
            fps,
            payload.duration_per_image,
            &ext,
            &video_limits(payload.max_bytes),
        )
        .map_err(|e| video_err("video slideshow failed", e))?;
    let dir = file.parent().unwrap_or(root);
    write_output_atomic(&dir.join(format!("slideshow.{ext}")), &bytes)
}

/// `video.normalize` implementation: EBU R128 loudness normalization
/// toward `target_lufs` (`None` → engine default) and write
/// `<path>.norm<ext>`.
fn normalize_video_at(root: &Path, payload: &VideoNormalizePayload) -> Result<(), JobError> {
    video_transform_at(
        root,
        &payload.path,
        payload.max_bytes,
        VIDEO_NORMALIZE_KIND,
        ".norm",
        |data, limits| {
            crate::crates::video_engine::VideoEngine::new().audio_normalize(
                data,
                payload.target_lufs,
                limits,
            )
        },
    )
}

/// `video.loudness` implementation: measure EBU R128 loudness and
/// write the pretty-JSON [`LoudnessReport`] to
/// `<path>.loudness.json`.
fn loudness_video_at(root: &Path, payload: &VideoPathPayload) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let report = crate::crates::video_engine::VideoEngine::new()
        .measure_loudness(&data, &video_limits(payload.max_bytes))
        .map_err(|e| video_err(&format!("video loudness failed for {:?}", payload.path), e))?;
    let json = serde_json::to_string_pretty(&report)?;
    write_output_atomic(&path_with_suffix(&file, ".loudness.json"), json.as_bytes())
}

/// `video.scene_frames` implementation: extract one frame per
/// detected scene change (`threshold` 0.0–1.0, default 0.4) and write
/// the [`MediaOutput`] set under `<path>.scenes/`.
fn scene_frames_video_at(root: &Path, payload: &VideoSceneFramesPayload) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let out = crate::crates::video_engine::VideoEngine::new()
        .scene_keyframes(
            &data,
            payload.threshold.unwrap_or(0.4),
            &video_limits(payload.max_bytes),
        )
        .map_err(|e| {
            video_err(
                &format!("video scene_frames failed for {:?}", payload.path),
                e,
            )
        })?;
    write_media_output(&path_with_suffix(&file, ".scenes"), &out)
}

/// `video.merge_av` implementation: merge `audio_path` into
/// `video_path` (video stream copied) and write
/// `<video_path>.merged<ext>`. `target_ext` defaults to the video
/// input's extension, or `"mp4"`.
fn merge_av_video_at(root: &Path, payload: &VideoMergeAvPayload) -> Result<(), JobError> {
    let (video_file, video) = read_video_input(root, &payload.video_path, payload.max_bytes)?;
    let (_audio_file, audio) = read_video_input(root, &payload.audio_path, payload.max_bytes)?;
    let ext = match &payload.target_ext {
        Some(e) => sanitize_target_ext(e)?,
        None => video_file
            .extension()
            .and_then(|e| e.to_str())
            .and_then(|e| sanitize_target_ext(e).ok())
            .unwrap_or_else(|| "mp4".to_string()),
    };
    let out = path_with_suffix(&video_file, &format!(".merged.{ext}"));
    let bytes = crate::crates::video_engine::VideoEngine::new()
        .merge_av(&video, &audio, &ext, &video_limits(payload.max_bytes))
        .map_err(|e| video_err("video merge_av failed", e))?;
    write_output_atomic(&out, &bytes)
}

// ---------------------------------------------------------------------------
// Additional video.* handlers (validate / chapters / hdr / segments /
// waveform / sync / stabilize / preview / proxy / stream select / filters)
// ---------------------------------------------------------------------------
//
// Same confinement + atomic-output model as the handlers above.
// Report ops write pretty JSON (`<path>.<report>.json`); single-file
// transforms reuse [`video_transform_at`]; multi-file segment splits
// go through [`write_media_output`] like `video.extract_frames`.

/// `video.validate` implementation: run the full validation pipeline
/// and write the pretty-JSON [`ValidationReport`] to
/// `<path>.validate.json`.
fn validate_video_at(root: &Path, payload: &VideoPathPayload) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let report = crate::crates::video_engine::VideoEngine::new()
        .validate_media(&data, &video_limits(payload.max_bytes))
        .map_err(|e| video_err(&format!("video validate failed for {:?}", payload.path), e))?;
    let json = serde_json::to_string_pretty(&report)?;
    write_output_atomic(&path_with_suffix(&file, ".validate.json"), json.as_bytes())
}

/// `video.chapters_list` implementation: list embedded chapters and
/// write `{"chapters": [...]}` (millisecond bounds) to
/// `<path>.chapters.json`. [`ChapterEdit`](crate::crates::video_engine::ChapterEdit)
/// is not `Serialize`, so the report is built field-by-field.
fn chapters_list_video_at(root: &Path, payload: &VideoPathPayload) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let chapters = crate::crates::video_engine::VideoEngine::new()
        .chapters_list(&data)
        .map_err(|e| {
            video_err(
                &format!("video chapters_list failed for {:?}", payload.path),
                e,
            )
        })?;
    let list: Vec<serde_json::Value> = chapters
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "start_ms": c.start_ms,
                "end_ms": c.end_ms,
                "title": c.title,
            })
        })
        .collect();
    let json = serde_json::to_string_pretty(&json!({ "chapters": list }))?;
    write_output_atomic(&path_with_suffix(&file, ".chapters.json"), json.as_bytes())
}

/// Extension suffix (`mp4`/`mkv`/...) for outputs whose container the
/// engine derives from the probed input format (chapters, cover art):
/// the input's own extension when present and safe, else `"mp4"` —
/// same fallback as `video.metadata_strip`.
fn derived_container_ext(file: &Path) -> String {
    file.extension()
        .and_then(|e| e.to_str())
        .map(|e| sanitize_target_ext(e).unwrap_or_else(|_| "mp4".to_string()))
        .unwrap_or_else(|| "mp4".to_string())
}

/// `video.chapters_set` implementation: replace the chapter table and
/// write the remuxed file to `<path>.chapters<ext>`.
fn chapters_set_video_at(root: &Path, payload: &VideoChaptersSetPayload) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let owned: Vec<(i64, i64, String)> = payload
        .chapters
        .iter()
        .map(|c| (c.start_ms, c.end_ms, c.title.clone().unwrap_or_default()))
        .collect();
    let chapters: Vec<(i64, i64, &str)> =
        owned.iter().map(|(s, e, t)| (*s, *e, t.as_str())).collect();
    let ext = derived_container_ext(&file);
    let out = path_with_suffix(&file, &format!(".chapters.{ext}"));
    let bytes = crate::crates::video_engine::VideoEngine::new()
        .chapters_set(&data, &chapters, &video_limits(payload.max_bytes))
        .map_err(|e| {
            video_err(
                &format!("video chapters_set failed for {:?}", payload.path),
                e,
            )
        })?;
    write_output_atomic(&out, &bytes)
}

/// `video.chapters_shift` implementation: shift every chapter by
/// `offset_ms` and write `<path>.chapters_shift<ext>`.
fn chapters_shift_video_at(
    root: &Path,
    payload: &VideoChaptersShiftPayload,
) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let ext = derived_container_ext(&file);
    let out = path_with_suffix(&file, &format!(".chapters_shift.{ext}"));
    let bytes = crate::crates::video_engine::VideoEngine::new()
        .chapters_shift(&data, payload.offset_ms, &video_limits(payload.max_bytes))
        .map_err(|e| {
            video_err(
                &format!("video chapters_shift failed for {:?}", payload.path),
                e,
            )
        })?;
    write_output_atomic(&out, &bytes)
}

/// `video.chapters_remove` implementation: drop the chapter table and
/// write `<path>.chapters_remove<ext>`.
fn chapters_remove_video_at(root: &Path, payload: &VideoPathPayload) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let ext = derived_container_ext(&file);
    let out = path_with_suffix(&file, &format!(".chapters_remove.{ext}"));
    let bytes = crate::crates::video_engine::VideoEngine::new()
        .chapters_remove(&data, &video_limits(payload.max_bytes))
        .map_err(|e| {
            video_err(
                &format!("video chapters_remove failed for {:?}", payload.path),
                e,
            )
        })?;
    write_output_atomic(&out, &bytes)
}

/// `video.hdr_detect` implementation: probe the primary video stream's
/// HDR signalling and write the pretty-JSON [`HdrInfo`] report to
/// `<path>.hdr.json`. [`HdrInfo`](crate::crates::video_engine::HdrInfo)
/// is not `Serialize`, so the report is built field-by-field.
fn hdr_detect_video_at(root: &Path, payload: &VideoPathPayload) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let info = crate::crates::video_engine::VideoEngine::new()
        .hdr_detect(&data)
        .map_err(|e| {
            video_err(
                &format!("video hdr_detect failed for {:?}", payload.path),
                e,
            )
        })?;
    let json = serde_json::to_string_pretty(&json!({
        "is_hdr": info.is_hdr,
        "color_transfer": info.color_transfer,
        "color_primaries": info.color_primaries,
        "color_space": info.color_space,
        "bit_depth": info.bit_depth,
        "pix_fmt": info.pix_fmt,
    }))?;
    write_output_atomic(&path_with_suffix(&file, ".hdr.json"), json.as_bytes())
}

/// `video.hdr_tone_map` implementation: tone-map HDR `path` to SDR
/// per `desat`/`peak` and write `<path>.tonemap<ext>`.
fn hdr_tone_map_video_at(root: &Path, payload: &VideoHdrToneMapPayload) -> Result<(), JobError> {
    let params = crate::crates::video_engine::HdrToneMapParams {
        desat: payload.desat,
        peak: payload.peak,
    };
    video_transform_at(
        root,
        &payload.path,
        payload.max_bytes,
        VIDEO_HDR_TONE_MAP_KIND,
        ".tonemap",
        |data, limits| {
            crate::crates::video_engine::VideoEngine::new().hdr_tone_map(data, &params, limits)
        },
    )
}

/// Container extensions accepted by the `video.segment_*` kinds —
/// mirrors the engine's segment allowlist (`mp4`/`mkv`/`webm`/`avi`).
const SEGMENT_FORMATS: &[&str] = &["mp4", "mkv", "webm", "avi"];

/// Resolves the `format` field of the `video.segment_*` payloads:
/// an explicit value is sanitized and checked against
/// [`SEGMENT_FORMATS`]; `None` falls back to the input's extension
/// when it is an allowed container, else `"mp4"`.
fn segment_format(format: Option<&str>, file: &Path) -> Result<String, JobError> {
    match format {
        Some(f) => {
            let ext = sanitize_ext_field("format", f)?;
            if SEGMENT_FORMATS.contains(&ext.as_str()) {
                Ok(ext)
            } else {
                Err(JobError::Failed(format!(
                    "payload.format is not a supported segment container: {f:?} \
                     (expected mp4|mkv|webm|avi)"
                )))
            }
        }
        None => {
            let ext = file
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .unwrap_or_default();
            if SEGMENT_FORMATS.contains(&ext.as_str()) {
                Ok(ext)
            } else {
                Ok("mp4".to_string())
            }
        }
    }
}

/// `video.segment_split` implementation: split `path` per `mode`
/// (`"duration"` default → `duration_secs`, `"scenes"` → `threshold`,
/// `"equal"` → `parts`) and write `seg-NNN.<format>` files plus a
/// `manifest.json` under `<path>.segments/`.
fn segment_split_video_at(root: &Path, payload: &VideoSegmentSplitPayload) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let format = segment_format(payload.format.as_deref(), &file)?;
    let limits = video_limits(payload.max_bytes);
    let engine = crate::crates::video_engine::VideoEngine::new();
    let mode = payload.mode.as_deref().unwrap_or("duration");
    let result = match mode.trim().to_ascii_lowercase().as_str() {
        "duration" => {
            let secs = payload.duration_secs.ok_or_else(|| {
                JobError::Failed(
                    "payload.duration_secs is required for mode \"duration\"".to_string(),
                )
            })?;
            engine.split_by_duration(&data, secs, &format, &limits)
        }
        "scenes" => {
            engine.split_by_scenes(&data, payload.threshold.unwrap_or(0.4), &format, &limits)
        }
        "equal" => {
            let parts = payload.parts.ok_or_else(|| {
                JobError::Failed("payload.parts is required for mode \"equal\"".to_string())
            })?;
            engine.split_equal(&data, parts, &format, &limits)
        }
        other => {
            return Err(JobError::Failed(format!(
                "payload.mode is not a valid split mode: {other:?} \
                 (expected duration|scenes|equal)"
            )));
        }
    }
    .map_err(|e| {
        video_err(
            &format!("video segment_split failed for {:?}", payload.path),
            e,
        )
    })?;

    let mut files = Vec::with_capacity(result.segments.len() + 1);
    let mut manifest = Vec::with_capacity(result.segments.len());
    for seg in result.segments {
        let name = format!("seg-{:03}.{format}", seg.index);
        manifest.push(json!({
            "index": seg.index,
            "start_ms": seg.start_ms,
            "end_ms": seg.end_ms,
            "name": name,
        }));
        files.push(crate::crates::video_engine::MediaFile {
            name,
            bytes: seg.bytes,
        });
    }
    files.push(crate::crates::video_engine::MediaFile {
        name: "manifest.json".to_string(),
        bytes: serde_json::to_vec_pretty(&json!({ "segments": manifest }))?,
    });
    write_media_output(
        &path_with_suffix(&file, ".segments"),
        &crate::crates::video_engine::MediaOutput { files },
    )
}

/// `video.segment_extract` implementation: cut `[start_ms, end_ms)`
/// of `path` into `format` and write `<path>.extract.<format>`.
fn segment_extract_video_at(
    root: &Path,
    payload: &VideoSegmentExtractPayload,
) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let format = segment_format(payload.format.as_deref(), &file)?;
    let bytes = crate::crates::video_engine::VideoEngine::new()
        .extract_segment(
            &data,
            payload.start_ms,
            payload.end_ms,
            &format,
            &video_limits(payload.max_bytes),
        )
        .map_err(|e| {
            video_err(
                &format!("video segment_extract failed for {:?}", payload.path),
                e,
            )
        })?;
    write_output_atomic(
        &path_with_suffix(&file, &format!(".extract.{format}")),
        &bytes,
    )
}

/// Canvas size cap for the `video.waveform`-family kinds — bounds the
/// ffmpeg `s=WxH` filter argument so a hostile payload can't request
/// an absurd raster.
const MAX_WAVEFORM_DIMENSION: u32 = 16_384;

/// Shared body for `video.waveform`/`video.spectrogram`/
/// `video.vectorscope`: render `kind` as a PNG still and write it to
/// `<path>.<suffix>.png`.
fn waveform_image_at(
    root: &Path,
    payload: &VideoWaveformPayload,
    kind: crate::crates::video_engine::WaveformKind,
    suffix: &str,
) -> Result<(), JobError> {
    let width = payload.width.unwrap_or(800);
    let height = payload.height.unwrap_or(400);
    if width == 0
        || height == 0
        || width > MAX_WAVEFORM_DIMENSION
        || height > MAX_WAVEFORM_DIMENSION
    {
        return Err(JobError::Failed(format!(
            "payload width/height must be in 1..={MAX_WAVEFORM_DIMENSION}, got {width}x{height}"
        )));
    }
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let params = crate::crates::video_engine::WaveformParams {
        kind,
        width,
        height,
        color: payload.color.clone(),
        scale: payload.scale.clone(),
    };
    let png = crate::crates::video_engine::VideoEngine::new()
        .audio_waveform_image(&data, &params)
        .map_err(|e| video_err(&format!("video {suffix} failed for {:?}", payload.path), e))?;
    write_output_atomic(&path_with_suffix(&file, &format!("{suffix}.png")), &png)
}

/// `video.waveform` implementation: amplitude waveform PNG at
/// `<path>.waveform.png`.
fn waveform_video_at(root: &Path, payload: &VideoWaveformPayload) -> Result<(), JobError> {
    waveform_image_at(
        root,
        payload,
        crate::crates::video_engine::WaveformKind::Waveform,
        "waveform",
    )
}

/// `video.spectrogram` implementation: frequency-vs-time spectrogram
/// PNG at `<path>.spectrogram.png`.
fn spectrogram_video_at(root: &Path, payload: &VideoWaveformPayload) -> Result<(), JobError> {
    waveform_image_at(
        root,
        payload,
        crate::crates::video_engine::WaveformKind::Spectrogram,
        "spectrogram",
    )
}

/// `video.vectorscope` implementation: stereo Lissajous vectorscope
/// PNG at `<path>.vectorscope.png`.
fn vectorscope_video_at(root: &Path, payload: &VideoWaveformPayload) -> Result<(), JobError> {
    waveform_image_at(
        root,
        payload,
        crate::crates::video_engine::WaveformKind::Vectorscope,
        "vectorscope",
    )
}

/// `video.sync_report` implementation: measure A/V stream start times
/// and write the pretty-JSON [`SyncReport`] to `<path>.sync.json`.
/// [`SyncReport`](crate::crates::video_engine::SyncReport) is not
/// `Serialize`, so the report is built field-by-field.
fn sync_report_video_at(root: &Path, payload: &VideoPathPayload) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let report = crate::crates::video_engine::VideoEngine::new()
        .av_sync_report(&data)
        .map_err(|e| {
            video_err(
                &format!("video sync_report failed for {:?}", payload.path),
                e,
            )
        })?;
    let json = serde_json::to_string_pretty(&json!({
        "audio_start_ms": report.audio_start_ms,
        "video_start_ms": report.video_start_ms,
        "applied_audio_shift_ms": report.applied_audio_shift_ms,
        "applied_video_shift_ms": report.applied_video_shift_ms,
    }))?;
    write_output_atomic(&path_with_suffix(&file, ".sync.json"), json.as_bytes())
}

/// `video.sync_fix` implementation: apply manual audio/video stream
/// shifts (`*_offset_ms`, default 0) and write `<path>.syncfix<ext>`.
fn sync_fix_video_at(root: &Path, payload: &VideoSyncFixPayload) -> Result<(), JobError> {
    let params = crate::crates::video_engine::SyncParams {
        audio_offset_ms: payload.audio_offset_ms.unwrap_or(0),
        video_offset_ms: payload.video_offset_ms.unwrap_or(0),
    };
    video_transform_at(
        root,
        &payload.path,
        payload.max_bytes,
        VIDEO_SYNC_FIX_KIND,
        ".syncfix",
        |data, limits| {
            crate::crates::video_engine::VideoEngine::new().av_sync_fix(data, &params, limits)
        },
    )
}

/// `video.sync_auto` implementation: automatically align A/V start
/// times and write `<path>.syncauto<ext>`.
fn sync_auto_video_at(root: &Path, payload: &VideoPathPayload) -> Result<(), JobError> {
    video_transform_at(
        root,
        &payload.path,
        payload.max_bytes,
        VIDEO_SYNC_AUTO_KIND,
        ".syncauto",
        |data, limits| crate::crates::video_engine::VideoEngine::new().av_sync_auto(data, limits),
    )
}

/// Parses the `video.stabilize` `algo` field (`"vidstab"` default,
/// `"deshake"`).
fn parse_stabilize_algo(
    algo: Option<&str>,
) -> Result<crate::crates::video_engine::StabilizeAlgo, JobError> {
    use crate::crates::video_engine::StabilizeAlgo;
    match algo.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("vidstab") => Ok(StabilizeAlgo::Vidstab),
        Some("deshake") => Ok(StabilizeAlgo::Deshake),
        Some(other) => Err(JobError::Failed(format!(
            "payload.algo is not a valid stabilize backend: {other:?} \
             (expected vidstab|deshake)"
        ))),
    }
}

/// `video.stabilize` implementation: stabilize per `shakiness`/
/// `smoothing`/`algo` (or the best available backend when `auto`) and
/// write `<path>.stabilize<ext>`.
fn stabilize_video_at(root: &Path, payload: &VideoStabilizePayload) -> Result<(), JobError> {
    let algo = parse_stabilize_algo(payload.algo.as_deref())?;
    let params = crate::crates::video_engine::StabilizeParams {
        shakiness: payload.shakiness.unwrap_or(5.0),
        smoothing: payload.smoothing.unwrap_or(15.0),
        algo,
    };
    let auto = payload.auto;
    video_transform_at(
        root,
        &payload.path,
        payload.max_bytes,
        VIDEO_STABILIZE_KIND,
        ".stabilize",
        |data, limits| {
            let engine = crate::crates::video_engine::VideoEngine::new();
            if auto {
                engine.stabilize_auto(data, &params, limits)
            } else {
                engine.stabilize_video(data, &params, limits)
            }
        },
    )
}

/// Shared body for `video.preview_gif`/`video.preview_webp`:
/// [`PreviewParams`] defaults mirror the engine's (`5s`, `10 fps`,
/// `320 px` wide); the output name is fixed per format.
fn preview_video_at(
    root: &Path,
    payload: &VideoPreviewPayload,
    format: crate::crates::video_engine::PreviewFormat,
    out_name: &str,
) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let params = crate::crates::video_engine::PreviewParams {
        max_duration_secs: payload.max_duration_secs.unwrap_or(5.0),
        fps: payload.fps.unwrap_or(10.0),
        width: payload.width.unwrap_or(320),
        format,
    };
    let bytes = crate::crates::video_engine::VideoEngine::new()
        .video_preview(&data, &params, &video_limits(payload.max_bytes))
        .map_err(|e| video_err(&format!("video preview failed for {:?}", payload.path), e))?;
    write_output_atomic(&path_with_suffix(&file, out_name), &bytes)
}

/// `video.preview_gif` implementation: animated GIF preview at
/// `<path>.preview.gif`.
fn preview_gif_video_at(root: &Path, payload: &VideoPreviewPayload) -> Result<(), JobError> {
    preview_video_at(
        root,
        payload,
        crate::crates::video_engine::PreviewFormat::Gif,
        ".preview.gif",
    )
}

/// `video.preview_webp` implementation: animated WebP preview at
/// `<path>.preview.webp`.
fn preview_webp_video_at(root: &Path, payload: &VideoPreviewPayload) -> Result<(), JobError> {
    preview_video_at(
        root,
        payload,
        crate::crates::video_engine::PreviewFormat::WebpAnimated,
        ".preview.webp",
    )
}

/// Parses the `video.proxy_generate` `kind` field.
fn parse_proxy_kind(
    kind: Option<&str>,
) -> Result<crate::crates::video_engine::ProxyKind, JobError> {
    use crate::crates::video_engine::ProxyKind;
    match kind.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("proxy") => Ok(ProxyKind::Proxy),
        Some("mezzanine") => Ok(ProxyKind::Mezzanine),
        Some("archival") => Ok(ProxyKind::Archival),
        Some(other) => Err(JobError::Failed(format!(
            "payload.kind is not a valid proxy kind: {other:?} \
             (expected proxy|mezzanine|archival)"
        ))),
    }
}

/// Guesses a container extension from produced bytes for naming
/// `video.proxy_generate` output: EBML magic → `mkv`, an `ftyp` box →
/// `mp4` (covers `mov` too), anything else → `bin`. The engine picks
/// the real container from encoder availability, so the payload's
/// `kind` alone can't predict it.
fn guess_media_ext(bytes: &[u8]) -> &'static str {
    const EBML: [u8; 4] = [0x1A, 0x45, 0xDF, 0xA3];
    if bytes.starts_with(&EBML) {
        "mkv"
    } else if bytes.len() > 8 && &bytes[4..8] == b"ftyp" {
        "mp4"
    } else {
        "bin"
    }
}

/// `video.proxy_generate` implementation: derive a `proxy`,
/// `mezzanine`, or `archival` media file and write it to
/// `<path>.<kind>.<ext>` (`ext` sniffed from the produced bytes).
fn proxy_generate_video_at(
    root: &Path,
    payload: &VideoProxyGeneratePayload,
) -> Result<(), JobError> {
    let kind = parse_proxy_kind(payload.kind.as_deref())?;
    let kind_name = match kind {
        crate::crates::video_engine::ProxyKind::Proxy => "proxy",
        crate::crates::video_engine::ProxyKind::Mezzanine => "mezzanine",
        crate::crates::video_engine::ProxyKind::Archival => "archival",
    };
    let params = crate::crates::video_engine::ProxyParams {
        kind,
        target_height: payload.target_height,
        crf: payload.crf,
        preset: payload.preset.clone(),
    };
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let bytes = crate::crates::video_engine::VideoEngine::new()
        .generate_proxy(&data, &params, &video_limits(payload.max_bytes))
        .map_err(|e| {
            video_err(
                &format!("video proxy_generate failed for {:?}", payload.path),
                e,
            )
        })?;
    let out = path_with_suffix(&file, &format!(".{kind_name}.{}", guess_media_ext(&bytes)));
    write_output_atomic(&out, &bytes)
}

/// `video.stream_select` implementation: map streams per the payload
/// selection into `target_ext` (default: input extension or `"mp4"`)
/// and write `<path>.select<ext>`.
fn stream_select_video_at(root: &Path, payload: &VideoStreamSelectPayload) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let ext = match &payload.target_ext {
        Some(e) => sanitize_target_ext(e)?,
        None => file
            .extension()
            .and_then(|e| e.to_str())
            .and_then(|e| sanitize_target_ext(e).ok())
            .unwrap_or_else(|| "mp4".to_string()),
    };
    let filter = crate::crates::video_engine::StreamFilter {
        keep_video: payload.keep_video.unwrap_or(true),
        audio_ordinals: payload.audio_ordinals.clone(),
        subtitle_ordinals: payload.subtitle_ordinals.clone(),
        drop_attachments: payload.drop_attachments.unwrap_or(false),
    };
    let out = path_with_suffix(&file, &format!(".select.{ext}"));
    let bytes = crate::crates::video_engine::VideoEngine::new()
        .select_streams(&data, &filter, &ext, &video_limits(payload.max_bytes))
        .map_err(|e| {
            video_err(
                &format!("video stream_select failed for {:?}", payload.path),
                e,
            )
        })?;
    write_output_atomic(&out, &bytes)
}

/// `video.stream_cover_art` implementation: attach the image at
/// `cover_path` as cover art and write `<path>.cover<ext>` (audio
/// containers only — the engine rejects others honestly).
fn stream_cover_art_video_at(
    root: &Path,
    payload: &VideoStreamCoverArtPayload,
) -> Result<(), JobError> {
    let (file, data) = read_video_input(root, &payload.path, payload.max_bytes)?;
    let (_cover_file, cover) = read_video_input(root, &payload.cover_path, payload.max_bytes)?;
    let ext = derived_container_ext(&file);
    let out = path_with_suffix(&file, &format!(".cover.{ext}"));
    let bytes = crate::crates::video_engine::VideoEngine::new()
        .set_cover_art(&data, &cover, &video_limits(payload.max_bytes))
        .map_err(|e| {
            video_err(
                &format!("video stream_cover_art failed for {:?}", payload.path),
                e,
            )
        })?;
    write_output_atomic(&out, &bytes)
}

/// `video.filters_apply` implementation: resolve `preset` (a named
/// entry from [`VideoEngine::filter_presets`], case-insensitive)
/// into `-vf`/`-af` chains, let explicit `video_filter`/
/// `audio_filter` override per chain, and write `<path>.filters<ext>`.
fn filters_apply_video_at(root: &Path, payload: &VideoFiltersApplyPayload) -> Result<(), JobError> {
    let mut video_filter = payload.video_filter.clone();
    let mut audio_filter = payload.audio_filter.clone();
    if let Some(name) = payload.preset.as_deref() {
        let preset = crate::crates::video_engine::VideoEngine::new()
            .filter_presets()
            .into_iter()
            .find(|p| p.name.eq_ignore_ascii_case(name.trim()))
            .ok_or_else(|| JobError::Failed(format!("unknown filter preset: {name:?}")))?;
        if video_filter.is_none() && !preset.video_filter.is_empty() {
            video_filter = Some(preset.video_filter.to_string());
        }
        if audio_filter.is_none() && !preset.audio_filter.is_empty() {
            audio_filter = Some(preset.audio_filter.to_string());
        }
    }
    let has_vf = video_filter
        .as_deref()
        .is_some_and(|f| !f.trim().is_empty());
    let has_af = audio_filter
        .as_deref()
        .is_some_and(|f| !f.trim().is_empty());
    if !has_vf && !has_af {
        return Err(JobError::Failed(
            "payload must set preset, video_filter, or audio_filter".to_string(),
        ));
    }
    video_transform_at(
        root,
        &payload.path,
        payload.max_bytes,
        VIDEO_FILTERS_APPLY_KIND,
        ".filters",
        |data, limits| {
            crate::crates::video_engine::VideoEngine::new().apply_filters(
                data,
                video_filter.as_deref(),
                audio_filter.as_deref(),
                limits,
            )
        },
    )
}

/// Validates a `MediaOutput` relative name for writing under a job
/// output directory: rejects empty, absolute, and `..`/non-normal
/// components (same confinement rule as [`resolve_under_root`]).
fn sanitize_media_name(name: &str) -> Result<PathBuf, JobError> {
    let rel = Path::new(name);
    let ok = !name.trim().is_empty()
        && !rel.is_absolute()
        && rel
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if !ok {
        return Err(JobError::Failed(format!(
            "media output name escapes its directory: {name:?}"
        )));
    }
    Ok(rel.to_path_buf())
}

/// Writes every file of a [`MediaOutput`] under `dir` (created if
/// needed), each atomically via [`write_output_atomic`]. Names are
/// sanitized by [`sanitize_media_name`]; an empty output is an error.
fn write_media_output(
    dir: &Path,
    out: &crate::crates::video_engine::MediaOutput,
) -> Result<(), JobError> {
    if out.files.is_empty() {
        return Err(JobError::Failed(
            "video operation produced no output files".to_string(),
        ));
    }
    std::fs::create_dir_all(dir)
        .map_err(|e| JobError::Failed(format!("cannot create {}: {e}", dir.display())))?;
    for file in &out.files {
        let rel = sanitize_media_name(&file.name)?;
        write_output_atomic(&dir.join(rel), &file.bytes)?;
    }
    Ok(())
}

/// Like [`sanitize_target_ext`] but reports `field` (e.g. `"format"`,
/// `"subtitle_format"`) in the error — used for payload fields that
/// become file extensions or ffmpeg format selectors.
fn sanitize_ext_field(field: &str, value: &str) -> Result<String, JobError> {
    let cleaned = value.trim().trim_start_matches('.').to_ascii_lowercase();
    let ok = !cleaned.is_empty()
        && cleaned.len() <= 16
        && cleaned.chars().all(|c| c.is_ascii_alphanumeric());
    if !ok {
        return Err(JobError::Failed(format!(
            "payload.{field} is not a safe extension: {value:?}"
        )));
    }
    Ok(cleaned)
}

/// Parses the `video.resize` `fit` field into a [`FitMode`].
/// `None`/empty/`"fit"` → [`FitMode::Fit`]; unknown values are
/// rejected rather than silently approximated.
fn parse_fit_mode(fit: Option<&str>) -> Result<crate::crates::video_engine::FitMode, JobError> {
    use crate::crates::video_engine::FitMode;
    match fit.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("fit") => Ok(FitMode::Fit),
        Some("cover") => Ok(FitMode::Cover),
        Some("contain") => Ok(FitMode::Contain),
        Some("stretch") => Ok(FitMode::Stretch),
        Some(other) => Err(JobError::Failed(format!(
            "payload.fit is not a valid fit mode: {other:?} (expected fit|cover|contain|stretch)"
        ))),
    }
}

// ---------------------------------------------------------------------------
// storage.gc_staging
// ---------------------------------------------------------------------------

/// `storage.gc_staging` — removes abandoned staging objects.
///
/// Implements the crash-recovery requirement: uploads that were staged
/// but never committed (e.g. the app died mid-operation) are deleted
/// once they are older than `retention_secs`. See module docs for the
/// payload contract.
struct StorageGcStagingHandler;

impl JobHandler for StorageGcStagingHandler {
    fn handle<'a>(
        &'a self,
        _ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let payload: GcStagingPayload = serde_json::from_value(job.payload.clone())?;
            let root = storage_root()?;
            let retention = Duration::from_secs(payload.retention_secs);
            // Two staging areas exist: the legacy `staging/` directory
            // and the streaming backend's reserved `.staging/`
            // (`storage::stream::STAGING_DIR_NAME`), where crashed
            // uploads leave partial objects behind.
            let removed = gc_staging_dir(&root.join("staging"), retention)
                + gc_staging_dir(
                    &root.join(crate::crates::storage::stream::STAGING_DIR_NAME),
                    retention,
                );
            if removed > 0 {
                tracing::info!(removed, "storage.gc_staging deleted abandoned entries");
            }
            Ok(())
        })
    }
}

/// Deletes entries directly under `staging_dir` whose `modified()`
/// time is older than `retention`. Files and directories are both
/// removed; entries whose metadata can't be read are skipped, as are
/// individual delete failures (logged, not fatal — GC is best-effort
/// and the job stays idempotent). A missing `staging_dir` is a no-op.
/// Returns the number of entries removed.
fn gc_staging_dir(staging_dir: &Path, retention: Duration) -> u64 {
    let Ok(entries) = std::fs::read_dir(staging_dir) else {
        return 0;
    };
    let cutoff = SystemTime::now()
        .checked_sub(retention)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut removed = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if modified > cutoff {
            continue; // still within the retention window
        }
        let result = if meta.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(()) => {
                removed += 1;
                tracing::info!(path = %path.display(), "storage.gc_staging removed abandoned entry");
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "storage.gc_staging delete failed");
            }
        }
    }
    removed
}

// ---------------------------------------------------------------------------
// document.metadata_extract / document.index / thumbnail.generate
// ---------------------------------------------------------------------------
//
// Shared machinery for the document-processing job kinds. See the
// module docs for the payload contracts and the processing-status
// scheme (`processing_status` + `{op}_status` metadata fields,
// ProcessingStarted/Completed/Failed audit events).

/// Payload shared by `document.metadata_extract`, `document.index`,
/// and `thumbnail.generate`.
#[derive(Debug)]
struct DocJobPayload {
    document_id: DocumentId,
    /// Optional artifact override; `None` selects the most recently
    /// created artifact.
    artifact_id: Option<String>,
}

impl DocJobPayload {
    fn parse(payload: &serde_json::Value) -> Result<Self, JobError> {
        let document_id = payload["document_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                JobError::Failed("payload.document_id must be a non-empty string".into())
            })?;
        let artifact_id = payload["artifact_id"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        Ok(Self {
            document_id: DocumentId::new(document_id),
            artifact_id,
        })
    }
}

/// Result of a bounded artifact read: at most `max_bytes` of content
/// plus a flag telling whether the object held more.
struct BoundedBytes {
    bytes: Vec<u8>,
    truncated: bool,
}

/// Converts a `local://` storage URI into the confined object name the
/// streaming backend expects. Other schemes are rejected — the
/// document store currently only produces `local://` URIs.
fn object_name_for_uri(uri: &str) -> Result<String, JobError> {
    uri.strip_prefix("local://")
        .map(str::to_string)
        .ok_or_else(|| JobError::Failed(format!("unsupported storage_uri: {uri}")))
}

/// Maps a storage error into a retryable job failure.
fn storage_err(e: StorageError) -> JobError {
    JobError::Failed(format!("storage error: {e}"))
}

/// Reads at most `max_bytes + 1` bytes from `name` so callers can tell
/// "exactly at the cap" apart from "there was more". Synchronous —
/// call inside `spawn_blocking`.
fn read_object_bounded(
    fs: &mut FaultInjectingFs,
    name: &str,
    max_bytes: u64,
) -> Result<BoundedBytes, JobError> {
    let reader = fs.open_reader(name).map_err(storage_err)?;
    let mut limited = reader.take(max_bytes.saturating_add(1));
    let mut buf = Vec::new();
    limited
        .read_to_end(&mut buf)
        .map_err(|e| JobError::Failed(format!("storage read failed for {name}: {e}")))?;
    let truncated = buf.len() as u64 > max_bytes;
    buf.truncate(max_bytes as usize);
    Ok(BoundedBytes {
        bytes: buf,
        truncated,
    })
}

/// [`EngineLimits`] applied to pdf-engine calls made by job handlers:
/// the same 64 MiB input cap as the storage read, plus the default
/// page budget from `security_engine::ResourceLimits`.
fn processing_limits() -> crate::crates::core::engine::EngineLimits {
    crate::crates::core::engine::EngineLimits {
        max_input_bytes: Some(MAX_PROCESSING_BYTES),
        max_pages: Some(10_000),
        ..Default::default()
    }
}

/// Runs a synchronous, potentially CPU-bound closure on the blocking
/// thread pool so lopdf/pdf-engine work never occupies an async
/// executor thread.
async fn run_blocking<T, F>(what: &str, f: F) -> Result<T, JobError>
where
    F: FnOnce() -> Result<T, JobError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| JobError::Failed(format!("{what} blocking task failed: {e}")))?
}

/// Fetches the document's metadata row (or a default one) and applies
/// `f`, then upserts — one read-modify-write per call.
async fn mutate_metadata(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    f: impl FnOnce(&mut DocumentMetadata),
) -> Result<(), JobError> {
    let mut meta = get_document_metadata(pool, document_id)
        .await
        .map_err(store_err)?
        .unwrap_or_else(|| DocumentMetadata {
            document_id: document_id.clone(),
            title: None,
            author: None,
            subject: None,
            language: None,
            keywords: Vec::new(),
            custom_fields: serde_json::Map::new(),
        });
    f(&mut meta);
    upsert_document_metadata(pool, &meta)
        .await
        .map_err(store_err)
}

/// Records a processing audit event; failures propagate so a missing
/// audit row can't silently pass.
async fn audit_event(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    event_type: DocumentAuditEventType,
    details: serde_json::Value,
) -> Result<(), JobError> {
    record_event(pool, document_id, None, event_type, Some(&details))
        .await
        .map_err(store_err)
}

/// Maps an operation-level status to the shared `processing_status`
/// vocabulary. Operation statuses are more specific
/// (`extracted`/`indexed`/`generated`/`empty`); `processing_status`
/// collapses them to `succeeded`/`failed` while passing the
/// already-generic values (`queued`, `running`, `unsupported`,
/// `unavailable`, `skipped_too_large`) through unchanged.
fn normalized_processing_status(status: &str) -> &'static str {
    match status {
        "extracted" | "indexed" | "generated" | "empty" => "succeeded",
        "error" | "failed" => "failed",
        "queued" => "queued",
        "running" => "running",
        "succeeded" => "succeeded",
        "unsupported" => "unsupported",
        "unavailable" => "unavailable",
        "skipped_too_large" => "skipped_too_large",
        _ => "failed",
    }
}

/// Sets `processing_status` and the per-operation `{op}_status`
/// custom field to `status` in one metadata update.
async fn mark_op_status(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    op_status_key: &str,
    status: &str,
) -> Result<(), JobError> {
    let key = op_status_key.to_string();
    let proc_status = normalized_processing_status(status);
    mutate_metadata(pool, document_id, move |m| {
        m.custom_fields
            .insert("processing_status".into(), json!(proc_status));
        m.custom_fields.insert(key, json!(status));
    })
    .await
}

/// Best-effort terminal `failed` status + audit event, used when a
/// handler is about to return `Err` on its final attempt. Errors are
/// swallowed: the job row already records the failure.
async fn mark_failed(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    op_status_key: &str,
    job: &Job,
    error: &JobError,
) {
    let _ = mark_op_status(pool, document_id, op_status_key, "failed").await;
    let _ = audit_event(
        pool,
        document_id,
        DocumentAuditEventType::ProcessingFailed,
        json!({
            "job_id": job.id,
            "job_kind": job.kind,
            "attempts": job.attempts,
            "error": error.to_string(),
        }),
    )
    .await;
}

/// Resolves which artifact a processing job operates on: the explicit
/// `artifact_id` when given (verified to belong to the document), or
/// the most recently created artifact otherwise.
async fn resolve_artifact(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    artifact_id: Option<&str>,
) -> Result<DocumentArtifact, JobError> {
    match artifact_id {
        Some(id) => {
            let artifact = download_artifact(pool, id)
                .await
                .map_err(store_err)?
                .ok_or_else(|| JobError::Failed(format!("artifact not found: {id}")))?;
            if artifact.document_id != *document_id {
                return Err(JobError::Failed(format!(
                    "artifact {id} does not belong to document {document_id}"
                )));
            }
            Ok(artifact)
        }
        None => {
            let artifacts = list_artifacts(pool, document_id).await.map_err(store_err)?;
            artifacts
                .into_iter()
                .last()
                .ok_or_else(|| JobError::Failed(format!("document {document_id} has no artifacts")))
        }
    }
}

/// Verifies the document exists, resolves the target artifact, marks
/// the operation `running`, and emits `ProcessingStarted`. Shared
/// prologue for all three processing handlers.
async fn begin_processing(
    pool: &sqlx::SqlitePool,
    job: &Job,
    op_status_key: &str,
) -> Result<(DocumentId, DocumentArtifact), JobError> {
    let payload = DocJobPayload::parse(&job.payload)?;
    let document_id = payload.document_id;

    get_document(pool, &document_id)
        .await
        .map_err(store_err)?
        .ok_or_else(|| JobError::Failed(format!("document not found: {document_id}")))?;

    let artifact = resolve_artifact(pool, &document_id, payload.artifact_id.as_deref()).await?;

    mark_op_status(pool, &document_id, op_status_key, "running").await?;
    audit_event(
        pool,
        &document_id,
        DocumentAuditEventType::ProcessingStarted,
        json!({
            "job_id": job.id,
            "job_kind": job.kind,
            "artifact_id": artifact.id,
            "storage_uri": artifact.storage_uri,
        }),
    )
    .await?;

    Ok((document_id, artifact))
}

/// Stable lowercase name for a [`SniffedFormat`] (matches its serde
/// representation).
fn sniffed_name(format: SniffedFormat) -> &'static str {
    match format {
        SniffedFormat::Pdf => "pdf",
        SniffedFormat::Png => "png",
        SniffedFormat::Jpeg => "jpeg",
        SniffedFormat::Zip => "zip",
        SniffedFormat::Gif => "gif",
        SniffedFormat::Video { .. } => "video",
        SniffedFormat::Audio { .. } => "audio",
        SniffedFormat::Unknown => "unknown",
    }
}

/// Conventional MIME type for a sniffed format; `None` for unknown.
fn mime_for_format(format: SniffedFormat) -> Option<&'static str> {
    match format {
        SniffedFormat::Pdf => Some("application/pdf"),
        SniffedFormat::Png => Some("image/png"),
        SniffedFormat::Jpeg => Some("image/jpeg"),
        SniffedFormat::Zip => Some("application/zip"),
        SniffedFormat::Gif => Some("image/gif"),
        // `VideoContainer` lives in security_engine's private `sniff`
        // module, so dispatch on its public extension accessor.
        SniffedFormat::Video { container } => Some(match container.sniffed_extension() {
            ".mp4" => "video/mp4",
            ".mkv" => "video/x-matroska",
            ".webm" => "video/webm",
            ".avi" => "video/x-msvideo",
            ".ts" => "video/mp2t",
            _ => "video/mpeg",
        }),
        // Same dispatch as `Video`: `AudioContainer` is reachable
        // through its public extension accessor.
        SniffedFormat::Audio { container } => Some(match container.sniffed_extension() {
            ".mp3" => "audio/mpeg",
            ".wav" => "audio/wav",
            ".flac" => "audio/flac",
            ".ogg" => "audio/ogg",
            ".aac" => "audio/aac",
            ".aiff" => "audio/aiff",
            ".au" => "audio/basic",
            ".amr" => "audio/amr",
            ".caf" => "audio/x-caf",
            ".ac3" => "audio/ac3",
            _ => "audio/mpeg",
        }),
        SniffedFormat::Unknown => None,
    }
}

/// `document.metadata_extract` — sniffs the artifact's real format and,
/// for PDFs, extracts [`PdfInfo`](crate::crates::pdf_engine::PdfInfo)
/// via `pdf_engine` inside `spawn_blocking`.
///
/// Persisted results (document metadata): `detected_format`,
/// `mime_type`, `metadata_extracted_at`, `metadata_extract_status`,
/// plus `page_count`/`pdf_creator`/`pdf_producer`/`pdf_encrypted` and
/// the `title`/`author` columns (filled only when currently unset —
/// extracted values never overwrite curated metadata). Non-PDF and
/// oversized artifacts get an honest `unsupported` /
/// `skipped_too_large` status and the job still succeeds. Re-running
/// overwrites everything, so retries are safe.
pub struct DocumentMetadataExtractHandler {
    storage: StorageConfig,
}

/// Metadata custom-field key holding the extract status.
const META_EXTRACT_STATUS_KEY: &str = "metadata_extract_status";

impl DocumentMetadataExtractHandler {
    /// Builds the handler from the application storage configuration.
    pub fn from_app_config() -> Result<Self, crate::config::ConfigError> {
        Ok(Self::with_storage_config(
            AppConfig::load_lenient()?.to_storage_config(),
        ))
    }

    /// Builds the handler around an explicit storage config (tests).
    pub fn with_storage_config(storage: StorageConfig) -> Self {
        Self { storage }
    }
}

/// Everything the blocking half of `metadata_extract` produces.
struct ExtractOutcome {
    format: SniffedFormat,
    truncated: bool,
    pdf_info: Option<crate::crates::pdf_engine::PdfInfo>,
    pdf_error: Option<String>,
}

impl JobHandler for DocumentMetadataExtractHandler {
    fn handle<'a>(
        &'a self,
        ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let result = self.run(ctx, job).await;
            if let Err(e) = &result
                && job.attempts >= job.max_attempts
                && let Ok(payload) = DocJobPayload::parse(&job.payload)
            {
                mark_failed(
                    &ctx.pool,
                    &payload.document_id,
                    META_EXTRACT_STATUS_KEY,
                    job,
                    e,
                )
                .await;
            }
            result
        })
    }
}

impl DocumentMetadataExtractHandler {
    async fn run(&self, ctx: &JobContext, job: &Job) -> Result<(), JobError> {
        let (document_id, artifact) =
            begin_processing(&ctx.pool, job, META_EXTRACT_STATUS_KEY).await?;

        let object_name = object_name_for_uri(&artifact.storage_uri)?;
        let storage = self.storage.clone();
        let read_name = object_name.clone();
        let outcome = run_blocking("metadata_extract", move || {
            let mut fs = FaultInjectingFs::new(storage)
                .map_err(|e| JobError::Failed(format!("storage init failed: {e}")))?;
            let read = read_object_bounded(&mut fs, &read_name, MAX_PROCESSING_BYTES)?;
            let head = &read.bytes[..read.bytes.len().min(SNIFF_BYTES)];
            let format = sniff_format(head);

            let (pdf_info, pdf_error) = if format == SniffedFormat::Pdf && !read.truncated {
                let engine = crate::crates::pdf_engine::PdfEngine::with_limits(processing_limits());
                match engine.inspect(&read.bytes) {
                    Ok(info) => (Some(info), None),
                    Err(e) => (None, Some(e.to_string())),
                }
            } else {
                (None, None)
            };

            Ok(ExtractOutcome {
                format,
                truncated: read.truncated,
                pdf_info,
                pdf_error,
            })
        })
        .await?;

        // Terminal status: oversized input wins over everything; a PDF
        // parse failure is reported as `error`; unsupported formats
        // (including non-PDF) succeed with `unsupported`.
        let status = if outcome.truncated {
            "skipped_too_large"
        } else if outcome.pdf_error.is_some() {
            "error"
        } else if outcome.pdf_info.is_some() {
            "extracted"
        } else {
            "unsupported"
        };

        let mime_type = mime_for_format(outcome.format)
            .map(str::to_string)
            .or_else(|| artifact.mime_type.clone())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let extracted_at = crate::crates::document_engine::store::now_rfc3339();
        let info = outcome.pdf_info.clone();
        let pdf_error = outcome.pdf_error.clone();
        let format_name = sniffed_name(outcome.format);
        let artifact_id = artifact.id.clone();
        let storage_uri = artifact.storage_uri.clone();
        let artifact_size = artifact.size_bytes;

        mutate_metadata(&ctx.pool, &document_id, move |m| {
            m.custom_fields.insert(
                "processing_status".into(),
                json!(normalized_processing_status(status)),
            );
            m.custom_fields
                .insert(META_EXTRACT_STATUS_KEY.into(), json!(status));
            m.custom_fields
                .insert("detected_format".into(), json!(format_name));
            m.custom_fields.insert("mime_type".into(), json!(mime_type));
            m.custom_fields
                .insert("artifact_id".into(), json!(artifact_id));
            m.custom_fields
                .insert("storage_uri".into(), json!(storage_uri));
            m.custom_fields
                .insert("artifact_size_bytes".into(), json!(artifact_size));
            m.custom_fields
                .insert("metadata_extracted_at".into(), json!(extracted_at));
            if let Some(err) = &pdf_error {
                m.custom_fields.insert("extract_error".into(), json!(err));
            }
            if let Some(info) = &info {
                m.custom_fields
                    .insert("page_count".into(), json!(info.page_count));
                m.custom_fields.insert(
                    "pdf_creator".into(),
                    json!(info.creator.clone().unwrap_or_default()),
                );
                m.custom_fields.insert(
                    "pdf_producer".into(),
                    json!(info.producer.clone().unwrap_or_default()),
                );
                m.custom_fields
                    .insert("pdf_encrypted".into(), json!(info.encrypted));
                // Extracted values fill empty columns only — curated
                // titles/authors are never overwritten.
                if m.title.is_none() {
                    m.title = info.title.clone();
                }
                if m.author.is_none() {
                    m.author = info.author.clone();
                }
                // Keep pdf_title/pdf_author as custom fields too so the
                // raw extracted values stay visible even when the
                // columns were already populated.
                m.custom_fields.insert(
                    "pdf_title".into(),
                    json!(info.title.clone().unwrap_or_default()),
                );
                m.custom_fields.insert(
                    "pdf_author".into(),
                    json!(info.author.clone().unwrap_or_default()),
                );
            }
        })
        .await?;

        audit_event(
            &ctx.pool,
            &document_id,
            DocumentAuditEventType::ProcessingCompleted,
            json!({
                "job_id": job.id,
                "job_kind": job.kind,
                "status": status,
                "format": format_name,
            }),
        )
        .await?;

        Ok(())
    }
}

/// `document.index` — extracts searchable text where feasible and
/// stores a bounded index payload in the `index` metadata custom
/// field.
///
/// Sources: PDFs go through `pdf_engine::extract_text` (all pages,
/// `spawn_blocking`); artifacts whose bytes are valid UTF-8 (or whose
/// MIME type is `text/*`) are indexed directly; everything else gets
/// `index_status: "unsupported"`. Oversized artifacts get
/// `skipped_too_large`. The `index` field holds `{status, format,
/// char_count, word_count, sha256, excerpt}` where `excerpt` is the
/// first [`INDEX_EXCERPT_CHARS`] characters — there is no separate
/// search table by design.
pub struct DocumentIndexHandler {
    storage: StorageConfig,
}

/// Metadata custom-field key holding the index status.
const INDEX_STATUS_KEY: &str = "index_status";

impl DocumentIndexHandler {
    /// Builds the handler from the application storage configuration.
    pub fn from_app_config() -> Result<Self, crate::config::ConfigError> {
        Ok(Self::with_storage_config(
            AppConfig::load_lenient()?.to_storage_config(),
        ))
    }

    /// Builds the handler around an explicit storage config (tests).
    pub fn with_storage_config(storage: StorageConfig) -> Self {
        Self { storage }
    }
}

/// Outcome of the blocking half of `document.index`.
enum IndexOutcome {
    /// Object exceeded [`MAX_PROCESSING_BYTES`].
    TooLarge,
    /// Format has no text-extraction path.
    Unsupported,
    /// Extraction ran but produced no text.
    Empty,
    /// Full extracted text (unbounded by the excerpt cap).
    Text { format: SniffedFormat, text: String },
    /// Permanent extraction failure (e.g. corrupt PDF).
    Failed {
        format: SniffedFormat,
        error: String,
    },
    /// PDF is encrypted — cannot extract without a password.
    Encrypted,
}

impl JobHandler for DocumentIndexHandler {
    fn handle<'a>(
        &'a self,
        ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let result = self.run(ctx, job).await;
            if let Err(e) = &result
                && job.attempts >= job.max_attempts
                && let Ok(payload) = DocJobPayload::parse(&job.payload)
            {
                mark_failed(&ctx.pool, &payload.document_id, INDEX_STATUS_KEY, job, e).await;
            }
            result
        })
    }
}

impl DocumentIndexHandler {
    async fn run(&self, ctx: &JobContext, job: &Job) -> Result<(), JobError> {
        let (document_id, artifact) = begin_processing(&ctx.pool, job, INDEX_STATUS_KEY).await?;

        let object_name = object_name_for_uri(&artifact.storage_uri)?;
        let storage = self.storage.clone();
        let artifact_mime = artifact.mime_type.clone();
        let read_name = object_name.clone();
        let outcome = run_blocking("document.index", move || {
            let mut fs = FaultInjectingFs::new(storage)
                .map_err(|e| JobError::Failed(format!("storage init failed: {e}")))?;
            let read = read_object_bounded(&mut fs, &read_name, MAX_PROCESSING_BYTES)?;
            if read.truncated {
                return Ok(IndexOutcome::TooLarge);
            }
            let head = &read.bytes[..read.bytes.len().min(SNIFF_BYTES)];
            let format = sniff_format(head);

            match format {
                SniffedFormat::Pdf => {
                    let engine =
                        crate::crates::pdf_engine::PdfEngine::with_limits(processing_limits());
                    match engine.extract_text(&read.bytes, None) {
                        Ok(pages) => {
                            let text = pages
                                .iter()
                                .map(|p| p.text.as_str())
                                .collect::<Vec<_>>()
                                .join("\n\n");
                            if text.trim().is_empty() {
                                Ok(IndexOutcome::Empty)
                            } else {
                                Ok(IndexOutcome::Text { format, text })
                            }
                        }
                        Err(crate::crates::pdf_engine::PdfError::Encrypted) => {
                            Ok(IndexOutcome::Encrypted)
                        }
                        Err(e) => Ok(IndexOutcome::Failed {
                            format,
                            error: e.to_string(),
                        }),
                    }
                }
                _ => {
                    // Text path: declared text/* MIME or content that
                    // is valid UTF-8 end-to-end.
                    let declared_text = artifact_mime
                        .as_deref()
                        .is_some_and(|m| m.trim_start().to_ascii_lowercase().starts_with("text/"));
                    match std::str::from_utf8(&read.bytes) {
                        Ok(text) if declared_text || !text.trim().is_empty() => {
                            if text.trim().is_empty() {
                                Ok(IndexOutcome::Empty)
                            } else {
                                Ok(IndexOutcome::Text {
                                    format,
                                    text: text.to_string(),
                                })
                            }
                        }
                        _ => Ok(IndexOutcome::Unsupported),
                    }
                }
            }
        })
        .await?;

        let (status, index_value) = match &outcome {
            IndexOutcome::TooLarge => ("skipped_too_large", None),
            IndexOutcome::Unsupported | IndexOutcome::Encrypted => ("unsupported", None),
            IndexOutcome::Empty => ("empty", None),
            IndexOutcome::Failed { error, format } => {
                let v = serde_json::json!({
                    "status": "error",
                    "format": sniffed_name(*format),
                    "error": error,
                });
                ("error", Some(v))
            }
            IndexOutcome::Text { format, text } => {
                let char_count = text.chars().count() as u64;
                let word_count = text.split_whitespace().count() as u64;
                let sha256 = hex_lower(&Sha256::digest(text.as_bytes()));
                let excerpt: String = text.chars().take(INDEX_EXCERPT_CHARS).collect();
                let value = serde_json::json!({
                    "status": "indexed",
                    "format": sniffed_name(*format),
                    "char_count": char_count,
                    "word_count": word_count,
                    "sha256": sha256,
                    "excerpt": excerpt,
                    "indexed_at": crate::crates::document_engine::store::now_rfc3339(),
                });
                ("indexed", Some(value))
            }
        };

        let artifact_id = artifact.id.clone();
        let format_label = match &outcome {
            IndexOutcome::Text { format, .. } | IndexOutcome::Failed { format, .. } => {
                Some(sniffed_name(*format))
            }
            _ => None,
        };
        let index_error = match &outcome {
            IndexOutcome::Failed { error, .. } => Some(error.clone()),
            IndexOutcome::Encrypted => Some("encrypted PDF".to_string()),
            _ => None,
        };

        mutate_metadata(&ctx.pool, &document_id, move |m| {
            m.custom_fields.insert(
                "processing_status".into(),
                json!(normalized_processing_status(status)),
            );
            m.custom_fields
                .insert(INDEX_STATUS_KEY.into(), json!(status));
            m.custom_fields
                .insert("index_artifact_id".into(), json!(artifact_id));
            if let Some(f) = format_label {
                m.custom_fields.insert("index_format".into(), json!(f));
            }
            if let Some(e) = &index_error {
                m.custom_fields.insert("index_error".into(), json!(e));
            }
            if let Some(v) = index_value {
                m.custom_fields.insert("index".into(), v);
            }
        })
        .await?;

        let event_type = if status == "indexed" {
            DocumentAuditEventType::Indexed
        } else {
            DocumentAuditEventType::ProcessingCompleted
        };
        audit_event(
            &ctx.pool,
            &document_id,
            event_type,
            json!({
                "job_id": job.id,
                "job_kind": job.kind,
                "status": status,
                "format": format_label,
            }),
        )
        .await?;

        Ok(())
    }
}

/// `thumbnail.generate` — extracts an embedded thumbnail/preview image
/// from a PDF (page `/Thumb` entry or an image `/XObject`) and stores
/// it under `documents/{document_id}/thumbnails/` via the streaming
/// upload API.
///
/// Supported encodings: `DCTDecode` (written verbatim as `.jpg`),
/// `JPXDecode` (`.jp2`), and unfiltered/`FlateDecode` `DeviceRGB` or
/// `DeviceGray` 8-bpc rasters (re-encoded as `.png` via the `image`
/// crate). PDFs without an extractable embedded image get
/// `thumbnail_status: "unavailable"`; non-PDFs get `"unsupported"`;
/// oversized inputs get `"skipped_too_large"`. All cases succeed —
/// only genuine I/O or store failures retry.
pub struct ThumbnailGenerateHandler {
    storage: StorageConfig,
}

/// Metadata custom-field key holding the thumbnail status.
const THUMBNAIL_STATUS_KEY: &str = "thumbnail_status";

impl ThumbnailGenerateHandler {
    /// Builds the handler from the application storage configuration.
    pub fn from_app_config() -> Result<Self, crate::config::ConfigError> {
        Ok(Self::with_storage_config(
            AppConfig::load_lenient()?.to_storage_config(),
        ))
    }

    /// Builds the handler around an explicit storage config (tests).
    pub fn with_storage_config(storage: StorageConfig) -> Self {
        Self { storage }
    }
}

/// Color space of a raw thumbnail raster.
#[derive(Clone, Copy)]
enum ThumbColor {
    Rgb,
    Gray,
}

/// A thumbnail image extracted from a PDF, before file encoding.
enum ThumbImage {
    /// Already-encoded image file bytes (JPEG/JP2) plus extension.
    Encoded { bytes: Vec<u8>, ext: &'static str },
    /// Raw samples to be PNG-encoded.
    Samples {
        width: u32,
        height: u32,
        color: ThumbColor,
        data: Vec<u8>,
    },
}

/// A located thumbnail: which page it came from and the image.
struct ThumbCandidate {
    page: u32,
    image: ThumbImage,
}

/// Reads an integer entry from a lopdf dictionary.
fn dict_int(dict: &lopdf::Dictionary, key: &[u8]) -> Option<i64> {
    match dict.get(key).ok()? {
        lopdf::Object::Integer(n) => Some(*n),
        _ => None,
    }
}

/// Dereferences `obj` (following `Reference`s) and returns it as a
/// dictionary when possible.
fn as_dict<'a>(doc: &'a lopdf::Document, obj: &'a lopdf::Object) -> Option<&'a lopdf::Dictionary> {
    match doc.dereference(obj).ok()?.1 {
        lopdf::Object::Dictionary(d) => Some(d),
        _ => None,
    }
}

/// Converts a PDF image [`lopdf::Stream`] into a [`ThumbImage`], or
/// `None` when the encoding/color space is not supported.
fn thumb_from_stream(stream: &lopdf::Stream) -> Option<ThumbImage> {
    // `filters()` errors when /Filter is absent — treat as unfiltered.
    let filters = stream.filters().unwrap_or_default();

    // Already-encoded formats: the stream content *is* the file.
    if filters.len() == 1 && filters[0] == b"DCTDecode" {
        return Some(ThumbImage::Encoded {
            bytes: stream.content.clone(),
            ext: "jpg",
        });
    }
    if filters.len() == 1 && filters[0] == b"JPXDecode" {
        return Some(ThumbImage::Encoded {
            bytes: stream.content.clone(),
            ext: "jp2",
        });
    }
    // Only unfiltered or FlateDecode streams are decodable to raw
    // samples; anything else (CCITT, JBIG2, multi-filter chains, ...)
    // is reported as unavailable rather than approximated.
    if !(filters.is_empty() || filters.iter().all(|f| *f == b"FlateDecode")) {
        return None;
    }

    let dict = &stream.dict;
    let width = u32::try_from(dict_int(dict, b"Width")?).ok()?;
    let height = u32::try_from(dict_int(dict, b"Height")?).ok()?;
    let bpc = dict_int(dict, b"BitsPerComponent")?;
    if bpc != 8 || width == 0 || height == 0 {
        return None;
    }
    if (width as u64) * (height as u64) > MAX_THUMBNAIL_PIXELS {
        return None;
    }

    let color = match dict.get(b"ColorSpace").ok().and_then(|o| o.as_name().ok()) {
        Some(b"DeviceRGB") => ThumbColor::Rgb,
        Some(b"DeviceGray") => ThumbColor::Gray,
        _ => return None,
    };

    let data = stream
        .decompressed_content_with_limit(MAX_THUMBNAIL_DECODED_BYTES)
        .ok()?;
    let expected = match color {
        ThumbColor::Rgb => width as usize * height as usize * 3,
        ThumbColor::Gray => width as usize * height as usize,
    };
    if data.len() != expected {
        return None;
    }
    Some(ThumbImage::Samples {
        width,
        height,
        color,
        data,
    })
}

/// Finds the first usable embedded thumbnail in `doc`: prefers each
/// page's `/Thumb` entry, then falls back to the first `/Subtype
/// /Image` XObject in the page's (inherited) resources.
fn find_pdf_thumbnail(doc: &lopdf::Document) -> Option<ThumbCandidate> {
    for (page_num, page_id) in doc.get_pages() {
        // 1) Explicit /Thumb entry on the page dictionary.
        if let Ok(page) = doc.get_dictionary(page_id)
            && let Ok(thumb) = page.get(b"Thumb")
            && let Ok((_, resolved)) = doc.dereference(thumb)
            && let lopdf::Object::Stream(stream) = resolved
            && let Some(image) = thumb_from_stream(stream)
        {
            return Some(ThumbCandidate {
                page: page_num,
                image,
            });
        }

        // 2) Image XObjects in the page's resource dictionaries.
        if let Ok((direct, referenced)) = doc.get_page_resources(page_id) {
            let mut resource_dicts: Vec<&lopdf::Dictionary> = Vec::new();
            if let Some(d) = direct {
                resource_dicts.push(d);
            }
            for id in referenced {
                if let Ok(d) = doc.get_dictionary(id) {
                    resource_dicts.push(d);
                }
            }
            for resources in resource_dicts {
                let Some(xobjects) = resources.get(b"XObject").ok().and_then(|o| as_dict(doc, o))
                else {
                    continue;
                };
                for (_, xobj) in xobjects.iter() {
                    let Ok((_, resolved)) = doc.dereference(xobj) else {
                        continue;
                    };
                    let lopdf::Object::Stream(stream) = resolved else {
                        continue;
                    };
                    if stream
                        .dict
                        .get(b"Subtype")
                        .ok()
                        .and_then(|o| o.as_name().ok())
                        != Some(b"Image")
                    {
                        continue;
                    }
                    if let Some(image) = thumb_from_stream(stream) {
                        return Some(ThumbCandidate {
                            page: page_num,
                            image,
                        });
                    }
                }
            }
        }
    }
    None
}

/// Encodes a [`ThumbImage`] into a file-ready byte vector. Returns the
/// bytes and the file extension to use.
fn encode_thumbnail(image: &ThumbImage) -> Option<(Vec<u8>, &'static str)> {
    match image {
        ThumbImage::Encoded { bytes, ext } => Some((bytes.clone(), ext)),
        ThumbImage::Samples {
            width,
            height,
            color,
            data,
        } => {
            use image::ImageEncoder as _;
            let mut out = Vec::new();
            let result = match color {
                ThumbColor::Rgb => image::codecs::png::PngEncoder::new(&mut out).write_image(
                    data,
                    *width,
                    *height,
                    image::ExtendedColorType::Rgb8,
                ),
                ThumbColor::Gray => image::codecs::png::PngEncoder::new(&mut out).write_image(
                    data,
                    *width,
                    *height,
                    image::ExtendedColorType::L8,
                ),
            };
            result.ok()?;
            Some((out, "png"))
        }
    }
}

/// Outcome of the blocking half of `thumbnail.generate`.
enum ThumbOutcome {
    /// Object exceeded [`MAX_PROCESSING_BYTES`].
    TooLarge,
    /// Not a PDF — nothing to extract.
    Unsupported,
    /// PDF loaded but has no usable embedded thumbnail.
    Unavailable,
    /// Permanent failure (unparseable PDF, oversized raster, ...).
    Failed { error: String },
    /// Thumbnail located and encoded, ready to upload.
    Ready {
        page: u32,
        bytes: Vec<u8>,
        ext: &'static str,
    },
}

impl JobHandler for ThumbnailGenerateHandler {
    fn handle<'a>(
        &'a self,
        ctx: &'a JobContext,
        job: &'a Job,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), JobError>> + Send + 'a>>
    {
        Box::pin(async move {
            let result = self.run(ctx, job).await;
            if let Err(e) = &result
                && job.attempts >= job.max_attempts
                && let Ok(payload) = DocJobPayload::parse(&job.payload)
            {
                mark_failed(
                    &ctx.pool,
                    &payload.document_id,
                    THUMBNAIL_STATUS_KEY,
                    job,
                    e,
                )
                .await;
            }
            result
        })
    }
}

impl ThumbnailGenerateHandler {
    async fn run(&self, ctx: &JobContext, job: &Job) -> Result<(), JobError> {
        let (document_id, artifact) =
            begin_processing(&ctx.pool, job, THUMBNAIL_STATUS_KEY).await?;

        let object_name = object_name_for_uri(&artifact.storage_uri)?;
        let storage = self.storage.clone();
        let read_name = object_name.clone();
        let outcome = run_blocking("thumbnail.generate", move || {
            let mut fs = FaultInjectingFs::new(storage)
                .map_err(|e| JobError::Failed(format!("storage init failed: {e}")))?;
            let read = read_object_bounded(&mut fs, &read_name, MAX_PROCESSING_BYTES)?;
            if read.truncated {
                return Ok(ThumbOutcome::TooLarge);
            }
            let head = &read.bytes[..read.bytes.len().min(SNIFF_BYTES)];
            if sniff_format(head) != SniffedFormat::Pdf {
                return Ok(ThumbOutcome::Unsupported);
            }

            // lopdf can panic on hostile input — same guard the
            // pdf-engine applies at load time.
            let loaded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                lopdf::Document::load_mem(&read.bytes)
            }));
            let doc = match loaded {
                Ok(Ok(doc)) => doc,
                Ok(Err(e)) => {
                    return Ok(ThumbOutcome::Failed {
                        error: format!("pdf load failed: {e}"),
                    });
                }
                Err(_) => {
                    return Ok(ThumbOutcome::Failed {
                        error: "pdf parser crashed".to_string(),
                    });
                }
            };

            match find_pdf_thumbnail(&doc).and_then(|c| {
                encode_thumbnail(&c.image).map(|(bytes, ext)| ThumbOutcome::Ready {
                    page: c.page,
                    bytes,
                    ext,
                })
            }) {
                Some(ready) => Ok(ready),
                None => Ok(ThumbOutcome::Unavailable),
            }
        })
        .await?;

        match outcome {
            ThumbOutcome::TooLarge | ThumbOutcome::Unsupported | ThumbOutcome::Unavailable => {
                let status = match outcome {
                    ThumbOutcome::TooLarge => "skipped_too_large",
                    ThumbOutcome::Unsupported => "unsupported",
                    _ => "unavailable",
                };
                mutate_metadata(&ctx.pool, &document_id, move |m| {
                    m.custom_fields.insert(
                        "processing_status".into(),
                        json!(normalized_processing_status(status)),
                    );
                    m.custom_fields
                        .insert(THUMBNAIL_STATUS_KEY.into(), json!(status));
                })
                .await?;
                audit_event(
                    &ctx.pool,
                    &document_id,
                    DocumentAuditEventType::ProcessingCompleted,
                    json!({
                        "job_id": job.id,
                        "job_kind": job.kind,
                        "status": status,
                    }),
                )
                .await?;
                Ok(())
            }
            ThumbOutcome::Failed { error } => {
                mutate_metadata(&ctx.pool, &document_id, move |m| {
                    m.custom_fields
                        .insert("processing_status".into(), json!("failed"));
                    m.custom_fields
                        .insert(THUMBNAIL_STATUS_KEY.into(), json!("error"));
                    m.custom_fields
                        .insert("thumbnail_error".into(), json!(error));
                })
                .await?;
                audit_event(
                    &ctx.pool,
                    &document_id,
                    DocumentAuditEventType::ProcessingCompleted,
                    json!({
                        "job_id": job.id,
                        "job_kind": job.kind,
                        "status": "error",
                    }),
                )
                .await?;
                Ok(())
            }
            ThumbOutcome::Ready { page, bytes, ext } => {
                // Deterministic object name → replays rewrite the same
                // thumbnail instead of accumulating versions.
                let thumb_name = format!(
                    "documents/{}/thumbnails/{}-p{}.{}",
                    document_id, artifact.id, page, ext
                );
                let storage_uri = format!("local://{thumb_name}");

                // Upload inside the blocking pool too: storage I/O is
                // synchronous and chunking keeps memory bounded.
                let storage = self.storage.clone();
                let upload_name = thumb_name.clone();
                let receipt = run_blocking("thumbnail upload", move || {
                    let mut fs = FaultInjectingFs::new(storage)
                        .map_err(|e| JobError::Failed(format!("storage init failed: {e}")))?;
                    let mut upload = fs.begin_upload(&upload_name).map_err(storage_err)?;
                    for chunk in bytes.chunks(UPLOAD_CHUNK_BYTES) {
                        upload.write_chunk(chunk).map_err(storage_err)?;
                    }
                    upload.commit().map_err(storage_err)
                })
                .await?;

                let sha = receipt.sha256.clone();
                let nbytes = receipt.bytes;
                let artifact_id = artifact.id.clone();
                mutate_metadata(&ctx.pool, &document_id, move |m| {
                    m.custom_fields
                        .insert("processing_status".into(), json!("succeeded"));
                    m.custom_fields
                        .insert(THUMBNAIL_STATUS_KEY.into(), json!("generated"));
                    m.custom_fields
                        .insert("thumbnail_uri".into(), json!(storage_uri));
                    m.custom_fields
                        .insert("thumbnail_sha256".into(), json!(sha));
                    m.custom_fields
                        .insert("thumbnail_bytes".into(), json!(nbytes));
                    m.custom_fields.insert("thumbnail_page".into(), json!(page));
                    m.custom_fields
                        .insert("thumbnail_format".into(), json!(ext));
                    m.custom_fields
                        .insert("thumbnail_artifact_id".into(), json!(artifact_id));
                })
                .await?;

                audit_event(
                    &ctx.pool,
                    &document_id,
                    DocumentAuditEventType::ThumbnailGenerated,
                    json!({
                        "job_id": job.id,
                        "job_kind": job.kind,
                        "page": page,
                        "format": ext,
                        "storage_uri": format!("local://{thumb_name}"),
                        "bytes": nbytes,
                        "sha256": receipt.sha256,
                    }),
                )
                .await?;
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Enqueue helpers — producers should use these instead of hand-crafting
// job payloads. Each returns the enqueued [`Job`] and best-effort stamps
// `processing_status = "queued"` on the document's metadata.
// ---------------------------------------------------------------------------

/// Builds the shared document-processing payload.
fn doc_job_payload(document_id: &DocumentId, artifact_id: Option<&str>) -> serde_json::Value {
    let mut p = serde_json::json!({ "document_id": document_id.as_str() });
    if let Some(id) = artifact_id {
        p["artifact_id"] = json!(id);
    }
    p
}

/// Enqueues a `document.ingest` job for `bytes`.
///
/// Payload contract: `{document_id, content_b64, file_name?,
/// mime_type?}` — see [`DocumentIngestHandler`]. `file_name` defaults
/// to `"upload.bin"`.
pub async fn enqueue_ingest(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    file_name: Option<&str>,
    mime_type: Option<&str>,
    bytes: &[u8],
) -> Result<Job, JobError> {
    let mut payload = serde_json::json!({
        "document_id": document_id.as_str(),
        "content_b64": B64.encode(bytes),
    });
    if let Some(name) = file_name {
        payload["file_name"] = json!(name);
    }
    if let Some(mime) = mime_type {
        payload["mime_type"] = json!(mime);
    }
    enqueue(
        pool,
        &NewJob {
            kind: DOCUMENT_INGEST_KIND.to_string(),
            payload,
            priority: 0,
            run_at: None,
            max_attempts: None,
        },
        DEFAULT_MAX_ATTEMPTS,
    )
    .await
}

/// Enqueues a `document.metadata_extract` job. Payload:
/// `{document_id, artifact_id?}` — see [`DocumentMetadataExtractHandler`].
pub async fn enqueue_metadata_extract(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    artifact_id: Option<&str>,
) -> Result<Job, JobError> {
    let job = enqueue(
        pool,
        &NewJob {
            kind: DOCUMENT_METADATA_EXTRACT_KIND.to_string(),
            payload: doc_job_payload(document_id, artifact_id),
            priority: 0,
            run_at: None,
            max_attempts: None,
        },
        DEFAULT_MAX_ATTEMPTS,
    )
    .await?;
    // Best-effort: the job itself is the source of truth; a missing
    // document simply means the queued stamp has nowhere to go.
    let _ = mark_op_status(pool, document_id, META_EXTRACT_STATUS_KEY, "queued").await;
    Ok(job)
}

/// Enqueues a `document.index` job. Payload: `{document_id,
/// artifact_id?}` — see [`DocumentIndexHandler`].
pub async fn enqueue_index(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    artifact_id: Option<&str>,
) -> Result<Job, JobError> {
    let job = enqueue(
        pool,
        &NewJob {
            kind: DOCUMENT_INDEX_KIND.to_string(),
            payload: doc_job_payload(document_id, artifact_id),
            priority: 0,
            run_at: None,
            max_attempts: None,
        },
        DEFAULT_MAX_ATTEMPTS,
    )
    .await?;
    let _ = mark_op_status(pool, document_id, INDEX_STATUS_KEY, "queued").await;
    Ok(job)
}

/// Enqueues a `thumbnail.generate` job. Payload: `{document_id,
/// artifact_id?}` — see [`ThumbnailGenerateHandler`].
pub async fn enqueue_thumbnail(
    pool: &sqlx::SqlitePool,
    document_id: &DocumentId,
    artifact_id: Option<&str>,
) -> Result<Job, JobError> {
    let job = enqueue(
        pool,
        &NewJob {
            kind: THUMBNAIL_GENERATE_KIND.to_string(),
            payload: doc_job_payload(document_id, artifact_id),
            priority: 0,
            run_at: None,
            max_attempts: None,
        },
        DEFAULT_MAX_ATTEMPTS,
    )
    .await?;
    let _ = mark_op_status(pool, document_id, THUMBNAIL_STATUS_KEY, "queued").await;
    Ok(job)
}

// ---------------------------------------------------------------------------
// video.* enqueue helpers
// ---------------------------------------------------------------------------
//
// Unlike the document handlers there is no document-level status
// tracking for video jobs — they operate on storage-root paths, not
// document rows — so these helpers just enqueue with the documented
// payload contract.

/// Enqueues a `video.probe` job. Payload: `{path, max_bytes?}` — see
/// [`VideoProbeHandler`].
pub async fn enqueue_video_probe(
    pool: &sqlx::SqlitePool,
    path: &str,
    max_bytes: Option<u64>,
) -> Result<Job, JobError> {
    enqueue_video_job(
        pool,
        VIDEO_PROBE_KIND,
        video_path_payload(path, max_bytes, &[]),
    )
    .await
}

/// Enqueues a `video.thumbnail` job. Payload: `{path, at_secs?,
/// max_bytes?}` — see [`VideoThumbnailHandler`].
pub async fn enqueue_video_thumbnail(
    pool: &sqlx::SqlitePool,
    path: &str,
    at_secs: Option<f64>,
    max_bytes: Option<u64>,
) -> Result<Job, JobError> {
    let mut extra = Vec::new();
    if let Some(secs) = at_secs {
        extra.push(("at_secs", json!(secs)));
    }
    enqueue_video_job(
        pool,
        VIDEO_THUMBNAIL_KIND,
        video_path_payload(path, max_bytes, &extra),
    )
    .await
}

/// Enqueues a `video.transcode` job. Payload: `{path, target_ext,
/// max_bytes?}` — see [`VideoTranscodeHandler`].
pub async fn enqueue_video_transcode(
    pool: &sqlx::SqlitePool,
    path: &str,
    target_ext: &str,
    max_bytes: Option<u64>,
) -> Result<Job, JobError> {
    let mut payload = video_path_payload(path, max_bytes, &[]);
    payload["target_ext"] = json!(target_ext);
    enqueue_video_job(pool, VIDEO_TRANSCODE_KIND, payload).await
}

/// Enqueues a `video.trim` job. Payload: `{path, start_secs,
/// end_secs?, reencode?, max_bytes?}` — see [`VideoTrimHandler`].
pub async fn enqueue_video_trim(
    pool: &sqlx::SqlitePool,
    path: &str,
    start_secs: f64,
    end_secs: Option<f64>,
    reencode: bool,
    max_bytes: Option<u64>,
) -> Result<Job, JobError> {
    let mut extra = vec![
        ("start_secs", json!(start_secs)),
        ("reencode", json!(reencode)),
    ];
    if let Some(end) = end_secs {
        extra.push(("end_secs", json!(end)));
    }
    enqueue_video_job(
        pool,
        VIDEO_TRIM_KIND,
        video_path_payload(path, max_bytes, &extra),
    )
    .await
}

/// Enqueues a `video.remux` job. Payload: `{path, target_ext,
/// max_bytes?}` — see [`VideoRemuxHandler`].
pub async fn enqueue_video_remux(
    pool: &sqlx::SqlitePool,
    path: &str,
    target_ext: &str,
    max_bytes: Option<u64>,
) -> Result<Job, JobError> {
    let mut payload = video_path_payload(path, max_bytes, &[]);
    payload["target_ext"] = json!(target_ext);
    enqueue_video_job(pool, VIDEO_REMUX_KIND, payload).await
}

/// Enqueues a `video.extract_frames` job. Payload: `{path,
/// every_secs?, fps?, all?, keyframes_only?, format?, max_bytes?}` —
/// see [`VideoExtractFramesHandler`]. `None`/`false` options are
/// omitted from the payload.
pub async fn enqueue_video_extract_frames(
    pool: &sqlx::SqlitePool,
    path: &str,
    every_secs: Option<f64>,
    fps: Option<f64>,
    all: bool,
    keyframes_only: bool,
    format: Option<&str>,
    max_bytes: Option<u64>,
) -> Result<Job, JobError> {
    let mut extra = vec![
        ("all", json!(all)),
        ("keyframes_only", json!(keyframes_only)),
    ];
    if let Some(v) = every_secs {
        extra.push(("every_secs", json!(v)));
    }
    if let Some(v) = fps {
        extra.push(("fps", json!(v)));
    }
    if let Some(v) = format {
        extra.push(("format", json!(v)));
    }
    enqueue_video_job(
        pool,
        VIDEO_EXTRACT_FRAMES_KIND,
        video_path_payload(path, max_bytes, &extra),
    )
    .await
}

/// Enqueues a `video.subtitles_extract` job. Payload: `{path, track?,
/// format?, max_bytes?}` — see [`VideoSubtitlesExtractHandler`].
pub async fn enqueue_video_subtitles_extract(
    pool: &sqlx::SqlitePool,
    path: &str,
    track: Option<u32>,
    format: Option<&str>,
    max_bytes: Option<u64>,
) -> Result<Job, JobError> {
    let mut extra = Vec::new();
    if let Some(t) = track {
        extra.push(("track", json!(t)));
    }
    if let Some(f) = format {
        extra.push(("format", json!(f)));
    }
    enqueue_video_job(
        pool,
        VIDEO_SUBTITLES_EXTRACT_KIND,
        video_path_payload(path, max_bytes, &extra),
    )
    .await
}

/// Enqueues a `video.analyze` job. Payload: `{path, max_bytes?}` —
/// see [`VideoAnalyzeHandler`].
pub async fn enqueue_video_analyze(
    pool: &sqlx::SqlitePool,
    path: &str,
    max_bytes: Option<u64>,
) -> Result<Job, JobError> {
    enqueue_video_job(
        pool,
        VIDEO_ANALYZE_KIND,
        video_path_payload(path, max_bytes, &[]),
    )
    .await
}

/// Enqueues a `video.package_hls` job. Payload: `{path, max_bytes?}`
/// — see [`VideoPackageHlsHandler`].
pub async fn enqueue_video_package_hls(
    pool: &sqlx::SqlitePool,
    path: &str,
    max_bytes: Option<u64>,
) -> Result<Job, JobError> {
    enqueue_video_job(
        pool,
        VIDEO_PACKAGE_HLS_KIND,
        video_path_payload(path, max_bytes, &[]),
    )
    .await
}

/// Enqueues a `video.metadata_set` job. Payload: `{path, tags,
/// max_bytes?}` — see [`VideoMetadataSetHandler`].
pub async fn enqueue_video_metadata_set(
    pool: &sqlx::SqlitePool,
    path: &str,
    tags: &BTreeMap<String, String>,
    max_bytes: Option<u64>,
) -> Result<Job, JobError> {
    let mut payload = video_path_payload(path, max_bytes, &[]);
    payload["tags"] = json!(tags);
    enqueue_video_job(pool, VIDEO_METADATA_SET_KIND, payload).await
}

/// Enqueues a `video.metadata_strip` job. Payload: `{path,
/// max_bytes?}` — see [`VideoMetadataStripHandler`].
pub async fn enqueue_video_metadata_strip(
    pool: &sqlx::SqlitePool,
    path: &str,
    max_bytes: Option<u64>,
) -> Result<Job, JobError> {
    enqueue_video_job(
        pool,
        VIDEO_METADATA_STRIP_KIND,
        video_path_payload(path, max_bytes, &[]),
    )
    .await
}

/// Enqueues a `video.extract_streams` job. Payload: `{path, subtitles?,
/// attachments?, cover_art?, chapters?, data?, subtitle_format?,
/// max_bytes?}` — see [`VideoExtractStreamsHandler`]. `false` flags
/// are omitted from the payload.
pub async fn enqueue_video_extract_streams(
    pool: &sqlx::SqlitePool,
    path: &str,
    subtitles: bool,
    attachments: bool,
    cover_art: bool,
    chapters: bool,
    data: bool,
    subtitle_format: Option<&str>,
    max_bytes: Option<u64>,
) -> Result<Job, JobError> {
    let mut extra = Vec::new();
    for (key, val) in [
        ("subtitles", subtitles),
        ("attachments", attachments),
        ("cover_art", cover_art),
        ("chapters", chapters),
        ("data", data),
    ] {
        if val {
            extra.push((key, json!(true)));
        }
    }
    if let Some(f) = subtitle_format {
        extra.push(("subtitle_format", json!(f)));
    }
    enqueue_video_job(
        pool,
        VIDEO_EXTRACT_STREAMS_KIND,
        video_path_payload(path, max_bytes, &extra),
    )
    .await
}

/// Enqueues a `video.audio_volume` job. Payload: `{path, gain_db,
/// max_bytes?}` — see [`VideoAudioVolumeHandler`].
pub async fn enqueue_video_audio_volume(
    pool: &sqlx::SqlitePool,
    path: &str,
    gain_db: f64,
    max_bytes: Option<u64>,
) -> Result<Job, JobError> {
    let payload = video_path_payload(path, max_bytes, &[("gain_db", json!(gain_db))]);
    enqueue_video_job(pool, VIDEO_AUDIO_VOLUME_KIND, payload).await
}

/// Enqueues a `video.contact_sheet` job. Payload: `{path, cols?,
/// rows?, thumb_width?, interval_secs?, max_bytes?}` — see
/// [`VideoContactSheetHandler`].
pub async fn enqueue_video_contact_sheet(
    pool: &sqlx::SqlitePool,
    path: &str,
    cols: Option<u32>,
    rows: Option<u32>,
    thumb_width: Option<u32>,
    interval_secs: Option<f64>,
    max_bytes: Option<u64>,
) -> Result<Job, JobError> {
    let mut extra = Vec::new();
    if let Some(v) = cols {
        extra.push(("cols", json!(v)));
    }
    if let Some(v) = rows {
        extra.push(("rows", json!(v)));
    }
    if let Some(v) = thumb_width {
        extra.push(("thumb_width", json!(v)));
    }
    if let Some(v) = interval_secs {
        extra.push(("interval_secs", json!(v)));
    }
    enqueue_video_job(
        pool,
        VIDEO_CONTACT_SHEET_KIND,
        video_path_payload(path, max_bytes, &extra),
    )
    .await
}

/// Builds the shared `{path, max_bytes?}` portion of a `video.*`
/// payload plus any operation-specific extra fields.
fn video_path_payload(
    path: &str,
    max_bytes: Option<u64>,
    extra: &[(&str, serde_json::Value)],
) -> serde_json::Value {
    let mut payload = serde_json::json!({ "path": path });
    if let Some(max) = max_bytes {
        payload["max_bytes"] = json!(max);
    }
    for (key, value) in extra {
        payload[*key] = value.clone();
    }
    payload
}

/// Enqueues a `video.*` job with the standard retry policy.
async fn enqueue_video_job(
    pool: &sqlx::SqlitePool,
    kind: &str,
    payload: serde_json::Value,
) -> Result<Job, JobError> {
    enqueue(
        pool,
        &NewJob {
            kind: kind.to_string(),
            payload,
            priority: 0,
            run_at: None,
            max_attempts: None,
        },
        DEFAULT_MAX_ATTEMPTS,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::document_engine::document::{DocumentOrigin, DocumentSource, DocumentType};
    use crate::crates::document_engine::store::artifacts::list_artifacts;
    use crate::crates::document_engine::store::documents::{NewDocument, create_document};
    use crate::crates::document_engine::store::history::get_document_history;
    use crate::crates::document_engine::store::schema::init_document_store;
    use crate::crates::job_engine::create_jobs_table;
    use crate::crates::job_engine::{JobEngineConfig, JobStatus, NewJob, list_jobs};
    use base64::engine::general_purpose::STANDARD as B64;
    use sqlx::SqlitePool;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        create_jobs_table(&pool).await.unwrap();
        init_document_store(&pool).await.unwrap();
        pool
    }

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kaladoc-ingest-test-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn engine(pool: SqlitePool, root: &std::path::Path) -> JobEngine {
        let mut eng = JobEngine::new(
            pool,
            JobEngineConfig {
                poll_interval: Duration::from_millis(10),
                default_max_attempts: 3,
                backoff_base: Duration::from_secs(0),
                stale_after: Duration::from_secs(60),
            },
        );
        eng.register(
            DOCUMENT_INGEST_KIND,
            Arc::new(DocumentIngestHandler::with_storage_config(
                StorageConfig::new(root),
            )),
        );
        eng
    }

    async fn make_document(pool: &SqlitePool) -> DocumentId {
        let doc = create_document(
            pool,
            &NewDocument {
                name: "report".into(),
                document_type: DocumentType::Pdf,
                owner_id: "user-1".into(),
                source: DocumentSource::new(DocumentOrigin::Upload),
            },
        )
        .await
        .unwrap();
        doc.id
    }

    fn ingest_payload(document_id: &DocumentId, bytes: &[u8]) -> serde_json::Value {
        serde_json::json!({
            "document_id": document_id.as_str(),
            "file_name": "report.pdf",
            "mime_type": "application/pdf",
            "content_b64": B64.encode(bytes),
        })
    }

    #[tokio::test]
    async fn ingest_writes_bytes_and_artifact() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = engine(pool.clone(), &root);

        let bytes = b"%PDF-1.4 fake pdf bytes";
        eng.enqueue(&NewJob {
            kind: DOCUMENT_INGEST_KIND.into(),
            payload: ingest_payload(&doc_id, bytes),
            priority: 0,
            run_at: None,
            max_attempts: None,
        })
        .await
        .unwrap();
        assert!(eng.run_once().await.unwrap());

        // Bytes landed under the deterministic object name.
        let on_disk = root
            .join("documents")
            .join(doc_id.as_str())
            .join("report.pdf");
        assert_eq!(std::fs::read(&on_disk).unwrap(), bytes);

        // Artifact row recorded with checksum + storage_uri.
        let arts = list_artifacts(&pool, &doc_id).await.unwrap();
        assert_eq!(arts.len(), 1);
        assert_eq!(
            arts[0].storage_uri,
            format!("local://documents/{}/report.pdf", doc_id)
        );
        assert_eq!(arts[0].size_bytes, bytes.len() as u64);
        let checksum = arts[0].checksum.as_ref().unwrap();
        assert_eq!(checksum.algorithm, ChecksumAlgorithm::Sha256);
        assert_eq!(checksum.value.len(), 64);

        // Audit trail got an `imported` event.
        let history = get_document_history(&pool, &doc_id).await.unwrap();
        assert!(
            history
                .iter()
                .any(|e| e.event_type == DocumentAuditEventType::Imported)
        );

        // Job marked succeeded.
        let jobs = list_jobs(&pool, Some(JobStatus::Succeeded), 10)
            .await
            .unwrap();
        assert_eq!(jobs.len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn ingest_is_idempotent_across_retries() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = engine(pool.clone(), &root);
        let payload = ingest_payload(&doc_id, b"same bytes");

        // Two jobs with identical payloads — e.g. a producer retry —
        // must not create duplicate artifact rows.
        for _ in 0..2 {
            eng.enqueue(&NewJob {
                kind: DOCUMENT_INGEST_KIND.into(),
                payload: payload.clone(),
                priority: 0,
                run_at: None,
                max_attempts: None,
            })
            .await
            .unwrap();
            assert!(eng.run_once().await.unwrap());
        }
        assert_eq!(list_artifacts(&pool, &doc_id).await.unwrap().len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn ingest_rejects_missing_document() {
        let pool = test_pool().await;
        let root = temp_root();
        let eng = engine(pool.clone(), &root);
        let ghost = DocumentId::new("doc-does-not-exist");

        eng.enqueue(&NewJob {
            kind: DOCUMENT_INGEST_KIND.into(),
            payload: ingest_payload(&ghost, b"x"),
            priority: 0,
            run_at: None,
            max_attempts: Some(1),
        })
        .await
        .unwrap();
        assert!(eng.run_once().await.unwrap());

        // max_attempts=1 → straight to failed with the not-found message.
        let jobs = list_jobs(&pool, Some(JobStatus::Failed), 10).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(
            jobs[0]
                .last_error
                .as_deref()
                .unwrap_or("")
                .contains("document not found")
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn ingest_rejects_bad_payload() {
        let pool = test_pool().await;
        let root = temp_root();
        let eng = engine(pool.clone(), &root);

        // Neither content field present.
        eng.enqueue(&NewJob {
            kind: DOCUMENT_INGEST_KIND.into(),
            payload: serde_json::json!({ "document_id": "doc-x" }),
            priority: 0,
            run_at: None,
            max_attempts: Some(1),
        })
        .await
        .unwrap();
        // Unsafe file name.
        let doc_id = make_document(&pool).await;
        eng.enqueue(&NewJob {
            kind: DOCUMENT_INGEST_KIND.into(),
            payload: serde_json::json!({
                "document_id": doc_id.as_str(),
                "content": "hi",
                "file_name": "../escape.txt",
            }),
            priority: 0,
            run_at: None,
            max_attempts: Some(1),
        })
        .await
        .unwrap();

        assert!(eng.run_once().await.unwrap());
        assert!(eng.run_once().await.unwrap());
        let failed = list_jobs(&pool, Some(JobStatus::Failed), 10).await.unwrap();
        assert_eq!(failed.len(), 2);
        std::fs::remove_dir_all(&root).ok();
    }

    // -----------------------------------------------------------------
    // register_builtin / system.ping / storage.gc_staging
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn register_builtin_handles_system_ping() {
        let pool = test_pool().await;
        let mut eng = JobEngine::new(
            pool.clone(),
            JobEngineConfig {
                poll_interval: Duration::from_millis(10),
                default_max_attempts: 3,
                backoff_base: Duration::from_secs(0),
                stale_after: Duration::from_secs(60),
            },
        );
        register_builtin(&mut eng);

        eng.enqueue(&NewJob {
            kind: SYSTEM_PING_KIND.into(),
            payload: serde_json::json!({}),
            priority: 0,
            run_at: None,
            max_attempts: None,
        })
        .await
        .unwrap();
        assert!(eng.run_once().await.unwrap());

        let jobs = list_jobs(&pool, Some(JobStatus::Succeeded), 10)
            .await
            .unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].kind, SYSTEM_PING_KIND);
    }

    #[test]
    fn resolve_under_root_rejects_escapes() {
        let root = PathBuf::from("/storage");
        assert!(resolve_under_root(&root, "docs/a.pdf").is_ok());
        assert!(resolve_under_root(&root, "./docs/a.pdf").is_ok());
        for bad in [
            "",
            "   ",
            "/etc/passwd",
            "../escape.pdf",
            "docs/../../escape.pdf",
            "docs/../a.pdf",
        ] {
            assert!(
                resolve_under_root(&root, bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    // -----------------------------------------------------------------
    // video.* handlers
    // -----------------------------------------------------------------

    #[test]
    fn video_payloads_parse_with_defaults() {
        let p: VideoPathPayload =
            serde_json::from_value(json!({"path": "media/clip.mp4"})).unwrap();
        assert_eq!(p.path, "media/clip.mp4");
        assert_eq!(p.max_bytes, DEFAULT_VIDEO_MAX_BYTES);

        let p: VideoThumbnailPayload =
            serde_json::from_value(json!({"path": "clip.mp4", "at_secs": 2.5})).unwrap();
        assert_eq!(p.at_secs, Some(2.5));
        assert_eq!(p.max_bytes, DEFAULT_VIDEO_MAX_BYTES);

        let p: VideoTranscodePayload =
            serde_json::from_value(json!({"path": "clip.mp4", "target_ext": "webm"})).unwrap();
        assert_eq!(p.target_ext, "webm");

        let p: VideoTrimPayload = serde_json::from_value(
            json!({"path": "clip.mp4", "start_secs": 1.0, "end_secs": 3.0, "reencode": true}),
        )
        .unwrap();
        assert_eq!(p.start_secs, 1.0);
        assert_eq!(p.end_secs, Some(3.0));
        assert!(p.reencode);

        // Required fields are enforced.
        assert!(serde_json::from_value::<VideoTrimPayload>(json!({"path": "clip.mp4"})).is_err());
        assert!(
            serde_json::from_value::<VideoTranscodePayload>(json!({"path": "clip.mp4"})).is_err()
        );
    }

    #[test]
    fn video_handlers_reject_path_escapes() {
        let root = PathBuf::from("/storage");
        for bad in ["", "../clip.mp4", "/abs/clip.mp4", "a/../../clip.mp4"] {
            assert!(
                probe_video_at(&root, bad, DEFAULT_VIDEO_MAX_BYTES).is_err(),
                "probe should reject {bad:?}"
            );
            assert!(
                thumbnail_video_at(
                    &root,
                    &VideoThumbnailPayload {
                        path: bad.into(),
                        at_secs: None,
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "thumbnail should reject {bad:?}"
            );
            assert!(
                transcode_video_at(
                    &root,
                    &VideoTranscodePayload {
                        path: bad.into(),
                        target_ext: "webm".into(),
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "transcode should reject {bad:?}"
            );
            assert!(
                trim_video_at(
                    &root,
                    &VideoTrimPayload {
                        path: bad.into(),
                        start_secs: 0.0,
                        end_secs: None,
                        reencode: false,
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "trim should reject {bad:?}"
            );
        }
    }

    #[test]
    fn video_transcode_rejects_unsafe_target_ext() {
        let root = PathBuf::from("/storage");
        for bad in ["", "  ", "../x", "a/b", "a.b", "mp4;rm", "-"] {
            let r = transcode_video_at(
                &root,
                &VideoTranscodePayload {
                    path: "clip.mp4".into(),
                    target_ext: bad.into(),
                    max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                },
            );
            // Unsafe ext fails at sanitize (before any file access);
            // "-" is alphanumeric-safe? No — '-' is not alphanumeric.
            assert!(r.is_err(), "target_ext {bad:?} should be rejected");
        }
        assert_eq!(sanitize_target_ext(" WebM ").unwrap(), "webm");
        assert_eq!(sanitize_target_ext(".MP4").unwrap(), "mp4");
    }

    #[test]
    fn video_probe_writes_report_when_ffmpeg_available() {
        use std::process::Command;
        // Skip when the ffmpeg toolchain isn't installed — same probe
        // style other engine tests use.
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            return;
        }
        let root = temp_root();
        let src = root.join("clip.mp4");
        let generated = Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:d=1:r=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest",
            ])
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !generated || !src.is_file() {
            std::fs::remove_dir_all(&root).ok();
            return; // encoder unavailable (e.g. no libx264) — skip
        }

        probe_video_at(&root, "clip.mp4", DEFAULT_VIDEO_MAX_BYTES).unwrap();
        let report = root.join("clip.mp4.probe.json");
        assert!(report.is_file(), "probe report should be written");
        let parsed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
        assert!(parsed.is_object());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn video_extended_payloads_parse_with_defaults() {
        let p: VideoExtractFramesPayload =
            serde_json::from_value(json!({"path": "clip.mp4", "every_secs": 2.0})).unwrap();
        assert_eq!(p.path, "clip.mp4");
        assert_eq!(p.every_secs, Some(2.0));
        assert_eq!(p.fps, None);
        assert!(!p.all);
        assert!(!p.keyframes_only);
        assert_eq!(p.format, None);
        assert_eq!(p.max_bytes, DEFAULT_VIDEO_MAX_BYTES);

        let p: VideoSubtitlesExtractPayload =
            serde_json::from_value(json!({"path": "clip.mp4", "track": 1})).unwrap();
        assert_eq!(p.track, Some(1));
        assert_eq!(p.format, None);

        let p: VideoMetadataSetPayload = serde_json::from_value(
            json!({"path": "clip.mp4", "tags": {"title": "Demo", "artist": "Me"}}),
        )
        .unwrap();
        assert_eq!(p.tags["title"], "Demo");
        assert_eq!(p.tags.len(), 2);
        assert!(
            serde_json::from_value::<VideoMetadataSetPayload>(json!({"path": "clip.mp4"})).is_err(),
            "tags is required"
        );

        let p: VideoExtractStreamsPayload = serde_json::from_value(
            json!({"path": "clip.mp4", "subtitles": true, "chapters": true}),
        )
        .unwrap();
        assert!(p.subtitles);
        assert!(p.chapters);
        assert!(!p.attachments);
        assert!(!p.cover_art);
        assert!(!p.data);
        assert_eq!(p.subtitle_format, None);

        let p: VideoAudioVolumePayload =
            serde_json::from_value(json!({"path": "clip.mp4", "gain_db": -3.5})).unwrap();
        assert_eq!(p.gain_db, -3.5);

        let p: VideoContactSheetPayload =
            serde_json::from_value(json!({"path": "clip.mp4", "cols": 3})).unwrap();
        assert_eq!(p.cols, Some(3));
        assert_eq!(p.rows, None);
        assert_eq!(p.thumb_width, None);
        assert_eq!(p.interval_secs, None);

        let p: VideoSpeedPayload =
            serde_json::from_value(json!({"path": "clip.mp4", "factor": 1.5})).unwrap();
        assert_eq!(p.factor, 1.5);
        assert!(
            serde_json::from_value::<VideoSpeedPayload>(json!({"path": "clip.mp4"})).is_err(),
            "factor is required"
        );

        let p: VideoLoopPayload =
            serde_json::from_value(json!({"path": "clip.mp4", "count": 2})).unwrap();
        assert_eq!(p.count, 2);

        let p: VideoFreezePayload = serde_json::from_value(
            json!({"path": "clip.mp4", "at_secs": 1.0, "duration_secs": 0.5}),
        )
        .unwrap();
        assert_eq!(p.at_secs, 1.0);
        assert_eq!(p.duration_secs, 0.5);

        let p: VideoResizePayload = serde_json::from_value(
            json!({"path": "clip.mp4", "width": 640, "height": 360, "fit": "cover"}),
        )
        .unwrap();
        assert_eq!(p.width, 640);
        assert_eq!(p.fit.as_deref(), Some("cover"));

        let p: VideoCropPayload =
            serde_json::from_value(json!({"path": "clip.mp4", "x": 0, "y": 0, "w": 100, "h": 50}))
                .unwrap();
        assert_eq!((p.x, p.y, p.w, p.h), (0, 0, 100, 50));

        let p: VideoRotatePayload =
            serde_json::from_value(json!({"path": "clip.mp4", "degrees": 90.0})).unwrap();
        assert_eq!(p.degrees, 90.0);

        let p: VideoFlipPayload = serde_json::from_value(json!({"path": "clip.mp4"})).unwrap();
        assert_eq!(p.horizontal, None);
        let p: VideoFlipPayload =
            serde_json::from_value(json!({"path": "clip.mp4", "horizontal": false})).unwrap();
        assert_eq!(p.horizontal, Some(false));

        let p: VideoGifPayload =
            serde_json::from_value(json!({"path": "clip.mp4", "fps": 12.0, "width": 320})).unwrap();
        assert_eq!(p.fps, Some(12.0));
        assert_eq!(p.width, Some(320));

        let p: VideoSlideshowPayload = serde_json::from_value(
            json!({"paths": ["a.png", "b.png"], "fps": 2.0, "target_ext": "mp4"}),
        )
        .unwrap();
        assert_eq!(p.paths, vec!["a.png".to_string(), "b.png".to_string()]);
        assert_eq!(p.fps, Some(2.0));
        assert_eq!(p.duration_per_image, None);
        assert_eq!(p.target_ext, "mp4");
        assert!(
            serde_json::from_value::<VideoSlideshowPayload>(
                json!({"paths": ["a.png"], "fps": 2.0})
            )
            .is_err(),
            "target_ext is required"
        );
        assert!(
            serde_json::from_value::<VideoSlideshowPayload>(json!({"target_ext": "mp4"})).is_err(),
            "paths is required"
        );

        let p: VideoNormalizePayload =
            serde_json::from_value(json!({"path": "clip.mp4", "target_lufs": -14.0})).unwrap();
        assert_eq!(p.target_lufs, Some(-14.0));

        let p: VideoSceneFramesPayload =
            serde_json::from_value(json!({"path": "clip.mp4", "threshold": 0.5})).unwrap();
        assert_eq!(p.threshold, Some(0.5));

        let p: VideoMergeAvPayload = serde_json::from_value(
            json!({"video_path": "clip.mp4", "audio_path": "voice.wav", "target_ext": "mkv"}),
        )
        .unwrap();
        assert_eq!(p.video_path, "clip.mp4");
        assert_eq!(p.audio_path, "voice.wav");
        assert_eq!(p.target_ext.as_deref(), Some("mkv"));
        assert!(
            serde_json::from_value::<VideoMergeAvPayload>(json!({"video_path": "clip.mp4"}))
                .is_err(),
            "audio_path is required"
        );
    }

    #[test]
    fn video_ops_payloads_parse_with_defaults() {
        let p: VideoChaptersSetPayload = serde_json::from_value(
            json!({"path": "clip.mp4", "chapters": [{"start_ms": 0, "end_ms": 5000, "title": "Intro"}]}),
        )
        .unwrap();
        assert_eq!(p.chapters.len(), 1);
        assert_eq!(p.chapters[0].start_ms, 0);
        assert_eq!(p.chapters[0].title.as_deref(), Some("Intro"));
        assert_eq!(p.max_bytes, DEFAULT_VIDEO_MAX_BYTES);
        assert!(
            serde_json::from_value::<VideoChaptersSetPayload>(json!({"path": "clip.mp4"})).is_err(),
            "chapters is required"
        );

        let p: VideoChaptersShiftPayload =
            serde_json::from_value(json!({"path": "clip.mp4", "offset_ms": -250})).unwrap();
        assert_eq!(p.offset_ms, -250);

        let p: VideoHdrToneMapPayload =
            serde_json::from_value(json!({"path": "clip.mp4", "desat": 2.0, "peak": 1000}))
                .unwrap();
        assert_eq!(p.desat, Some(2.0));
        assert_eq!(p.peak, Some(1000));

        let p: VideoSegmentSplitPayload =
            serde_json::from_value(json!({"path": "clip.mp4", "mode": "scenes"})).unwrap();
        assert_eq!(p.mode.as_deref(), Some("scenes"));
        assert_eq!(p.duration_secs, None);
        assert_eq!(p.threshold, None);
        assert_eq!(p.parts, None);
        assert_eq!(p.format, None);
        let p: VideoSegmentSplitPayload =
            serde_json::from_value(json!({"path": "clip.mp4"})).unwrap();
        assert_eq!(p.mode, None, "mode defaults to duration");

        let p: VideoSegmentExtractPayload = serde_json::from_value(
            json!({"path": "clip.mp4", "start_ms": 1000, "end_ms": 5000, "format": "mkv"}),
        )
        .unwrap();
        assert_eq!(p.start_ms, 1000);
        assert_eq!(p.end_ms, 5000);
        assert_eq!(p.format.as_deref(), Some("mkv"));
        assert!(
            serde_json::from_value::<VideoSegmentExtractPayload>(
                json!({"path": "clip.mp4", "start_ms": 0})
            )
            .is_err(),
            "end_ms is required"
        );

        let p: VideoWaveformPayload =
            serde_json::from_value(json!({"path": "clip.mp4", "width": 640})).unwrap();
        assert_eq!(p.width, Some(640));
        assert_eq!(p.height, None);
        assert_eq!(p.color, None);
        assert_eq!(p.scale, None);

        let p: VideoSyncFixPayload = serde_json::from_value(
            json!({"path": "clip.mp4", "audio_offset_ms": 40, "video_offset_ms": -10}),
        )
        .unwrap();
        assert_eq!(p.audio_offset_ms, Some(40));
        assert_eq!(p.video_offset_ms, Some(-10));

        let p: VideoStabilizePayload = serde_json::from_value(
            json!({"path": "clip.mp4", "shakiness": 7.0, "algo": "deshake", "auto": true}),
        )
        .unwrap();
        assert_eq!(p.shakiness, Some(7.0));
        assert_eq!(p.algo.as_deref(), Some("deshake"));
        assert!(p.auto);

        let p: VideoPreviewPayload =
            serde_json::from_value(json!({"path": "clip.mp4", "fps": 5.0, "width": 160})).unwrap();
        assert_eq!(p.fps, Some(5.0));
        assert_eq!(p.width, Some(160));
        assert_eq!(p.max_duration_secs, None);

        let p: VideoProxyGeneratePayload =
            serde_json::from_value(json!({"path": "clip.mp4", "kind": "mezzanine"})).unwrap();
        assert_eq!(p.kind.as_deref(), Some("mezzanine"));
        assert_eq!(p.target_height, None);
        assert_eq!(p.crf, None);
        assert_eq!(p.preset, None);

        let p: VideoStreamSelectPayload = serde_json::from_value(
            json!({"path": "clip.mp4", "keep_video": false, "audio_ordinals": [0], "drop_attachments": true}),
        )
        .unwrap();
        assert_eq!(p.keep_video, Some(false));
        assert_eq!(p.audio_ordinals, Some(vec![0]));
        assert!(p.drop_attachments.unwrap_or(false));
        assert_eq!(p.subtitle_ordinals, None);
        assert_eq!(p.target_ext, None);

        let p: VideoStreamCoverArtPayload =
            serde_json::from_value(json!({"path": "song.mp3", "cover_path": "cover.png"})).unwrap();
        assert_eq!(p.path, "song.mp3");
        assert_eq!(p.cover_path, "cover.png");
        assert!(
            serde_json::from_value::<VideoStreamCoverArtPayload>(json!({"path": "song.mp3"}))
                .is_err(),
            "cover_path is required"
        );

        let p: VideoFiltersApplyPayload = serde_json::from_value(
            json!({"path": "clip.mp4", "preset": "grayscale", "video_filter": "hflip"}),
        )
        .unwrap();
        assert_eq!(p.preset.as_deref(), Some("grayscale"));
        assert_eq!(p.video_filter.as_deref(), Some("hflip"));
        assert_eq!(p.audio_filter, None);
        assert!(
            serde_json::from_value::<VideoFiltersApplyPayload>(json!({"path": "clip.mp4"})).is_ok(),
            "all filter fields are optional at parse time"
        );
    }

    #[test]
    fn video_ops_helpers_validate_payload_fields() {
        let root = PathBuf::from("/storage");
        let file = root.join("clip.mp4");

        // Segment format: explicit values are allowlisted.
        assert_eq!(segment_format(Some("mkv"), &file).unwrap(), "mkv");
        assert_eq!(segment_format(Some(" MP4 "), &file).unwrap(), "mp4");
        assert!(segment_format(Some("mov"), &file).is_err());
        assert!(segment_format(Some("../x"), &file).is_err());
        // Default: input ext when allowed, else mp4.
        assert_eq!(segment_format(None, &file).unwrap(), "mp4");
        assert_eq!(
            segment_format(None, &root.join("clip.webm")).unwrap(),
            "webm"
        );
        assert_eq!(segment_format(None, &root.join("clip.mov")).unwrap(), "mp4");

        // Enum parsers.
        assert_eq!(
            parse_stabilize_algo(Some("Deshake")).unwrap(),
            crate::crates::video_engine::StabilizeAlgo::Deshake
        );
        assert!(parse_stabilize_algo(Some("magic")).is_err());
        assert_eq!(
            parse_proxy_kind(Some("ARCHIVAL")).unwrap(),
            crate::crates::video_engine::ProxyKind::Archival
        );
        assert!(parse_proxy_kind(Some("tiny")).is_err());

        // Output-extension guessing for proxy bytes.
        assert_eq!(guess_media_ext(&[0x1A, 0x45, 0xDF, 0xA3, 0, 0]), "mkv");
        let mut mp4 = vec![0u8; 12];
        mp4[4..8].copy_from_slice(b"ftyp");
        assert_eq!(guess_media_ext(&mp4), "mp4");
        assert_eq!(guess_media_ext(b"junk"), "bin");

        // Waveform canvas bounds.
        let payload = VideoWaveformPayload {
            path: "clip.mp4".into(),
            width: Some(0),
            height: None,
            color: None,
            scale: None,
            max_bytes: DEFAULT_VIDEO_MAX_BYTES,
        };
        assert!(
            waveform_image_at(
                &root,
                &payload,
                crate::crates::video_engine::WaveformKind::Waveform,
                "waveform"
            )
            .is_err()
        );
        let payload = VideoWaveformPayload {
            width: Some(MAX_WAVEFORM_DIMENSION + 1),
            ..payload
        };
        assert!(
            waveform_image_at(
                &root,
                &payload,
                crate::crates::video_engine::WaveformKind::Waveform,
                "waveform"
            )
            .is_err()
        );
    }

    #[test]
    fn video_extended_handlers_reject_path_escapes() {
        let root = PathBuf::from("/storage");
        let path_payload = |bad: &str| VideoPathPayload {
            path: bad.into(),
            max_bytes: DEFAULT_VIDEO_MAX_BYTES,
        };
        for bad in ["", "../clip.mp4", "/abs/clip.mp4", "a/../../clip.mp4"] {
            assert!(
                remux_video_at(
                    &root,
                    &VideoTranscodePayload {
                        path: bad.into(),
                        target_ext: "mkv".into(),
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "remux should reject {bad:?}"
            );
            assert!(
                extract_frames_video_at(
                    &root,
                    &VideoExtractFramesPayload {
                        path: bad.into(),
                        every_secs: None,
                        fps: None,
                        all: false,
                        keyframes_only: false,
                        format: None,
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "extract_frames should reject {bad:?}"
            );
            assert!(
                subtitles_extract_video_at(
                    &root,
                    &VideoSubtitlesExtractPayload {
                        path: bad.into(),
                        track: None,
                        format: None,
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "subtitles_extract should reject {bad:?}"
            );
            assert!(analyze_video_at(&root, &path_payload(bad)).is_err());
            assert!(package_hls_video_at(&root, &path_payload(bad)).is_err());
            assert!(
                metadata_set_video_at(
                    &root,
                    &VideoMetadataSetPayload {
                        path: bad.into(),
                        tags: BTreeMap::new(),
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "metadata_set should reject {bad:?}"
            );
            assert!(metadata_strip_video_at(&root, &path_payload(bad)).is_err());
            assert!(
                extract_streams_video_at(
                    &root,
                    &VideoExtractStreamsPayload {
                        path: bad.into(),
                        subtitles: true,
                        attachments: false,
                        cover_art: false,
                        chapters: false,
                        data: false,
                        subtitle_format: None,
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "extract_streams should reject {bad:?}"
            );
            assert!(
                audio_volume_video_at(
                    &root,
                    &VideoAudioVolumePayload {
                        path: bad.into(),
                        gain_db: 0.0,
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "audio_volume should reject {bad:?}"
            );
            assert!(
                contact_sheet_video_at(
                    &root,
                    &VideoContactSheetPayload {
                        path: bad.into(),
                        cols: None,
                        rows: None,
                        thumb_width: None,
                        interval_secs: None,
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "contact_sheet should reject {bad:?}"
            );
            assert!(
                speed_video_at(
                    &root,
                    &VideoSpeedPayload {
                        path: bad.into(),
                        factor: 1.0,
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "speed should reject {bad:?}"
            );
            assert!(reverse_video_at(&root, &path_payload(bad)).is_err());
            assert!(
                loop_video_at(
                    &root,
                    &VideoLoopPayload {
                        path: bad.into(),
                        count: 1,
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "loop should reject {bad:?}"
            );
            assert!(
                freeze_video_at(
                    &root,
                    &VideoFreezePayload {
                        path: bad.into(),
                        at_secs: 0.0,
                        duration_secs: 1.0,
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "freeze should reject {bad:?}"
            );
            assert!(
                resize_video_at(
                    &root,
                    &VideoResizePayload {
                        path: bad.into(),
                        width: 64,
                        height: 64,
                        fit: None,
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "resize should reject {bad:?}"
            );
            assert!(
                crop_video_at(
                    &root,
                    &VideoCropPayload {
                        path: bad.into(),
                        x: 0,
                        y: 0,
                        w: 10,
                        h: 10,
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "crop should reject {bad:?}"
            );
            assert!(
                rotate_video_at(
                    &root,
                    &VideoRotatePayload {
                        path: bad.into(),
                        degrees: 90.0,
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "rotate should reject {bad:?}"
            );
            assert!(
                flip_video_at(
                    &root,
                    &VideoFlipPayload {
                        path: bad.into(),
                        horizontal: None,
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "flip should reject {bad:?}"
            );
            assert!(
                gif_video_at(
                    &root,
                    &VideoGifPayload {
                        path: bad.into(),
                        fps: None,
                        width: None,
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "gif should reject {bad:?}"
            );
            assert!(
                slideshow_video_at(
                    &root,
                    &VideoSlideshowPayload {
                        paths: vec![bad.into()],
                        fps: None,
                        duration_per_image: None,
                        target_ext: "mp4".into(),
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "slideshow should reject {bad:?}"
            );
            assert!(
                normalize_video_at(
                    &root,
                    &VideoNormalizePayload {
                        path: bad.into(),
                        target_lufs: None,
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "normalize should reject {bad:?}"
            );
            assert!(loudness_video_at(&root, &path_payload(bad)).is_err());
            assert!(
                scene_frames_video_at(
                    &root,
                    &VideoSceneFramesPayload {
                        path: bad.into(),
                        threshold: None,
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "scene_frames should reject {bad:?}"
            );
            assert!(
                merge_av_video_at(
                    &root,
                    &VideoMergeAvPayload {
                        video_path: bad.into(),
                        audio_path: "voice.wav".into(),
                        target_ext: None,
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "merge_av should reject bad video_path {bad:?}"
            );
            assert!(
                merge_av_video_at(
                    &root,
                    &VideoMergeAvPayload {
                        video_path: "clip.mp4".into(),
                        audio_path: bad.into(),
                        target_ext: None,
                        max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                    },
                )
                .is_err(),
                "merge_av should reject bad audio_path {bad:?}"
            );
        }
        // Slideshow additionally rejects an empty path list before any
        // file access.
        assert!(
            slideshow_video_at(
                &root,
                &VideoSlideshowPayload {
                    paths: vec![],
                    fps: None,
                    duration_per_image: None,
                    target_ext: "mp4".into(),
                    max_bytes: DEFAULT_VIDEO_MAX_BYTES,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn video_media_output_names_are_sanitized() {
        assert!(sanitize_media_name("index.m3u8").is_ok());
        assert!(sanitize_media_name("sub/seg_000.ts").is_ok());
        for bad in ["", "  ", "/abs/x", "../x", "a/../../x", ".."] {
            assert!(
                sanitize_media_name(bad).is_err(),
                "name {bad:?} should be rejected"
            );
        }
        // An empty MediaOutput is an error rather than a silent no-op.
        let root = temp_root();
        let empty = crate::crates::video_engine::MediaOutput { files: vec![] };
        assert!(write_media_output(&root, &empty).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn video_resize_fit_and_ext_sanitizers() {
        use crate::crates::video_engine::FitMode;
        assert_eq!(parse_fit_mode(None).unwrap(), FitMode::Fit);
        assert_eq!(parse_fit_mode(Some(" COVER ")).unwrap(), FitMode::Cover);
        assert_eq!(parse_fit_mode(Some("contain")).unwrap(), FitMode::Contain);
        assert_eq!(parse_fit_mode(Some("stretch")).unwrap(), FitMode::Stretch);
        assert!(parse_fit_mode(Some("bogus")).is_err());
        assert_eq!(sanitize_ext_field("format", " PNG ").unwrap(), "png");
        assert!(sanitize_ext_field("format", "a/b").is_err());
    }

    #[test]
    fn video_remux_writes_output_when_ffmpeg_available() {
        use std::process::Command;
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            return;
        }
        let root = temp_root();
        let src = root.join("clip.mp4");
        // `mpeg4` is an ffmpeg-native encoder — unlike `libx264` it is
        // always available, so this fixture works on minimal builds.
        let generated = Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:d=1:r=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-c:v",
                "mpeg4",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest",
            ])
            .arg(&src)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !generated || !src.is_file() {
            std::fs::remove_dir_all(&root).ok();
            return; // encoder unavailable — skip
        }

        remux_video_at(
            &root,
            &VideoTranscodePayload {
                path: "clip.mp4".into(),
                target_ext: "mkv".into(),
                max_bytes: DEFAULT_VIDEO_MAX_BYTES,
            },
        )
        .unwrap();
        let out = root.join("clip.mp4.mkv");
        assert!(out.is_file(), "remux output should be written");
        assert!(out.metadata().unwrap().len() > 0);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn gc_staging_dir_removes_only_expired_entries() {
        let root = temp_root();
        let staging = root.join("staging");
        std::fs::create_dir_all(&staging).unwrap();

        // Fresh file (modified now) — must survive any positive retention.
        let fresh = staging.join("fresh.bin");
        std::fs::write(&fresh, b"new").unwrap();
        // A directory full of files — also fresh.
        let dir = staging.join("dir");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("nested/f.bin"), b"x").unwrap();

        // retention = 0 → cutoff is ~now, everything created before the
        // call is "expired".
        let removed = gc_staging_dir(&staging, Duration::ZERO);
        assert_eq!(removed, 2);
        assert!(!fresh.exists());
        assert!(!dir.exists());

        // Missing staging dir is a no-op.
        assert_eq!(gc_staging_dir(&root.join("nope"), Duration::ZERO), 0);

        // Huge retention keeps everything.
        std::fs::write(&fresh, b"new").unwrap();
        let kept = gc_staging_dir(&staging, Duration::from_secs(u64::MAX));
        assert_eq!(kept, 0);
        assert!(fresh.exists());

        std::fs::remove_dir_all(&root).ok();
    }

    // -----------------------------------------------------------------
    // document.metadata_extract / document.index / thumbnail.generate
    // -----------------------------------------------------------------

    use crate::crates::document_engine::store::metadata::get_document_metadata;

    /// Engine with every built-in document handler registered against
    /// an explicit storage root (no app config involved).
    fn full_engine(pool: SqlitePool, root: &std::path::Path) -> JobEngine {
        let storage = || StorageConfig::new(root);
        let mut eng = JobEngine::new(
            pool,
            JobEngineConfig {
                poll_interval: Duration::from_millis(10),
                default_max_attempts: 3,
                backoff_base: Duration::from_secs(0),
                stale_after: Duration::from_secs(60),
            },
        );
        eng.register(
            DOCUMENT_INGEST_KIND,
            Arc::new(DocumentIngestHandler::with_storage_config(storage())),
        );
        eng.register(
            DOCUMENT_METADATA_EXTRACT_KIND,
            Arc::new(DocumentMetadataExtractHandler::with_storage_config(
                storage(),
            )),
        );
        eng.register(
            DOCUMENT_INDEX_KIND,
            Arc::new(DocumentIndexHandler::with_storage_config(storage())),
        );
        eng.register(
            THUMBNAIL_GENERATE_KIND,
            Arc::new(ThumbnailGenerateHandler::with_storage_config(storage())),
        );
        eng
    }

    /// Creates a document, ingests `bytes` as `name`, and returns its id.
    async fn ingest_doc(
        eng: &JobEngine,
        pool: &SqlitePool,
        doc_id: &DocumentId,
        bytes: &[u8],
        name: &str,
        mime: &str,
    ) {
        enqueue_ingest(pool, doc_id, Some(name), Some(mime), bytes)
            .await
            .unwrap();
        assert!(eng.run_once().await.unwrap());
    }

    /// Builds a real one-page PDF with extractable text and an Info
    /// dict, using lopdf (same approach as pdf-engine's test_utils).
    fn test_pdf_bytes() -> Vec<u8> {
        use lopdf::content::{Content, Operation};
        use lopdf::{Object, Stream, dictionary};

        let mut doc = lopdf::Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Courier",
        });
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![100.into(), 700.into()]),
                Operation::new("Tj", vec![Object::string_literal("Hello KalaDoc")]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        let info_id = doc.add_object(dictionary! {
            "Title" => Object::string_literal("Job Handler Test"),
            "Author" => Object::string_literal("KalaDoc Tests"),
        });
        doc.trailer.set("Root", catalog_id);
        doc.trailer.set("Info", info_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    /// Like [`test_pdf_bytes`] but adds a 2x2 DeviceRGB image XObject —
    /// used to exercise real thumbnail extraction.
    fn test_pdf_with_image_bytes() -> Vec<u8> {
        use lopdf::content::{Content, Operation};
        use lopdf::{Object, Stream, dictionary};

        let mut doc = lopdf::Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Courier",
        });
        // 2x2 RGB pixels, uncompressed.
        let img = Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 2,
                "Height" => 2,
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => 8,
            },
            vec![
                255, 0, 0, 0, 255, 0, //
                0, 0, 255, 255, 255, 0,
            ],
        );
        let img_id = doc.add_object(img);
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![100.into(), 700.into()]),
                Operation::new("Tj", vec![Object::string_literal("doc with image")]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
            "XObject" => dictionary! { "Im1" => img_id },
        });
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    async fn metadata_of(pool: &SqlitePool, doc_id: &DocumentId) -> DocumentMetadata {
        get_document_metadata(pool, doc_id)
            .await
            .unwrap()
            .expect("metadata row")
    }

    #[tokio::test]
    async fn metadata_extract_pdf_records_info() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        ingest_doc(
            &eng,
            &pool,
            &doc_id,
            &test_pdf_bytes(),
            "report.pdf",
            "application/pdf",
        )
        .await;

        enqueue_metadata_extract(&pool, &doc_id, None)
            .await
            .unwrap();
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        let cf = &meta.custom_fields;
        assert_eq!(cf["metadata_extract_status"], json!("extracted"));
        assert_eq!(cf["processing_status"], json!("succeeded"));
        assert_eq!(cf["detected_format"], json!("pdf"));
        assert_eq!(cf["mime_type"], json!("application/pdf"));
        assert_eq!(cf["page_count"], json!(1));
        assert_eq!(cf["pdf_title"], json!("Job Handler Test"));
        assert!(cf["metadata_extracted_at"].is_string());
        // Title/author columns filled from the Info dict (were unset).
        assert_eq!(meta.title.as_deref(), Some("Job Handler Test"));
        assert_eq!(meta.author.as_deref(), Some("KalaDoc Tests"));

        // Audit: started + completed events.
        let history = get_document_history(&pool, &doc_id).await.unwrap();
        let types: Vec<_> = history.iter().map(|e| e.event_type).collect();
        assert!(types.contains(&DocumentAuditEventType::ProcessingStarted));
        assert!(types.contains(&DocumentAuditEventType::ProcessingCompleted));

        // Job succeeded.
        let jobs = list_jobs(&pool, Some(JobStatus::Succeeded), 10)
            .await
            .unwrap();
        assert_eq!(jobs.len(), 2); // ingest + extract
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn metadata_extract_unsupported_format_is_honest() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        // PNG signature bytes — recognized but not PDF-extractable.
        let png = b"\x89PNG\r\n\x1a\nrest-of-file";
        ingest_doc(&eng, &pool, &doc_id, png, "img.png", "image/png").await;

        enqueue_metadata_extract(&pool, &doc_id, None)
            .await
            .unwrap();
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(
            meta.custom_fields["metadata_extract_status"],
            json!("unsupported")
        );
        assert_eq!(
            meta.custom_fields["processing_status"],
            json!("unsupported")
        );
        assert_eq!(meta.custom_fields["detected_format"], json!("png"));
        assert_eq!(meta.custom_fields["mime_type"], json!("image/png"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn metadata_extract_missing_document_fails_job() {
        let pool = test_pool().await;
        let root = temp_root();
        let eng = full_engine(pool.clone(), &root);
        let ghost = DocumentId::new("ghost-doc");

        enqueue_metadata_extract(&pool, &ghost, None).await.unwrap();
        // max_attempts default is 3 — run all attempts.
        for _ in 0..3 {
            assert!(eng.run_once().await.unwrap());
        }
        let jobs = list_jobs(&pool, Some(JobStatus::Failed), 10).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(
            jobs[0]
                .last_error
                .as_deref()
                .unwrap_or("")
                .contains("document not found")
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn index_pdf_extracts_text() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        ingest_doc(
            &eng,
            &pool,
            &doc_id,
            &test_pdf_bytes(),
            "report.pdf",
            "application/pdf",
        )
        .await;

        enqueue_index(&pool, &doc_id, None).await.unwrap();
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["index_status"], json!("indexed"));
        assert_eq!(meta.custom_fields["processing_status"], json!("succeeded"));
        let index = &meta.custom_fields["index"];
        assert_eq!(index["status"], json!("indexed"));
        assert_eq!(index["format"], json!("pdf"));
        assert!(index["char_count"].as_u64().unwrap() > 0);
        assert!(index["word_count"].as_u64().unwrap() > 0);
        assert_eq!(index["sha256"].as_str().unwrap().len(), 64);
        assert!(index["excerpt"].as_str().unwrap().contains("Hello KalaDoc"));

        let history = get_document_history(&pool, &doc_id).await.unwrap();
        assert!(
            history
                .iter()
                .any(|e| e.event_type == DocumentAuditEventType::Indexed)
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn index_plain_text_document() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        ingest_doc(
            &eng,
            &pool,
            &doc_id,
            b"the quick brown fox jumps over the lazy dog",
            "notes.txt",
            "text/plain",
        )
        .await;

        enqueue_index(&pool, &doc_id, None).await.unwrap();
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["index_status"], json!("indexed"));
        let index = &meta.custom_fields["index"];
        assert_eq!(index["char_count"], json!(43));
        assert_eq!(index["word_count"], json!(9));
        assert!(
            index["excerpt"]
                .as_str()
                .unwrap()
                .contains("quick brown fox")
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn index_unsupported_format_is_honest() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        // Valid PNG signature + non-UTF8 payload → not indexable.
        let bytes: &[u8] = b"\x89PNG\r\n\x1a\n\xff\xfe\x00\x01binary";
        ingest_doc(&eng, &pool, &doc_id, bytes, "img.png", "image/png").await;

        enqueue_index(&pool, &doc_id, None).await.unwrap();
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["index_status"], json!("unsupported"));
        assert_eq!(
            meta.custom_fields["processing_status"],
            json!("unsupported")
        );
        assert!(meta.custom_fields.get("index").is_none());
        // Job itself succeeded.
        let jobs = list_jobs(&pool, Some(JobStatus::Succeeded), 10)
            .await
            .unwrap();
        assert_eq!(jobs.len(), 2);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn thumbnail_generated_from_image_xobject() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        ingest_doc(
            &eng,
            &pool,
            &doc_id,
            &test_pdf_with_image_bytes(),
            "withimg.pdf",
            "application/pdf",
        )
        .await;

        enqueue_thumbnail(&pool, &doc_id, None).await.unwrap();
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["thumbnail_status"], json!("generated"));
        assert_eq!(meta.custom_fields["processing_status"], json!("succeeded"));
        assert_eq!(meta.custom_fields["thumbnail_format"], json!("png"));
        assert_eq!(meta.custom_fields["thumbnail_page"], json!(1));

        // Thumbnail bytes really landed in storage as a PNG.
        let arts = list_artifacts(&pool, &doc_id).await.unwrap();
        let uri = meta.custom_fields["thumbnail_uri"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(uri.starts_with("local://documents/"));
        assert!(uri.contains("/thumbnails/"));
        assert!(uri.ends_with(".png"));
        let object_name = uri.trim_start_matches("local://");
        let on_disk = root.join(object_name);
        let bytes = std::fs::read(&on_disk).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(
            meta.custom_fields["thumbnail_bytes"].as_u64().unwrap(),
            bytes.len() as u64
        );
        assert_eq!(arts.len(), 1); // thumbnail is not an artifact row

        let history = get_document_history(&pool, &doc_id).await.unwrap();
        assert!(
            history
                .iter()
                .any(|e| e.event_type == DocumentAuditEventType::ThumbnailGenerated)
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn thumbnail_unavailable_without_embedded_image() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        ingest_doc(
            &eng,
            &pool,
            &doc_id,
            &test_pdf_bytes(),
            "plain.pdf",
            "application/pdf",
        )
        .await;

        enqueue_thumbnail(&pool, &doc_id, None).await.unwrap();
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["thumbnail_status"], json!("unavailable"));
        assert_eq!(
            meta.custom_fields["processing_status"],
            json!("unavailable")
        );
        assert!(meta.custom_fields.get("thumbnail_uri").is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn thumbnail_unsupported_for_non_pdf() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        ingest_doc(&eng, &pool, &doc_id, b"plain text", "a.txt", "text/plain").await;

        enqueue_thumbnail(&pool, &doc_id, None).await.unwrap();
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["thumbnail_status"], json!("unsupported"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn processing_status_transitions_queued_running_succeeded() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        ingest_doc(
            &eng,
            &pool,
            &doc_id,
            &test_pdf_bytes(),
            "r.pdf",
            "application/pdf",
        )
        .await;

        // Enqueue helper stamps "queued".
        enqueue_metadata_extract(&pool, &doc_id, None)
            .await
            .unwrap();
        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["processing_status"], json!("queued"));
        assert_eq!(
            meta.custom_fields["metadata_extract_status"],
            json!("queued")
        );

        assert!(eng.run_once().await.unwrap());
        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["processing_status"], json!("succeeded"));
        assert_eq!(
            meta.custom_fields["metadata_extract_status"],
            json!("extracted")
        );

        // Audit ordering: started before completed.
        let history = get_document_history(&pool, &doc_id).await.unwrap();
        let pos = |t: DocumentAuditEventType| history.iter().position(|e| e.event_type == t);
        let started = pos(DocumentAuditEventType::ProcessingStarted).unwrap();
        let completed = pos(DocumentAuditEventType::ProcessingCompleted).unwrap();
        assert!(started < completed);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn processing_jobs_are_idempotent_on_rerun() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        ingest_doc(
            &eng,
            &pool,
            &doc_id,
            &test_pdf_with_image_bytes(),
            "i.pdf",
            "application/pdf",
        )
        .await;

        // Run each processing job twice — second run must succeed and
        // leave a single consistent state (no duplicate artifacts,
        // deterministic thumbnail path).
        for kind_payload in [
            (
                DOCUMENT_METADATA_EXTRACT_KIND,
                json!({"document_id": doc_id.as_str()}),
            ),
            (DOCUMENT_INDEX_KIND, json!({"document_id": doc_id.as_str()})),
            (
                THUMBNAIL_GENERATE_KIND,
                json!({"document_id": doc_id.as_str()}),
            ),
        ] {
            for _ in 0..2 {
                eng.enqueue(&NewJob {
                    kind: kind_payload.0.into(),
                    payload: kind_payload.1.clone(),
                    priority: 0,
                    run_at: None,
                    max_attempts: None,
                })
                .await
                .unwrap();
                assert!(eng.run_once().await.unwrap());
            }
        }

        assert_eq!(list_artifacts(&pool, &doc_id).await.unwrap().len(), 1);
        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(
            meta.custom_fields["metadata_extract_status"],
            json!("extracted")
        );
        assert_eq!(meta.custom_fields["index_status"], json!("indexed"));
        assert_eq!(meta.custom_fields["thumbnail_status"], json!("generated"));
        // Exactly one thumbnail object on disk.
        let thumbs = root
            .join("documents")
            .join(doc_id.as_str())
            .join("thumbnails");
        let entries: Vec<_> = std::fs::read_dir(&thumbs).unwrap().collect();
        assert_eq!(entries.len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn crash_recovery_reexecutes_processing_job() {
        let pool = test_pool().await;
        let root = temp_root();
        let doc_id = make_document(&pool).await;
        let eng = full_engine(pool.clone(), &root);
        ingest_doc(
            &eng,
            &pool,
            &doc_id,
            &test_pdf_bytes(),
            "c.pdf",
            "application/pdf",
        )
        .await;

        enqueue_index(&pool, &doc_id, None).await.unwrap();
        let job = list_jobs(&pool, None, 10)
            .await
            .unwrap()
            .into_iter()
            .find(|j| j.kind == DOCUMENT_INDEX_KIND)
            .unwrap();

        // Simulate a worker that claimed the job then died.
        sqlx::query(
            "UPDATE jobs_tbl SET status = 'running', worker_id = 'dead-worker',
             started_at = ?, attempts = 1 WHERE id = ?",
        )
        .bind(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
        )
        .bind(job.id)
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(eng.recover().await.unwrap(), 1);
        assert!(eng.run_once().await.unwrap());

        let meta = metadata_of(&pool, &doc_id).await;
        assert_eq!(meta.custom_fields["index_status"], json!("indexed"));
        std::fs::remove_dir_all(&root).ok();
    }
}
