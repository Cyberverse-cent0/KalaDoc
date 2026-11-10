//! Operation dispatch for the `media.op` job kind.
//!
//! [`run_media_op`] maps a `media.op` payload's `op` name plus its
//! `params` JSON onto the implemented functions of
//! [`multimedia-engine`](crate::crates::multimedia_engine). Only ops
//! that actually exist are dispatched — placeholder modules
//! (1-line stubs) and multi-input ops that need more than the single
//! resolved input are deliberately absent from [`MEDIA_OPS`]; unknown
//! names fail validation with the supported list.
//!
//! All ops are synchronous and process-backed (ffmpeg/ffprobe): the
//! job handler calls this inside `tokio::task::spawn_blocking` via
//! [`crate::crates::application::job_handlers`]' blocking helper.
//! Ops manage their own temp dirs internally — inputs arrive as byte
//! buffers and outputs come back as [`MediaOutput`] files or a
//! structured report value; there is no temp-file lifecycle for the
//! caller to clean up.
//!
//! # Op coverage
//!
//! | Op | Kind | Params (`params` object) |
//! |----|------|---------------------------|
//! | `probe.inspect` | report | — |
//! | `probe.streams` | report | — |
//! | `probe.chapters` | report | — |
//! | `metadata.read_tags` | report | — |
//! | `metadata.strip` | files | [`StripOptions`] |
//! | `metadata.edit_tags` | files | [`TagOptions`] |
//! | `subtitles.list` | report | — |
//! | `subtitles.convert` | files | `input_format`, `output_format` (`srt`/`ass`/`vtt`/`txt`) |
//! | `subtitles.extract` | files | `index?`, `output_format?` |
//! | `subtitles.burn` | files | [`BurnOptions`] + `subtitle_b64`/`subtitle_artifact_id` (handler-resolved) |
//! | `analysis.loudness` | report | [`LoudnessOptions`] |
//! | `analysis.silence` | report | [`SilenceOptions`] |
//! | `analysis.blackdetect` | report | [`BlackDetectOptions`] |
//! | `analysis.freezedetect` | report | [`FreezeOptions`] |
//! | `analysis.signalstats` | report | [`SignalStatsOptions`] |
//! | `conversion.transcode` | files | [`TranscodeOptions`] |
//! | `conversion.remux` | files | [`RemuxOptions`] |
//! | `conversion.extract_audio` | files | `output_format` (req), `audio_codec?`, `bitrate?`, `stream_index?`, `strip_video?` |
//! | `video.trim` | files | [`TrimOptions`] |
//! | `video.speed` | files | `factor` (req), `preserve_pitch?`, `output_format?` |
//! | `video.resize` | files | [`ResizeOptions`] |
//! | `video.crop` | files | [`CropOptions`] |
//! | `video.pad` | files | [`PadOptions`] |
//! | `video.letterbox` | files | [`LetterboxOptions`] |
//! | `video.pillarbox` | files | [`LetterboxOptions`] |
//! | `video.remove_black_borders` | files | [`BlackBorderOptions`] |
//! | `video.orient` | files | `op` (req): `rotate_90`/`rotate_180`/`rotate_270`/`flip_h`/`flip_v`/`transpose`/`transverse` |
//! | `video.split` | files | [`SplitOptions`] |
//! | `audio.volume` | files | [`VolumeOptions`] |
//! | `audio.trim` | files | [`AudioTrimOptions`] |
//! | `audio.normalize` | files | [`NormalizeOptions`] |
//! | `audio.channels` | files | `channel_op`/`op`: `"to_mono"`, `"to_stereo"`, `"ToMono"`, `"ToStereo"`, or `{"Custom":{...}}` |
//! | `audio.resample` | files | `sample_rate` (req, Hz) |
//! | `audio.fades` | files | [`FadeOptions`] |
//! | `frames.thumbnail` | files | `at_secs?`, `width?`, `height?`, `output_format?` (default `jpg`), `quality?` |
//! | `frames.frame_at` | files | `at_secs?`, `output_format?` (default `png`), `width?`, `height?`, `quality?` |
//! | `frames.keyframes` | files | [`KeyframeOptions`] |
//! | `packaging.hls` | files | [`HlsOptions`] |
//! | `packaging.dash` | files | [`DashOptions`] |
//! | `filters.apply` | files | [`FilterGraphOptions`] (`video_filter?`, `audio_filter?`, `map?`, `frames?`, `output_format`) |
//! | `filters.presets` | files | [`PresetOptions`] (`preset` req, `intensity?`, `output_format?`) |
//! | `filters.list_presets` | report | — |
//! | `encoding.two_pass` | files | [`TwoPassOptions`] |
//! | `container.select_streams` | files | [`SelectStreamsOptions`] |
//! | `container.set_language` | files | [`LanguageOptions`] (`assignments` req) |
//! | `container.chapters_edit` | files | [`ChaptersEditOptions`] |
//! | `extraction.attachments` | files | [`AttachmentOptions`] |
//! | `extraction.list_attachments` | report | — |
//! | `extraction.chapters_export` | files | [`ChapterExportOptions`] (`formats`) |
//! | `extraction.embedded_images` | files | [`EmbeddedImageOptions`] |
//! | `extraction.list_embedded_images` | report | — |
//! | `extraction.stream_copy` | files | [`StreamCopyOptions`] |
//! | `metadata.chapters` | files | [`ChaptersOptions`] |
//! | `metadata.cover_art_add` / `_extract` / `_remove` | files/report | second input for `_add` |
//! | `video.reverse`/`loop`/`freeze_frame`/`chroma_key`/`fps_convert` | files | option structs |
//! | `video.picture_in_picture`/`split_screen`/`xfade`/`overlay` | files | second input via `secondary_input` |
//! | `audio.equalizer`/`compressor`/`channel_remix`/`tempo`/`spectrogram`/`remove_silence` | files | option structs (`equalizer` also takes `params.bands`) |
//! | `audio.crossfade` | files | second input via `secondary_input` |
//! | `frames.all_frames`/`gif_preview` | files | option structs |
//! | `subtitles.retime` | files | [`RetimeOptions`] (subtitle file input) |
//! | `analysis.histogram`/`av_sync` | report | option structs |
//! | `analysis.quality` | report | `metric` (`psnr`/`ssim`) + second input |
//! | `conversion.mux_replace_audio` | files | second input (audio) |
//!
//! Bracketed params are the multimedia-engine option structs; they are
//! `serde` types, so `params` is deserialized directly into them
//! (missing fields fall back to the struct's `Default` where one
//! exists — the `*_defaulted` helpers below merge `Default` values
//! under the supplied params first).
//!
//! Two-input ops consume `secondary_input` (the job handler/CLI
//! resolves it from a second payload field). Ops needing more than
//! two inputs (`video.concat`, `audio.merge`, `conversion.slideshow`,
//! `conversion.mux_streams`) or whose option structs are not `serde`
//! (`video.remove_sections`, `frames.interval`, `frames.contact_sheet`,
//! `video.scenes`) remain unwired.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::crates::multimedia_engine::analysis::av_sync::{AvSyncOptions, av_sync};
use crate::crates::multimedia_engine::analysis::blackdetect::{BlackDetectOptions, blackdetect};
use crate::crates::multimedia_engine::analysis::freezedetect::{
    FreezeOptions as FreezeDetectOptions, detect_freeze,
};
use crate::crates::multimedia_engine::analysis::histogram::{HistogramOptions, histogram};
use crate::crates::multimedia_engine::analysis::loudness::{LoudnessOptions, measure_loudness};
use crate::crates::multimedia_engine::analysis::quality::{QualityMetric, compare_quality};
use crate::crates::multimedia_engine::analysis::signalstats::{SignalStatsOptions, signal_stats};
use crate::crates::multimedia_engine::analysis::silence::{SilenceOptions, silence_detect};
use crate::crates::multimedia_engine::audio::channel_remix::{ChannelRemixOptions, remix_channels};
use crate::crates::multimedia_engine::audio::channels::{ChannelOp, channels, resample};
use crate::crates::multimedia_engine::audio::compressor::{CompressorOptions, compress};
use crate::crates::multimedia_engine::audio::crossfade::{CrossfadeOptions, crossfade};
use crate::crates::multimedia_engine::audio::equalizer::{EqBand, EqOptions, equalizer};
use crate::crates::multimedia_engine::audio::fades::{FadeOptions, apply_fades};
use crate::crates::multimedia_engine::audio::normalize::{NormalizeOptions, normalize_loudness};
use crate::crates::multimedia_engine::audio::remove_silence::{
    RemoveSilenceOptions, remove_silence,
};
use crate::crates::multimedia_engine::audio::spectrogram::{
    SpectrogramOptions, render_spectrogram,
};
use crate::crates::multimedia_engine::audio::tempo::{TempoOptions, tempo};
use crate::crates::multimedia_engine::audio::trim::{AudioTrimOptions, trim_audio};
use crate::crates::multimedia_engine::audio::volume::{VolumeOptions, adjust_volume};
use crate::crates::multimedia_engine::container::chapters_edit::{
    ChaptersEditOptions, chapters_edit,
};
use crate::crates::multimedia_engine::container::select_streams::{
    SelectStreamsOptions, select_streams,
};
use crate::crates::multimedia_engine::container::set_language::{LanguageOptions, set_language};
use crate::crates::multimedia_engine::conversion::extract_audio::{
    ExtractAudioOptions, extract_audio,
};
use crate::crates::multimedia_engine::conversion::mux_replace::{
    ReplaceAudioOptions, replace_audio,
};
use crate::crates::multimedia_engine::conversion::remux::{RemuxOptions, remux};
use crate::crates::multimedia_engine::conversion::transcode::{TranscodeOptions, transcode};
use crate::crates::multimedia_engine::encoding::two_pass::{TwoPassOptions, two_pass};
use crate::crates::multimedia_engine::error::MediaError;
use crate::crates::multimedia_engine::extraction::attachments::{
    AttachmentOptions, extract_attachments, list_attachments,
};
use crate::crates::multimedia_engine::extraction::chapters_export::{
    ChapterExportOptions, export_chapters,
};
use crate::crates::multimedia_engine::extraction::embedded_images::{
    EmbeddedImageOptions, extract_embedded_images, list_embedded_images,
};
use crate::crates::multimedia_engine::extraction::stream_copy::{StreamCopyOptions, stream_copy};
use crate::crates::multimedia_engine::filters::graph::{FilterGraphOptions, apply_filters};
use crate::crates::multimedia_engine::filters::presets::{
    PresetOptions, apply_preset, list_presets,
};
use crate::crates::multimedia_engine::frames::all_frames::{AllFramesOptions, all_frames};
use crate::crates::multimedia_engine::frames::frame_at::{FrameOptions, frame_at};
use crate::crates::multimedia_engine::frames::gif_preview::{GifPreviewOptions, gif_preview};
use crate::crates::multimedia_engine::frames::keyframes::{KeyframeOptions, keyframes};
use crate::crates::multimedia_engine::frames::thumbnail::{ThumbnailOptions, thumbnail};
use crate::crates::multimedia_engine::metadata::chapters::{
    ChaptersOptions, chapters as write_chapters,
};
use crate::crates::multimedia_engine::metadata::cover_art::{
    add_cover_art, extract_cover_art, remove_cover_art,
};
use crate::crates::multimedia_engine::metadata::strip::{StripOptions, strip_metadata};
use crate::crates::multimedia_engine::metadata::tags::{TagOptions, edit_tags, read_tags};
use crate::crates::multimedia_engine::model::{MediaInput, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::packaging::dash::{DashOptions, dash_package};
use crate::crates::multimedia_engine::packaging::hls::{HlsOptions, hls_package};
use crate::crates::multimedia_engine::probe::chapters::chapters;
use crate::crates::multimedia_engine::probe::inspect::inspect;
use crate::crates::multimedia_engine::probe::streams::probe_streams;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::subtitles::burn::{BurnOptions, burn_subtitles};
use crate::crates::multimedia_engine::subtitles::convert::{
    ConvertOptions as SubConvertOptions, SubtitleFormat as SubConvertFormat, convert_subtitles,
};
use crate::crates::multimedia_engine::subtitles::extract::{
    SubtitleFormat as SubExtractFormat, SubtitleOptions, extract_subtitles, list_subtitles,
};
use crate::crates::multimedia_engine::subtitles::retime::{RetimeOptions, retime};
use crate::crates::multimedia_engine::video::chroma_key::{ChromaKeyOptions, chroma_key};
use crate::crates::multimedia_engine::video::fps_convert::{FpsOptions, fps_convert};
use crate::crates::multimedia_engine::video::freeze_frame::{
    FreezeOptions as FreezeFrameOptions, freeze_frame,
};
use crate::crates::multimedia_engine::video::geometry::{
    BlackBorderOptions, CropOptions, LetterboxOptions, PadOptions, ResizeOptions, crop, letterbox,
    pad, pillarbox, remove_black_borders, resize,
};
use crate::crates::multimedia_engine::video::r#loop::{LoopOptions, loop_video};
use crate::crates::multimedia_engine::video::orient::{OrientOp, orient};
use crate::crates::multimedia_engine::video::overlay::{OverlayOptions, overlay_image};
use crate::crates::multimedia_engine::video::picture_in_picture::{
    PictureInPictureOptions, picture_in_picture,
};
use crate::crates::multimedia_engine::video::reverse::{ReverseOptions, reverse};
use crate::crates::multimedia_engine::video::speed::SpeedOptions;
use crate::crates::multimedia_engine::video::split::{SplitOptions, split};
use crate::crates::multimedia_engine::video::split_screen::{SplitScreenOptions, split_screen};
use crate::crates::multimedia_engine::video::trim::{TrimOptions, trim};
use crate::crates::multimedia_engine::video::xfade::{XfadeOptions, xfade};

/// Every `media.op` name [`run_media_op`] dispatches. Producers
/// (REST API, CLI) validate `op` against this list before enqueueing;
/// the job handler re-checks so hand-built payloads fail fast.
pub const MEDIA_OPS: &[&str] = &[
    "analysis.av_sync",
    "analysis.blackdetect",
    "analysis.freezedetect",
    "analysis.histogram",
    "analysis.loudness",
    "analysis.quality",
    "analysis.signalstats",
    "analysis.silence",
    "audio.channel_remix",
    "audio.channels",
    "audio.compressor",
    "audio.crossfade",
    "audio.equalizer",
    "audio.fades",
    "audio.normalize",
    "audio.remove_silence",
    "audio.resample",
    "audio.spectrogram",
    "audio.tempo",
    "audio.trim",
    "audio.volume",
    "container.chapters_edit",
    "container.select_streams",
    "container.set_language",
    "conversion.extract_audio",
    "conversion.mux_replace_audio",
    "conversion.remux",
    "conversion.transcode",
    "encoding.two_pass",
    "extraction.attachments",
    "extraction.chapters_export",
    "extraction.embedded_images",
    "extraction.list_attachments",
    "extraction.list_embedded_images",
    "extraction.stream_copy",
    "filters.apply",
    "filters.list_presets",
    "filters.presets",
    "frames.all_frames",
    "frames.frame_at",
    "frames.gif_preview",
    "frames.keyframes",
    "frames.thumbnail",
    "metadata.chapters",
    "metadata.cover_art_add",
    "metadata.cover_art_extract",
    "metadata.cover_art_remove",
    "metadata.edit_tags",
    "metadata.read_tags",
    "metadata.strip",
    "packaging.dash",
    "packaging.hls",
    "probe.chapters",
    "probe.inspect",
    "probe.streams",
    "subtitles.burn",
    "subtitles.convert",
    "subtitles.extract",
    "subtitles.list",
    "subtitles.retime",
    "video.chroma_key",
    "video.crop",
    "video.fps_convert",
    "video.freeze_frame",
    "video.letterbox",
    "video.loop",
    "video.orient",
    "video.overlay",
    "video.pad",
    "video.picture_in_picture",
    "video.pillarbox",
    "video.remove_black_borders",
    "video.resize",
    "video.reverse",
    "video.speed",
    "video.split",
    "video.split_screen",
    "video.trim",
    "video.xfade",
];

/// Result of a dispatched media op.
#[derive(Debug)]
pub enum MediaOpOutcome {
    /// The op produced one or more files (`MediaOutput::files`).
    Files(MediaOutput),
    /// The op produced a structured report, already JSON-shaped.
    Report(Value),
}

fn invalid_params(op: &str, detail: impl std::fmt::Display) -> MediaError {
    MediaError::InvalidInput(format!("media op {op}: invalid params: {detail}"))
}

/// Deserialize `params` into an options struct. `params` must be a
/// JSON object (or null/absent → `{}`).
fn op_params<T>(op: &str, params: &Value) -> Result<T, MediaError>
where
    T: DeserializeOwned,
{
    serde_json::from_value(params.clone()).map_err(|e| invalid_params(op, e))
}

/// Like [`op_params`] but merges `T::default()` under the supplied
/// params first, so callers may omit fields the struct defaults.
fn op_params_defaulted<T>(op: &str, params: &Value) -> Result<T, MediaError>
where
    T: DeserializeOwned + Serialize + Default,
{
    let mut base = serde_json::to_value(T::default())
        .map_err(|e| MediaError::Probe(format!("cannot serialize default params: {e}")))?;
    merge_json(&mut base, params);
    serde_json::from_value(base).map_err(|e| invalid_params(op, e))
}

/// Deep-merges `overlay` into `base`: objects merge key-wise,
/// anything else is replaced. Non-object overlays replace `base`.
fn merge_json(base: &mut Value, overlay: &Value) {
    if overlay.is_null() {
        return;
    }
    match (base, overlay) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(k) {
                    Some(existing) => merge_json(existing, v),
                    None => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, o) => *b = o.clone(),
    }
}

fn param_f64(op: &str, params: &Value, key: &str) -> Result<Option<f64>, MediaError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(Some)
            .ok_or_else(|| invalid_params(op, format!("params.{key} must be a number"))),
    }
}

fn param_u32(op: &str, params: &Value, key: &str) -> Result<Option<u32>, MediaError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| invalid_params(op, format!("params.{key} must be a u32"))),
    }
}

fn param_str(op: &str, params: &Value, key: &str) -> Result<Option<String>, MediaError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .map(|s| Some(s.to_string()))
            .ok_or_else(|| invalid_params(op, format!("params.{key} must be a string"))),
    }
}

fn param_bool(op: &str, params: &Value, key: &str, default: bool) -> Result<bool, MediaError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_bool()
            .ok_or_else(|| invalid_params(op, format!("params.{key} must be a bool"))),
    }
}

/// `params.output_format` as a required non-empty string.
fn req_output_format(op: &str, params: &Value) -> Result<String, MediaError> {
    param_str(op, params, "output_format")?
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| invalid_params(op, "params.output_format is required"))
}

/// `conversion.extract_audio` options — the struct is not `serde`,
/// so params are mapped field-by-field.
fn extract_audio_options(op: &str, params: &Value) -> Result<ExtractAudioOptions, MediaError> {
    Ok(ExtractAudioOptions {
        output_format: req_output_format(op, params)?,
        audio_codec: param_str(op, params, "audio_codec")?,
        bitrate: param_str(op, params, "bitrate")?,
        stream_index: param_u32(op, params, "stream_index")?,
        strip_video: param_bool(op, params, "strip_video", true)?,
    })
}

/// `video.speed` options — not `serde`; mapped field-by-field.
fn speed_options(op: &str, params: &Value) -> Result<SpeedOptions, MediaError> {
    let factor = param_f64(op, params, "factor")?
        .ok_or_else(|| invalid_params(op, "params.factor is required"))?;
    Ok(SpeedOptions {
        factor,
        preserve_pitch: param_bool(op, params, "preserve_pitch", true)?,
        output_format: param_str(op, params, "output_format")?,
    })
}

/// `frames.thumbnail` options — not `serde`; mapped field-by-field.
fn thumbnail_options(op: &str, params: &Value) -> Result<ThumbnailOptions, MediaError> {
    let quality = match param_u32(op, params, "quality")? {
        None => None,
        Some(q) => Some(
            u8::try_from(q).map_err(|_| invalid_params(op, "params.quality must be in 0..=100"))?,
        ),
    };
    Ok(ThumbnailOptions {
        at_secs: param_f64(op, params, "at_secs")?,
        width: param_u32(op, params, "width")?,
        height: param_u32(op, params, "height")?,
        output_format: param_str(op, params, "output_format")?.unwrap_or_else(|| "jpg".to_string()),
        quality,
    })
}

/// `video.orient` op selector (`params.op`, snake_case serde names).
fn orient_op_param(op: &str, params: &Value) -> Result<OrientOp, MediaError> {
    let v = params
        .get("op")
        .ok_or_else(|| invalid_params(op, "params.op is required (e.g. \"rotate_90\")"))?;
    serde_json::from_value(v.clone()).map_err(|e| invalid_params(op, e))
}

/// `audio.channels` selector: accepts the serde forms (`"ToMono"`,
/// `"ToStereo"`, `{"Custom":{...}}`) plus friendly lowercase aliases.
fn channel_op_param(op: &str, params: &Value) -> Result<ChannelOp, MediaError> {
    let v = params
        .get("channel_op")
        .or_else(|| params.get("op"))
        .ok_or_else(|| {
            invalid_params(
                op,
                "params.channel_op is required (\"to_mono\"|\"to_stereo\"|custom)",
            )
        })?;
    if let Some(s) = v.as_str() {
        match s.to_ascii_lowercase().as_str() {
            "to_mono" | "mono" => return Ok(ChannelOp::ToMono),
            "to_stereo" | "stereo" => return Ok(ChannelOp::ToStereo),
            _ => {}
        }
    }
    serde_json::from_value(v.clone()).map_err(|e| invalid_params(op, e))
}

/// Capitalizes a subtitle-format string so `"srt"` deserializes into
/// the serde variant `Srt` (the enums have no `rename_all`).
fn subtitle_format_value(v: &Value) -> Value {
    match v.as_str() {
        Some(s) => {
            let mut c = s.trim().to_string();
            if let Some(first) = c.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            Value::String(c)
        }
        None => v.clone(),
    }
}

/// `subtitles.convert` options mapped field-by-field (the option
/// struct's `SubtitleFormat` is a private-ish enum — construct via
/// serde on normalized values).
fn sub_convert_options(op: &str, params: &Value) -> Result<SubConvertOptions, MediaError> {
    let input_format = params
        .get("input_format")
        .ok_or_else(|| invalid_params(op, "params.input_format is required"))?;
    let output_format = params
        .get("output_format")
        .ok_or_else(|| invalid_params(op, "params.output_format is required"))?;
    Ok(SubConvertOptions {
        input_format: serde_json::from_value::<SubConvertFormat>(subtitle_format_value(
            input_format,
        ))
        .map_err(|e| invalid_params(op, e))?,
        output_format: serde_json::from_value::<SubConvertFormat>(subtitle_format_value(
            output_format,
        ))
        .map_err(|e| invalid_params(op, e))?,
    })
}

/// `subtitles.extract` options mapped field-by-field.
fn sub_extract_options(op: &str, params: &Value) -> Result<SubtitleOptions, MediaError> {
    let output_format = match params.get("output_format") {
        None | Some(Value::Null) => SubExtractFormat::default(),
        Some(v) => serde_json::from_value::<SubExtractFormat>(subtitle_format_value(v))
            .map_err(|e| invalid_params(op, e))?,
    };
    Ok(SubtitleOptions {
        index: param_u32(op, params, "index")?,
        output_format,
    })
}

/// `frames.gif_preview` options — the struct is not `serde`; mapped
/// field-by-field.
fn gif_preview_options(op: &str, params: &Value) -> Result<GifPreviewOptions, MediaError> {
    Ok(GifPreviewOptions {
        width: param_u32(op, params, "width")?.unwrap_or(320),
        fps: param_f64(op, params, "fps")?.unwrap_or(10.0),
        max_duration_secs: param_f64(op, params, "max_duration_secs")?,
        colors: param_u32(op, params, "colors")?,
    })
}

/// `audio.equalizer` band list (`params.bands`): an array of
/// `{frequency, gain_db, width?}` objects. Absent → empty (the op
/// validates that at least one band is required).
fn eq_bands_param(op: &str, params: &Value) -> Result<Vec<EqBand>, MediaError> {
    match params.get("bands") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| invalid_params(op, e)),
    }
}

/// `analysis.quality` metric (`params.metric`): accepts the serde
/// variant names plus lowercase aliases.
fn quality_metric_param(op: &str, params: &Value) -> Result<QualityMetric, MediaError> {
    match params.get("metric") {
        None | Some(Value::Null) => Ok(QualityMetric::default()),
        Some(v) => {
            if let Some(s) = v.as_str() {
                match s.to_ascii_lowercase().as_str() {
                    "psnr" => return Ok(QualityMetric::Psnr),
                    "ssim" => return Ok(QualityMetric::Ssim),
                    _ => {}
                }
            }
            serde_json::from_value(v.clone()).map_err(|e| invalid_params(op, e))
        }
    }
}

/// Serializes a report value; report types are `Serialize` by
/// contract (the two detection reports expose their event lists
/// manually at the call sites).
fn report_value<T: Serialize>(op: &str, report: &T) -> Result<Value, MediaError> {
    serde_json::to_value(report).map_err(|e| invalid_params(op, format!("report: {e}")))
}

/// Builds a [`MediaInput`] for the geometry ops, which take an owned
/// named input rather than a byte slice.
fn media_input(input_name: &str, input: &[u8]) -> MediaInput {
    MediaInput {
        name: input_name.to_string(),
        data: input.to_vec(),
    }
}

/// Dispatches `op` against `input`. `secondary_input` carries the
/// second byte input for ops that need one (`subtitles.burn`'s
/// subtitle file); it is `None` for every other op.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — unknown op name (message lists
///   [`MEDIA_OPS`]) or malformed `params`.
/// - Whatever the underlying op returns — [`MediaError::Unsupported`]
///   for capability-gated features, [`MediaError::MissingTool`] when
///   ffmpeg/ffprobe is absent, [`MediaError::ToolFailed`] for
///   undecodable input, etc.
pub fn run_media_op(
    op: &str,
    input_name: &str,
    input: &[u8],
    params: &Value,
    secondary_input: Option<&[u8]>,
    limits: &MediaLimits,
    rt: &Runtime,
) -> Result<MediaOpOutcome, MediaError> {
    let files = |out: MediaOutput| Ok(MediaOpOutcome::Files(out));
    match op {
        // ------------------------------------------------ probe --
        "probe.inspect" => Ok(MediaOpOutcome::Report(report_value(
            op,
            &inspect(input, limits)?,
        )?)),
        "probe.streams" => Ok(MediaOpOutcome::Report(report_value(
            op,
            &probe_streams(input, limits)?,
        )?)),
        "probe.chapters" => Ok(MediaOpOutcome::Report(report_value(
            op,
            &chapters(input, limits)?,
        )?)),
        // -------------------------------------------- analysis --
        "analysis.av_sync" => {
            let opts: AvSyncOptions = op_params(op, params)?;
            Ok(MediaOpOutcome::Report(report_value(
                op,
                &av_sync(input, &opts, limits, rt)?,
            )?))
        }
        "analysis.histogram" => {
            let opts: HistogramOptions = op_params(op, params)?;
            Ok(MediaOpOutcome::Report(report_value(
                op,
                &histogram(input, &opts, limits, rt)?,
            )?))
        }
        "analysis.quality" => {
            let distorted = secondary_input.ok_or_else(|| {
                invalid_params(op, "analysis.quality requires a second input (distorted)")
            })?;
            let metric = quality_metric_param(op, params)?;
            Ok(MediaOpOutcome::Report(report_value(
                op,
                &compare_quality(input, distorted, metric, limits, rt)?,
            )?))
        }
        // ---------------------------------------- container --
        "container.select_streams" => {
            let opts: SelectStreamsOptions = op_params_defaulted(op, params)?;
            files(select_streams(input, &opts, limits, rt)?.into_output())
        }
        "container.set_language" => {
            let opts: LanguageOptions = op_params_defaulted(op, params)?;
            files(set_language(input, &opts, limits, rt)?)
        }
        "container.chapters_edit" => {
            let opts: ChaptersEditOptions = op_params_defaulted(op, params)?;
            files(chapters_edit(input, &opts, limits, rt)?)
        }
        // ---------------------------------------- extraction --
        "extraction.attachments" => {
            let opts: AttachmentOptions = op_params_defaulted(op, params)?;
            files(extract_attachments(input, &opts, limits, rt)?.into_output())
        }
        "extraction.list_attachments" => Ok(MediaOpOutcome::Report(report_value(
            op,
            &list_attachments(input, limits, rt)?,
        )?)),
        "extraction.chapters_export" => {
            let opts: ChapterExportOptions = op_params(op, params)?;
            files(export_chapters(input, &opts, limits)?.into_output())
        }
        "extraction.embedded_images" => {
            let opts: EmbeddedImageOptions = op_params_defaulted(op, params)?;
            files(extract_embedded_images(input, &opts, limits, rt)?.into_output())
        }
        "extraction.list_embedded_images" => Ok(MediaOpOutcome::Report(report_value(
            op,
            &list_embedded_images(input, limits, rt)?,
        )?)),
        "extraction.stream_copy" => {
            let opts: StreamCopyOptions = op_params_defaulted(op, params)?;
            files(stream_copy(input, &opts, limits, rt)?)
        }
        // -------------------------------------------- metadata --
        "metadata.read_tags" => Ok(MediaOpOutcome::Report(report_value(
            op,
            &read_tags(input, limits, rt)?,
        )?)),
        "metadata.chapters" => {
            let opts: ChaptersOptions = op_params_defaulted(op, params)?;
            files(write_chapters(input, &opts, limits, rt)?)
        }
        "metadata.cover_art_add" => {
            let image = secondary_input.ok_or_else(|| {
                invalid_params(op, "metadata.cover_art_add requires an image input")
            })?;
            files(add_cover_art(input, image, limits, rt)?)
        }
        "metadata.cover_art_extract" => match extract_cover_art(input, limits, rt)? {
            Some(out) => files(out),
            None => Ok(MediaOpOutcome::Report(json!({ "cover_art": null }))),
        },
        "metadata.cover_art_remove" => files(remove_cover_art(input, limits, rt)?),
        "metadata.strip" => {
            let opts: StripOptions = op_params_defaulted(op, params)?;
            files(strip_metadata(input, &opts, limits, rt)?)
        }
        "metadata.edit_tags" => {
            let opts: TagOptions = op_params_defaulted(op, params)?;
            files(edit_tags(input, &opts, limits, rt)?)
        }
        // ------------------------------------------ subtitles --
        "subtitles.list" => Ok(MediaOpOutcome::Report(report_value(
            op,
            &list_subtitles(input, limits, rt)?,
        )?)),
        "subtitles.retime" => {
            let opts: RetimeOptions = op_params_defaulted(op, params)?;
            files(retime(input, &opts, limits, rt)?)
        }
        "subtitles.convert" => {
            let opts = sub_convert_options(op, params)?;
            files(convert_subtitles(input, &opts, limits, rt)?)
        }
        "subtitles.extract" => {
            let opts = sub_extract_options(op, params)?;
            files(extract_subtitles(input, &opts, limits, rt)?)
        }
        "subtitles.burn" => {
            let subtitles = secondary_input.ok_or_else(|| {
                invalid_params(op, "subtitles.burn requires a subtitle input (subtitle_b64 or subtitle_artifact_id)")
            })?;
            let opts: BurnOptions = op_params_defaulted(op, params)?;
            files(burn_subtitles(input, subtitles, &opts, limits, rt)?)
        }
        // ------------------------------------------- analysis --
        "analysis.loudness" => {
            let opts: LoudnessOptions = op_params_defaulted(op, params)?;
            Ok(MediaOpOutcome::Report(report_value(
                op,
                &measure_loudness(input, &opts, limits, rt)?,
            )?))
        }
        "analysis.silence" => {
            let opts: SilenceOptions = op_params_defaulted(op, params)?;
            let report = silence_detect(input, &opts, limits, rt)?;
            // SilenceReport has no Serialize impl; expose its events.
            Ok(MediaOpOutcome::Report(json!({ "events": report.events })))
        }
        "analysis.blackdetect" => {
            let opts: BlackDetectOptions = op_params_defaulted(op, params)?;
            let report = blackdetect(input, &opts, limits, rt)?;
            Ok(MediaOpOutcome::Report(json!({
                "events": report.events,
                "tool": report.tool,
            })))
        }
        "analysis.freezedetect" => {
            let opts: FreezeDetectOptions = op_params_defaulted(op, params)?;
            let report = detect_freeze(input, &opts, limits, rt)?;
            Ok(MediaOpOutcome::Report(json!({
                "events": report.events,
                "tool": report.tool,
            })))
        }
        "analysis.signalstats" => {
            let opts: SignalStatsOptions = op_params_defaulted(op, params)?;
            Ok(MediaOpOutcome::Report(report_value(
                op,
                &signal_stats(input, &opts, limits, rt)?,
            )?))
        }
        // ---------------------------------------- conversion --
        "conversion.transcode" => {
            let opts: TranscodeOptions = op_params_defaulted(op, params)?;
            files(transcode(input, &opts, limits, rt)?)
        }
        "conversion.remux" => {
            let opts: RemuxOptions = op_params_defaulted(op, params)?;
            files(remux(input, &opts, limits, rt)?)
        }
        "conversion.extract_audio" => {
            let opts = extract_audio_options(op, params)?;
            files(extract_audio(input, &opts, limits, rt)?)
        }
        "conversion.mux_replace_audio" => {
            let audio = secondary_input.ok_or_else(|| {
                invalid_params(op, "conversion.mux_replace_audio requires an audio input")
            })?;
            let opts = ReplaceAudioOptions {
                output_format: param_str(op, params, "output_format")?,
                audio_codec: param_str(op, params, "audio_codec")?,
                shortest: param_bool(op, params, "shortest", false)?,
            };
            files(replace_audio(input, audio, &opts, limits, rt)?)
        }
        // ------------------------------------------ encoding --
        "encoding.two_pass" => {
            let opts: TwoPassOptions = op_params(op, params)?;
            files(two_pass(input, &opts, limits, rt)?)
        }
        // --------------------------------------------- video --
        "video.trim" => {
            let opts: TrimOptions = op_params_defaulted(op, params)?;
            files(trim(input, &opts, limits, rt)?)
        }
        "video.freeze_frame" => {
            let opts: FreezeFrameOptions = op_params_defaulted(op, params)?;
            files(freeze_frame(input, &opts, limits, rt)?)
        }
        "video.speed" => {
            let opts = speed_options(op, params)?;
            files(crate::crates::multimedia_engine::video::speed::set_speed(
                input, &opts, limits, rt,
            )?)
        }
        "video.resize" => {
            let opts: ResizeOptions = op_params(op, params)?;
            files(resize(&media_input(input_name, input), &opts, limits, rt)?)
        }
        "video.crop" => {
            let opts: CropOptions = op_params_defaulted(op, params)?;
            files(crop(&media_input(input_name, input), &opts, limits, rt)?)
        }
        "video.pad" => {
            let opts: PadOptions = op_params(op, params)?;
            files(pad(&media_input(input_name, input), &opts, limits, rt)?)
        }
        "video.letterbox" => {
            let opts: LetterboxOptions = op_params(op, params)?;
            files(letterbox(
                &media_input(input_name, input),
                &opts,
                limits,
                rt,
            )?)
        }
        "video.pillarbox" => {
            let opts: LetterboxOptions = op_params(op, params)?;
            files(pillarbox(
                &media_input(input_name, input),
                &opts,
                limits,
                rt,
            )?)
        }
        "video.remove_black_borders" => {
            let opts: BlackBorderOptions = op_params_defaulted(op, params)?;
            files(remove_black_borders(
                &media_input(input_name, input),
                &opts,
                limits,
                rt,
            )?)
        }
        "video.orient" => {
            let o = orient_op_param(op, params)?;
            files(orient(input, o, limits, rt)?)
        }
        "video.split" => {
            let opts: SplitOptions = op_params(op, params)?;
            files(split(input, &opts, limits, rt)?)
        }
        "video.reverse" => {
            let opts: ReverseOptions = op_params(op, params)?;
            files(reverse(input, &opts, limits, rt)?)
        }
        "video.loop" => {
            let opts: LoopOptions = op_params_defaulted(op, params)?;
            files(loop_video(input, &opts, limits, rt)?)
        }
        "video.picture_in_picture" => {
            let pip = secondary_input.ok_or_else(|| {
                invalid_params(op, "video.picture_in_picture requires a pip input")
            })?;
            let opts: PictureInPictureOptions = op_params(op, params)?;
            files(picture_in_picture(input, pip, &opts, limits, rt)?)
        }
        "video.split_screen" => {
            let right = secondary_input
                .ok_or_else(|| invalid_params(op, "video.split_screen requires a second input"))?;
            let opts: SplitScreenOptions = op_params(op, params)?;
            files(split_screen(input, right, &opts, limits, rt)?)
        }
        "video.chroma_key" => {
            let opts: ChromaKeyOptions = op_params(op, params)?;
            files(chroma_key(input, &opts, limits, rt)?)
        }
        "video.xfade" => {
            let b = secondary_input
                .ok_or_else(|| invalid_params(op, "video.xfade requires a second input"))?;
            let opts: XfadeOptions = op_params(op, params)?;
            files(xfade(input, b, &opts, limits, rt)?)
        }
        "video.fps_convert" => {
            let opts: FpsOptions = op_params(op, params)?;
            files(fps_convert(input, &opts, limits, rt)?)
        }
        "video.overlay" => {
            let overlay = secondary_input.ok_or_else(|| {
                invalid_params(op, "video.overlay requires an overlay image input")
            })?;
            let opts: OverlayOptions = op_params(op, params)?;
            files(overlay_image(input, overlay, &opts, limits, rt)?)
        }
        // --------------------------------------------- audio --
        "audio.volume" => {
            let opts: VolumeOptions = op_params_defaulted(op, params)?;
            files(adjust_volume(input, &opts, limits, rt)?)
        }
        "audio.trim" => {
            let opts: AudioTrimOptions = op_params_defaulted(op, params)?;
            files(trim_audio(input, &opts, limits, rt)?)
        }
        "audio.normalize" => {
            let opts: NormalizeOptions = op_params_defaulted(op, params)?;
            files(normalize_loudness(input, &opts, limits, rt)?)
        }
        "audio.channels" => {
            let c = channel_op_param(op, params)?;
            files(channels(input, &c, limits, rt)?)
        }
        "audio.resample" => {
            let rate = param_u32(op, params, "sample_rate")?
                .ok_or_else(|| invalid_params(op, "params.sample_rate is required"))?;
            files(resample(input, rate, limits, rt)?)
        }
        "audio.fades" => {
            let opts: FadeOptions = op_params_defaulted(op, params)?;
            files(apply_fades(input, &opts, limits, rt)?)
        }
        "audio.equalizer" => {
            let bands = eq_bands_param(op, params)?;
            let opts: EqOptions = op_params_defaulted(op, params)?;
            files(equalizer(input, &bands, &opts, limits, rt)?)
        }
        "audio.compressor" => {
            let opts: CompressorOptions = op_params(op, params)?;
            files(compress(input, &opts, limits, rt)?)
        }
        "audio.channel_remix" => {
            let opts: ChannelRemixOptions = op_params_defaulted(op, params)?;
            files(remix_channels(input, &opts, limits, rt)?)
        }
        "audio.tempo" => {
            let opts: TempoOptions = op_params(op, params)?;
            files(tempo(input, &opts, limits, rt)?)
        }
        "audio.spectrogram" => {
            let opts: SpectrogramOptions = op_params(op, params)?;
            files(render_spectrogram(input, &opts, limits, rt)?)
        }
        "audio.remove_silence" => {
            let opts: RemoveSilenceOptions = op_params(op, params)?;
            files(remove_silence(input, &opts, limits, rt)?)
        }
        "audio.crossfade" => {
            let b = secondary_input
                .ok_or_else(|| invalid_params(op, "audio.crossfade requires a second input"))?;
            let opts: CrossfadeOptions = op_params(op, params)?;
            files(crossfade(input, b, &opts, limits, rt)?)
        }
        // --------------------------------------------- frames --
        "frames.thumbnail" => {
            let opts = thumbnail_options(op, params)?;
            files(thumbnail(input, &opts, limits, rt)?)
        }
        "frames.frame_at" => {
            let opts: FrameOptions = op_params_defaulted(op, params)?;
            files(frame_at(input, &opts, limits, rt)?)
        }
        "frames.keyframes" => {
            let opts: KeyframeOptions = op_params_defaulted(op, params)?;
            files(keyframes(input, &opts, limits, rt)?)
        }
        "frames.all_frames" => {
            let opts: AllFramesOptions = op_params(op, params)?;
            files(all_frames(input, &opts, limits, rt)?)
        }
        "frames.gif_preview" => {
            let opts = gif_preview_options(op, params)?;
            files(gif_preview(input, &opts, limits, rt)?)
        }
        // ------------------------------------------ packaging --
        "packaging.hls" => {
            let opts: HlsOptions = op_params_defaulted(op, params)?;
            files(hls_package(input, &opts, limits, rt)?)
        }
        "packaging.dash" => {
            let opts: DashOptions = op_params_defaulted(op, params)?;
            files(dash_package(input, &opts, limits, rt)?)
        }
        // -------------------------------------------- filters --
        "filters.apply" => {
            let opts: FilterGraphOptions = op_params_defaulted(op, params)?;
            files(apply_filters(input, &opts, limits, rt)?)
        }
        "filters.presets" => {
            let opts: PresetOptions = op_params(op, params)?;
            files(apply_preset(input, &opts, limits, rt)?)
        }
        "filters.list_presets" => Ok(MediaOpOutcome::Report(json!({
            "presets": list_presets(),
        }))),
        _ => Err(MediaError::InvalidInput(format!(
            "unknown media op {op:?}; supported ops: {}",
            MEDIA_OPS.join(", ")
        ))),
    }
}
