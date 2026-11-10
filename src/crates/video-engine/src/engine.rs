//! [`VideoEngine`] — the `ProcessingEngine` implementation backed by
//! the external `ffmpeg`/`ffprobe` binaries.
//!
//! All operations are synchronous, subprocess-bound functions over
//! in-memory byte buffers (see the `ops` module). They are exposed as
//! plain methods rather than futures: callers running them on an async
//! runtime should wrap them in `spawn_blocking` themselves. The engine
//! holds no state — every call receives its own
//! [`EngineLimits`](crate::crates::core::engine::EngineLimits) — so it
//! is trivially `Send + Sync`.
//!
//! # External tools
//!
//! [`ProcessingEngine::health`] probes `PATH` for `ffmpeg` and
//! `ffprobe`: both present → [`EngineHealthStatus::Healthy`], exactly
//! one → `Degraded`, neither → `Unavailable`. Operations that need a
//! missing binary fail with [`VideoError::MissingTool`] — results are
//! never approximated.

use std::collections::BTreeMap;
use std::path::Path;

use crate::crates::core::engine::{
    EngineCapabilities, EngineFuture, EngineHealth, EngineHealthStatus, EngineKind, EngineLimits,
    EngineOperation, ProcessingEngine,
};

use crate::crates::video_engine::error::VideoResult;
use crate::crates::video_engine::model::{
    CaptureDevice, EncodeProfile, FramesSpec, LoudnessReport, MediaAnalysis, MediaOutput,
    QualityReport, StreamFilter, StreamSelection, SubtitleStyle, VideoInfo,
};
use crate::crates::video_engine::ops;
use crate::crates::video_engine::ops::audio_time::{AudioSpeedParams, ChannelLayout, LoopParams};
use crate::crates::video_engine::ops::av_visual::VisualStyle;
use crate::crates::video_engine::ops::capture::{
    CaptureFormat, CaptureInputOpts, CaptureOutputOpts, CaptureSource,
};
use crate::crates::video_engine::ops::composite::{
    PictureInPictureParams, SideBySideParams, SplitScreenParams,
};
use crate::crates::video_engine::ops::enhance::{
    DeinterlaceMethod, DenoiseParams, FpsConvertParams, SharpenParams, UpscaleParams,
};
use crate::crates::video_engine::ops::hdr::HdrToneMapParams;
use crate::crates::video_engine::ops::interpolate::InterpolateParams;
use crate::crates::video_engine::ops::live::{StreamCapabilityReport, StreamPushOpts};
use crate::crates::video_engine::ops::metadata_export::ExportFormat;
use crate::crates::video_engine::ops::preview::{PreviewFormat, PreviewParams};
use crate::crates::video_engine::ops::proxy::ProxyParams;
use crate::crates::video_engine::ops::stabilize::StabilizeParams;
use crate::crates::video_engine::ops::sync::SyncParams;
use crate::crates::video_engine::ops::waveform::WaveformParams;
use crate::crates::video_engine::ops::{FFMPEG, FFPROBE, tool_available};
use crate::crates::video_engine::runtime;

/// Media containers this engine accepts as input and can produce as
/// output (the transcode/trim/concat target set).
const MEDIA_FORMATS: &[&str] = &["mp4", "mkv", "mov", "webm", "avi"];

/// The ffmpeg-backed video engine.
///
/// Stateless: construct with [`VideoEngine::new`] (or
/// [`Default`]) and pass per-call [`EngineLimits`] to each operation.
#[derive(Debug, Clone, Copy, Default)]
pub struct VideoEngine;

impl VideoEngine {
    /// Create a video engine. Holds no configuration — limits are
    /// supplied per operation.
    pub fn new() -> Self {
        Self
    }

    /// Inspect `input` via ffprobe: duration, dimensions, codecs,
    /// frame rate, format name, size, audio presence.
    ///
    /// See [`ops::probe`]. No limits parameter — inspection is cheap
    /// and bounded by the engine's hard input-size cap.
    pub fn probe(&self, input: &[u8]) -> VideoResult<VideoInfo> {
        ops::probe::probe(input)
    }

    /// Extract a PNG thumbnail at `at_secs` (`None` → ~10% of duration,
    /// fallback 1s).
    ///
    /// `limits.max_pixels` bounds the probed frame size;
    /// `limits.max_duration_secs`/`max_input_bytes` bound the input.
    /// See [`ops::thumbnail`].
    pub fn thumbnail(
        &self,
        input: &[u8],
        at_secs: Option<f64>,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::thumbnail::thumbnail(input, at_secs, limits)
    }

    /// Cut `input` to `[start_secs, end_secs)` (`None` → to end).
    ///
    /// `reencode = false` stream-copies; `true` re-encodes H.264/AAC
    /// for frame accuracy. Output keeps the input's container family.
    /// See [`ops::trim`].
    pub fn trim(
        &self,
        input: &[u8],
        start_secs: f64,
        end_secs: Option<f64>,
        reencode: bool,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::trim::trim(input, start_secs, end_secs, reencode, limits)
    }

    /// Convert `input` to the container named by `target_ext`
    /// (`mp4`, `mkv`, `webm`, `avi`). See [`ops::transcode`].
    pub fn transcode(
        &self,
        input: &[u8],
        target_ext: &str,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::transcode::transcode(input, target_ext, limits)
    }

    /// Concatenate `inputs` into `target_ext`. See [`ops::concat`].
    pub fn concat(
        &self,
        inputs: &[Vec<u8>],
        target_ext: &str,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::concat::concat(inputs, target_ext, limits)
    }

    /// Extract the audio track into `target_ext`
    /// (`mp3`, `wav`, `aac`, `m4a`, `ogg`). See [`ops::extract_audio`].
    pub fn extract_audio(
        &self,
        input: &[u8],
        target_ext: &str,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::extract_audio::extract_audio(input, target_ext, limits)
    }

    // ------------------------------------------------------------------
    // Containers / remux / encode profiles
    // ------------------------------------------------------------------

    /// Remux `input` into `target_ext` copying all streams
    /// (`-c copy`). See [`ops::remux`].
    pub fn remux(
        &self,
        input: &[u8],
        target_ext: &str,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::remux::remux(input, target_ext, limits)
    }

    /// Transcode `input` with an explicit [`EncodeProfile`]
    /// (codec/quality knobs). See [`ops::encode`].
    pub fn transcode_profile(
        &self,
        input: &[u8],
        profile: &EncodeProfile,
        target_ext: &str,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::encode::transcode_profile(input, profile, target_ext, limits)
    }

    /// Named encode presets this build can honor (encoders verified
    /// at call time). See [`ops::encode::preset_profiles`].
    pub fn encode_presets(&self) -> BTreeMap<&'static str, EncodeProfile> {
        ops::encode::preset_profiles()
    }

    // ------------------------------------------------------------------
    // Geometry
    // ------------------------------------------------------------------

    /// Resize `input` per [`crate::crates::video_engine::model::FitMode`].
    pub fn resize_video(
        &self,
        input: &[u8],
        width: u32,
        height: u32,
        fit: crate::crates::video_engine::model::FitMode,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::geometry::resize(input, width, height, fit, limits)
    }

    /// Crop `input` to `(x, y, w, h)`.
    pub fn crop_video(
        &self,
        input: &[u8],
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::geometry::crop(input, x, y, w, h, limits)
    }

    /// Pad `input` with `color` by the given pixel amounts.
    #[allow(clippy::too_many_arguments)]
    pub fn pad_video(
        &self,
        input: &[u8],
        left: u32,
        right: u32,
        top: u32,
        bottom: u32,
        color: Option<&str>,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::geometry::pad(input, left, right, top, bottom, color, limits)
    }

    /// Rotate `input` by `degrees`.
    pub fn rotate_video(
        &self,
        input: &[u8],
        degrees: f64,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::geometry::rotate(input, degrees, limits)
    }

    /// Mirror `input` (`horizontal` selects hflip vs vflip).
    pub fn flip_video(
        &self,
        input: &[u8],
        horizontal: bool,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::geometry::flip(input, horizontal, limits)
    }

    /// Letterbox `input` into `width`×`height`.
    pub fn letterbox(
        &self,
        input: &[u8],
        width: u32,
        height: u32,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::geometry_adv::letterbox(input, width, height, limits)
    }

    /// Pillarbox `input` into `width`×`height`.
    pub fn pillarbox(
        &self,
        input: &[u8],
        width: u32,
        height: u32,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::geometry_adv::pillarbox(input, width, height, limits)
    }

    /// Auto-detect and crop black borders.
    pub fn autocrop(&self, input: &[u8], limits: &EngineLimits) -> VideoResult<Vec<u8>> {
        ops::geometry_adv::autocrop(input, limits)
    }

    /// 90° transpose (`clockwise` selects direction).
    pub fn transpose(
        &self,
        input: &[u8],
        clockwise: bool,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::geometry_adv::transpose(input, clockwise, limits)
    }

    /// Force portrait/landscape orientation.
    pub fn orient(
        &self,
        input: &[u8],
        landscape: bool,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::geometry_adv::orient(input, landscape, limits)
    }

    // ------------------------------------------------------------------
    // Tempo / time ops
    // ------------------------------------------------------------------

    /// Change playback speed by `factor`.
    pub fn set_speed(
        &self,
        input: &[u8],
        factor: f64,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::tempo::set_speed(input, factor, limits)
    }

    /// Reverse the clip.
    pub fn reverse(&self, input: &[u8], limits: &EngineLimits) -> VideoResult<Vec<u8>> {
        ops::tempo::reverse(input, limits)
    }

    /// Loop `input` `count` additional times.
    pub fn loop_clip(
        &self,
        input: &[u8],
        count: u32,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::tempo::loop_clip(input, count, limits)
    }

    /// Freeze the frame at `at_secs` for `duration_secs`.
    pub fn freeze_frame(
        &self,
        input: &[u8],
        at_secs: f64,
        duration_secs: f64,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::tempo::freeze_frame(input, at_secs, duration_secs, limits)
    }

    // ------------------------------------------------------------------
    // Audio processing
    // ------------------------------------------------------------------

    /// Apply gain in dB to all audio.
    pub fn audio_volume(
        &self,
        input: &[u8],
        gain_db: f64,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::audio::audio_volume(input, gain_db, limits)
    }

    /// EBU R128 loudness normalization.
    pub fn audio_normalize(
        &self,
        input: &[u8],
        target_lufs: Option<f64>,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::audio::audio_normalize(input, target_lufs, limits)
    }

    /// Mix audio-bearing inputs into one file.
    pub fn audio_merge(&self, inputs: &[Vec<u8>], limits: &EngineLimits) -> VideoResult<Vec<u8>> {
        ops::audio::audio_merge(inputs, limits)
    }

    /// Concatenate audio inputs into `target_ext`.
    pub fn audio_concat(
        &self,
        inputs: &[Vec<u8>],
        target_ext: &str,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::audio::audio_concat(inputs, target_ext, limits)
    }

    /// Fade in/out.
    pub fn audio_fade(
        &self,
        input: &[u8],
        fade_in_secs: f64,
        fade_out_secs: f64,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::audio::audio_fade(input, fade_in_secs, fade_out_secs, limits)
    }

    /// Strip all audio streams.
    pub fn remove_audio(&self, input: &[u8], limits: &EngineLimits) -> VideoResult<Vec<u8>> {
        ops::audio::remove_audio(input, limits)
    }

    /// Convert channel count/layout.
    pub fn channel_convert(
        &self,
        input: &[u8],
        channels: u32,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::audio::channel_convert(input, channels, limits)
    }

    /// 3-band EQ.
    pub fn audio_eq(
        &self,
        input: &[u8],
        bass_db: f64,
        mid_db: Option<f64>,
        treble_db: f64,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::audio_fx::audio_eq(input, bass_db, mid_db, treble_db, limits)
    }

    /// Spectral noise reduction.
    pub fn audio_denoise(
        &self,
        input: &[u8],
        strength: f64,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::audio_fx::audio_denoise(input, strength, limits)
    }

    /// Dynamic-range compression.
    pub fn audio_compress(
        &self,
        input: &[u8],
        threshold_db: f64,
        ratio: f64,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::audio_fx::audio_compress(input, threshold_db, ratio, limits)
    }

    /// Pitch shift in semitones (duration preserved).
    pub fn audio_pitch(
        &self,
        input: &[u8],
        semitones: f64,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::audio_fx::audio_pitch(input, semitones, limits)
    }

    /// Render a spectrogram PNG of the audio.
    pub fn audio_spectrogram(
        &self,
        input: &[u8],
        width: u32,
        height: u32,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::audio_fx::audio_spectrogram(input, width, height, limits)
    }

    /// Weighted multi-track mix.
    pub fn audio_mix_weighted(
        &self,
        inputs: &[Vec<u8>],
        weights: &[f64],
        normalize: bool,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::audio_mix::audio_mix_weighted(inputs, weights, normalize, limits)
    }

    /// Crossfade between inputs.
    pub fn audio_crossfade(
        &self,
        inputs: &[Vec<u8>],
        duration_secs: f64,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::audio_mix::audio_crossfade(inputs, duration_secs, limits)
    }

    /// Delay the audio track by `offset_ms`.
    pub fn audio_delay(
        &self,
        input: &[u8],
        offset_ms: i64,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::audio_mix::audio_delay(input, offset_ms, limits)
    }

    // ------------------------------------------------------------------
    // Frames / images
    // ------------------------------------------------------------------

    /// Extract frames per [`FramesSpec`] as named files.
    pub fn extract_frames(
        &self,
        input: &[u8],
        spec: &FramesSpec,
        limits: &EngineLimits,
    ) -> VideoResult<MediaOutput> {
        ops::frames::extract_frames(input, spec, limits)
    }

    /// Extract every frame as a numbered image sequence.
    pub fn video_to_image_sequence(
        &self,
        input: &[u8],
        format: &str,
        limits: &EngineLimits,
    ) -> VideoResult<MediaOutput> {
        ops::frames::video_to_image_sequence(input, format, limits)
    }

    /// Build a contact-sheet PNG.
    #[allow(clippy::too_many_arguments)]
    pub fn contact_sheet(
        &self,
        input: &[u8],
        cols: u32,
        rows: u32,
        thumb_width: u32,
        interval_secs: Option<f64>,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::contact_sheet::contact_sheet(input, cols, rows, thumb_width, interval_secs, limits)
    }

    /// Extract one frame per detected scene change.
    pub fn scene_keyframes(
        &self,
        input: &[u8],
        threshold: f64,
        limits: &EngineLimits,
    ) -> VideoResult<MediaOutput> {
        ops::contact_sheet::scene_keyframes(input, threshold, limits)
    }

    /// Images → video.
    #[allow(clippy::too_many_arguments)]
    pub fn images_to_video(
        &self,
        inputs: &[Vec<u8>],
        fps: f64,
        duration_per_image: Option<f64>,
        target_ext: &str,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::slideshow::images_to_video(inputs, fps, duration_per_image, target_ext, limits)
    }

    /// Video → animated GIF.
    pub fn video_to_gif(
        &self,
        input: &[u8],
        fps: Option<f64>,
        width: Option<u32>,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::slideshow::video_to_gif(input, fps, width, limits)
    }

    /// Video → animated WebP.
    pub fn video_to_animated_webp(
        &self,
        input: &[u8],
        fps: Option<f64>,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::slideshow::video_to_animated_webp(input, fps, limits)
    }

    /// Video → APNG.
    pub fn video_to_apng(
        &self,
        input: &[u8],
        fps: Option<f64>,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::slideshow::video_to_apng(input, fps, limits)
    }

    /// Audio → video with waveform/spectrum visualization.
    pub fn audio_to_video(
        &self,
        input: &[u8],
        style: VisualStyle,
        width: u32,
        height: u32,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::av_visual::audio_to_video(input, style, width, height, limits)
    }

    /// GIF → video container.
    pub fn gif_to_video(
        &self,
        input: &[u8],
        target_ext: &str,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::av_visual::gif_to_video(input, target_ext, limits)
    }

    /// Replace/insert the audio track without re-encoding video.
    pub fn replace_audio_track(
        &self,
        video: &[u8],
        audio: &[u8],
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::av_visual::replace_audio_track(video, audio, limits)
    }

    // ------------------------------------------------------------------
    // Extraction / subtitles
    // ------------------------------------------------------------------

    /// Extract embedded streams (subs, attachments, cover, chapters,
    /// data) as named files.
    pub fn extract_streams(
        &self,
        input: &[u8],
        selection: &StreamSelection,
        subtitle_format: &str,
        limits: &EngineLimits,
    ) -> VideoResult<MediaOutput> {
        ops::extract::extract_streams(input, selection, subtitle_format, limits)
    }

    /// Extract one subtitle track as text.
    pub fn extract_subtitles(
        &self,
        input: &[u8],
        track: Option<u32>,
        format: &str,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::subtitles::extract_subtitles(input, track, format, limits)
    }

    /// Convert subtitle text between srt/vtt/ass.
    pub fn convert_subtitles(
        &self,
        subtitle_bytes: &[u8],
        from_format: Option<&str>,
        to_format: &str,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::subtitles::convert_subtitles(subtitle_bytes, from_format, to_format, limits)
    }

    /// Shift subtitle cue times by `offset_secs`.
    pub fn shift_subtitles(
        &self,
        subtitle_bytes: &[u8],
        offset_secs: f64,
        format: &str,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::subtitles::shift_subtitles(subtitle_bytes, offset_secs, format, limits)
    }

    /// Burn subtitles into the video.
    pub fn burn_subtitles(
        &self,
        input: &[u8],
        subtitle_bytes: &[u8],
        style: Option<&SubtitleStyle>,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::subtitles::burn_subtitles(input, subtitle_bytes, style, limits)
    }

    // ------------------------------------------------------------------
    // Filters / overlays / stream ops
    // ------------------------------------------------------------------

    /// Apply raw ffmpeg `-vf`/`-af` filters.
    pub fn apply_filters(
        &self,
        input: &[u8],
        video_filter: Option<&str>,
        audio_filter: Option<&str>,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::filters::apply_filters(input, video_filter, audio_filter, limits)
    }

    /// Named filter presets verified against this build.
    pub fn filter_presets(&self) -> Vec<ops::filters::FilterPreset> {
        ops::filters::filter_presets()
    }

    /// Overlay an image onto the video.
    #[allow(clippy::too_many_arguments)]
    pub fn overlay_image(
        &self,
        input: &[u8],
        overlay: &[u8],
        x: i32,
        y: i32,
        opacity: Option<f64>,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::overlay::overlay_image(input, overlay, x, y, opacity, limits)
    }

    /// Draw text onto the video.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_text(
        &self,
        input: &[u8],
        text: &str,
        x: i32,
        y: i32,
        font_size: u32,
        color: Option<&str>,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::overlay::draw_text(input, text, x, y, font_size, color, limits)
    }

    /// Chroma-key composite foreground over background.
    pub fn chroma_key(
        &self,
        foreground: &[u8],
        background: &[u8],
        color: &str,
        similarity: f64,
        blend: f64,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::overlay::chroma_key(foreground, background, color, similarity, blend, limits)
    }

    /// Select/map streams into `target_ext`.
    pub fn select_streams(
        &self,
        input: &[u8],
        filter: &StreamFilter,
        target_ext: &str,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::stream_ops::select_streams(input, filter, target_ext, limits)
    }

    /// Merge an audio track into a video.
    pub fn merge_av(
        &self,
        video: &[u8],
        audio: &[u8],
        target_ext: &str,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::stream_ops::merge_av(video, audio, target_ext, limits)
    }

    /// Set container metadata tags.
    pub fn set_metadata(
        &self,
        input: &[u8],
        tags: &BTreeMap<String, String>,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::stream_ops::set_metadata(input, tags, limits)
    }

    /// Strip all metadata.
    pub fn strip_metadata(
        &self,
        input: &[u8],
        target_ext: &str,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::stream_ops::strip_metadata(input, target_ext, limits)
    }

    /// Attach/replace cover art (audio containers only).
    pub fn set_cover_art(
        &self,
        input: &[u8],
        cover: &[u8],
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::stream_ops::set_cover_art(input, cover, limits)
    }

    // ------------------------------------------------------------------
    // Analysis / loudness / quality
    // ------------------------------------------------------------------

    /// Full analysis report (black/freeze/silence/scene/metrics).
    pub fn analyze(&self, input: &[u8], limits: &EngineLimits) -> VideoResult<MediaAnalysis> {
        ops::analysis::analyze(input, limits)
    }

    /// Scene-change timestamps `(time, score)`.
    pub fn scene_timestamps(
        &self,
        input: &[u8],
        threshold: f64,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<(f64, f64)>> {
        ops::analysis::scene_timestamps(input, threshold, limits)
    }

    /// Black-frame intervals.
    pub fn detect_black_frames(
        &self,
        input: &[u8],
        limits: &EngineLimits,
    ) -> VideoResult<Vec<(f64, f64)>> {
        ops::analysis::detect_black_frames(input, limits)
    }

    /// Freeze-frame intervals.
    pub fn detect_freeze_frames(
        &self,
        input: &[u8],
        limits: &EngineLimits,
    ) -> VideoResult<Vec<(f64, f64)>> {
        ops::analysis::detect_freeze_frames(input, limits)
    }

    /// Silence intervals.
    pub fn detect_silence(
        &self,
        input: &[u8],
        limits: &EngineLimits,
    ) -> VideoResult<Vec<(f64, f64)>> {
        ops::analysis::detect_silence(input, limits)
    }

    /// EBU R128 loudness measurement.
    pub fn measure_loudness(
        &self,
        input: &[u8],
        limits: &EngineLimits,
    ) -> VideoResult<LoudnessReport> {
        ops::loudness::measure_loudness(input, limits)
    }

    /// Loudness normalization.
    pub fn normalize_loudness(
        &self,
        input: &[u8],
        target_lufs: Option<f64>,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::loudness::normalize_loudness(input, target_lufs, limits)
    }

    /// Audio statistics map.
    pub fn audio_stats(
        &self,
        input: &[u8],
        limits: &EngineLimits,
    ) -> VideoResult<BTreeMap<String, f64>> {
        ops::loudness::audio_stats(input, limits)
    }

    /// Objective quality comparison (psnr/ssim/vmaf).
    pub fn compare_quality(
        &self,
        reference: &[u8],
        distorted: &[u8],
        metrics: &[&str],
        limits: &EngineLimits,
    ) -> VideoResult<QualityReport> {
        ops::quality::compare_quality(reference, distorted, metrics, limits)
    }

    // ------------------------------------------------------------------
    // Packaging / capture / runtime
    // ------------------------------------------------------------------

    /// HLS package (`index.m3u8` + segments).
    pub fn package_hls(&self, input: &[u8], limits: &EngineLimits) -> VideoResult<MediaOutput> {
        ops::packaging::package_hls(input, limits)
    }

    /// MPEG-DASH package (`manifest.mpd` + segments).
    pub fn package_dash(&self, input: &[u8], limits: &EngineLimits) -> VideoResult<MediaOutput> {
        ops::packaging::package_dash(input, limits)
    }

    /// Faststart MP4 remux.
    pub fn faststart(&self, input: &[u8], limits: &EngineLimits) -> VideoResult<Vec<u8>> {
        ops::packaging::faststart(input, limits)
    }

    /// Enumerate capture devices/sources.
    pub fn list_devices(&self) -> VideoResult<Vec<CaptureDevice>> {
        ops::capture::list_devices()
    }

    /// List input/output protocols.
    pub fn list_protocols(&self) -> VideoResult<Vec<CaptureDevice>> {
        ops::capture::list_protocols()
    }

    /// List hardware accelerations.
    pub fn list_hwaccels(&self) -> VideoResult<Vec<CaptureDevice>> {
        ops::capture::list_hwaccels()
    }

    /// Best-guess usable hwaccel on this host (`None` → software).
    pub fn detect_hwaccel(&self) -> Option<String> {
        ops::capture::detect_hwaccel()
    }

    /// Validate a capture device string for `format` without building
    /// arguments. Thin wrapper over the same grammar
    /// [`capture_input_args`] enforces; returns
    /// [`VideoError::InvalidInput`] on malformed specs.
    pub fn validate_capture_device(&self, format: CaptureFormat, device: &str) -> VideoResult<()> {
        let source = CaptureSource {
            format,
            device: device.to_string(),
            audio_device: None,
            audio_format: None,
        };
        // capture_args validates the device before emitting options;
        // default input opts contribute no additional constraints.
        ops::capture::capture_args(&source, &CaptureInputOpts::default()).map(|_| ())
    }

    /// Input-side ffmpeg arguments for a capture `source` +
    /// `in_opts` (`-f <fmt> <input opts> -i <device> ...`). Pure
    /// builder — nothing is executed. See [`ops::capture::capture_args`].
    pub fn capture_input_args(
        &self,
        source: &CaptureSource,
        in_opts: &CaptureInputOpts,
    ) -> VideoResult<Vec<String>> {
        ops::capture::capture_args(source, in_opts)
    }

    /// Output-side ffmpeg arguments for capture `out_opts`
    /// (`-t`/`-s`/`-r`/codec selection). Pure builder — nothing is
    /// executed. See [`ops::capture::capture_output_args`].
    pub fn capture_output_args(&self, out_opts: &CaptureOutputOpts) -> VideoResult<Vec<String>> {
        ops::capture::capture_output_args(out_opts)
    }

    /// Full ffmpeg argv for a capture session:
    /// `["-hide_banner", "-y", <input args>, <output args>,
    /// <output_path>]`. Pure builder — the caller decides whether and
    /// where to execute it. See [`ops::capture::capture_argv`].
    pub fn capture_argv(
        &self,
        source: &CaptureSource,
        in_opts: &CaptureInputOpts,
        out_opts: &CaptureOutputOpts,
        output_path: &str,
    ) -> VideoResult<Vec<String>> {
        ops::capture::capture_argv(source, in_opts, out_opts, output_path)
    }

    // ------------------------------------------------------------------
    // Live streaming (safe surface: no method pushes a stream)
    // ------------------------------------------------------------------

    /// Stream protocols this engine permits pushing to — the static
    /// whitelist enforced by [`validate_stream_url`] (rtmp/rtsp/srt/
    /// udp/rtp/http/https/icecast families). Does not probe ffmpeg;
    /// use [`stream_targets`] for what this build actually supports.
    pub fn stream_protocols(&self) -> &'static [&'static str] {
        ops::live::STREAM_PROTOCOLS
    }

    /// Stream output protocols this ffmpeg build actually supports:
    /// the whitelist ∩ output-capable protocols from
    /// `ffmpeg -protocols`. Sorted, lowercase. Errors propagate
    /// honestly (`MissingTool`/`ExternalTool`).
    pub fn stream_targets(&self) -> VideoResult<Vec<String>> {
        ops::live::stream_targets()
    }

    /// Per-protocol input/output capability plus the muxer list for
    /// this ffmpeg build. Read-only; safe for health/capability
    /// probes. See [`ops::live::stream_capability_report`].
    pub fn stream_capabilities(&self) -> VideoResult<StreamCapabilityReport> {
        ops::live::stream_capability_report()
    }

    /// Validate a stream destination URL for [`stream_args`]: scheme
    /// must be whitelisted, `scheme://authority` form with non-empty
    /// authority, no dangerous characters. Returns `()` or
    /// [`VideoError::InvalidInput`].
    pub fn validate_stream_url(&self, url: &str) -> VideoResult<()> {
        ops::live::stream_validate_url(url)
    }

    /// Build the ffmpeg argument vector for pushing `input_path` to
    /// `url` — a **pure builder** that validates inputs and returns
    /// argv (`ffmpeg -re -i <input> <codec args> <url>`); it never
    /// opens a socket or executes anything. Executing the push is the
    /// caller/job layer's responsibility. See
    /// [`ops::live::stream_args`].
    pub fn stream_args(
        &self,
        input_path: &Path,
        url: &str,
        opts: &StreamPushOpts,
    ) -> VideoResult<Vec<String>> {
        ops::live::stream_args(input_path, url, opts)
    }

    /// Full capability snapshot of the installed ffmpeg build.
    pub fn media_capabilities(&self) -> VideoResult<runtime::MediaCapabilities> {
        runtime::capabilities()
    }

    /// `ffmpeg -version` first line.
    pub fn ffmpeg_version(&self) -> VideoResult<String> {
        runtime::ffmpeg_version()
    }

    /// `ffprobe -version` first line.
    pub fn ffprobe_version(&self) -> VideoResult<String> {
        runtime::ffprobe_version()
    }

    // ------------------------------------------------------------------
    // Audio time ops
    // ------------------------------------------------------------------

    /// Reverse the audio track (`areverse`).
    pub fn audio_reverse(&self, input: &[u8], limits: &EngineLimits) -> VideoResult<Vec<u8>> {
        ops::audio_time::audio_reverse(input, limits)
    }

    /// Repeat the audio `params.count` additional times.
    pub fn audio_loop(
        &self,
        input: &[u8],
        params: LoopParams,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::audio_time::audio_loop(input, params, limits)
    }

    /// Change audio tempo by `params.factor` without pitch shift.
    pub fn audio_speed(
        &self,
        input: &[u8],
        params: AudioSpeedParams,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::audio_time::audio_speed(input, params, limits)
    }

    /// Trim the audio to `[start_ms, end_ms)` into `format`.
    pub fn audio_trim(
        &self,
        input: &[u8],
        start_ms: u64,
        end_ms: u64,
        format: &str,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::audio_time::audio_trim(input, start_ms, end_ms, format, limits)
    }

    /// Convert the audio channel layout to `layout`.
    pub fn audio_channels(
        &self,
        input: &[u8],
        layout: &ChannelLayout,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::audio_time::audio_channels(input, layout, limits)
    }

    /// Transcode the audio stream to `target_format`
    /// (`mp3`, `wav`, `flac`, `ogg`, `opus`, `m4a`, `aac`).
    pub fn audio_convert(
        &self,
        input: &[u8],
        target_format: &str,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::audio_time::audio_convert(input, target_format, limits)
    }

    // ------------------------------------------------------------------
    // Chapters
    // ------------------------------------------------------------------

    /// List embedded chapters (millisecond bounds).
    pub fn chapters_list(&self, input: &[u8]) -> VideoResult<Vec<ops::chapters::ChapterInfo>> {
        ops::chapters::chapters_list(input)
    }

    /// Replace the chapter table with `chapters`
    /// (`(start_ms, end_ms, title)`).
    pub fn chapters_set(
        &self,
        input: &[u8],
        chapters: &[(i64, i64, &str)],
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::chapters::chapters_set(input, chapters, limits)
    }

    /// Remove every embedded chapter.
    pub fn chapters_remove(&self, input: &[u8], limits: &EngineLimits) -> VideoResult<Vec<u8>> {
        ops::chapters::chapters_remove(input, limits)
    }

    /// Shift every chapter by `offset_ms` milliseconds.
    pub fn chapters_shift(
        &self,
        input: &[u8],
        offset_ms: i64,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::chapters::chapters_shift(input, offset_ms, limits)
    }

    // ------------------------------------------------------------------
    // Compositing
    // ------------------------------------------------------------------

    /// Composite `params.overlay` onto `params.base` as picture-in-picture.
    pub fn picture_in_picture(
        &self,
        params: &PictureInPictureParams,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::composite::picture_in_picture(params, limits)
    }

    /// Stack `params.left`/`params.right` per `params.orientation`.
    pub fn side_by_side(
        &self,
        params: &SideBySideParams,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::composite::side_by_side(params, limits)
    }

    /// Stack `params.top` over `params.bottom` (equal widths required).
    pub fn split_screen(
        &self,
        params: &SplitScreenParams,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::composite::split_screen(params, limits)
    }

    // ------------------------------------------------------------------
    // Enhancement / HDR / frame-rate
    // ------------------------------------------------------------------

    /// Spatial/temporal denoise per `params`.
    pub fn denoise(
        &self,
        input: &[u8],
        params: DenoiseParams,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::enhance::denoise(input, params, limits)
    }

    /// Sharpen per `params`.
    pub fn sharpen(
        &self,
        input: &[u8],
        params: SharpenParams,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::enhance::sharpen(input, params, limits)
    }

    /// Deinterlace using `method`.
    pub fn deinterlace(
        &self,
        input: &[u8],
        method: DeinterlaceMethod,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::enhance::deinterlace(input, method, limits)
    }

    /// Convert to a constant frame rate per `params`.
    pub fn fps_convert(
        &self,
        input: &[u8],
        params: FpsConvertParams,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::enhance::fps_convert(input, params, limits)
    }

    /// Upscale per `params` algorithm/scale.
    pub fn upscale(
        &self,
        input: &[u8],
        params: UpscaleParams,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::enhance::upscale(input, params, limits)
    }

    /// Detect HDR characteristics of `input`.
    pub fn hdr_detect(&self, input: &[u8]) -> VideoResult<ops::hdr::HdrInfo> {
        ops::hdr::hdr_detect(input)
    }

    /// Tone-map HDR `input` per `params`.
    pub fn hdr_tone_map(
        &self,
        input: &[u8],
        params: &HdrToneMapParams,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::hdr::hdr_tone_map(input, params, limits)
    }

    /// Frame-rate conversion / interpolation per `params`.
    pub fn interpolate_video(
        &self,
        input: &[u8],
        params: &InterpolateParams,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::interpolate::interpolate_video(input, params, limits)
    }

    // ------------------------------------------------------------------
    // Proxy / segmentation / sync
    // ------------------------------------------------------------------

    /// Generate a low-res proxy per `params`.
    pub fn generate_proxy(
        &self,
        input: &[u8],
        params: &ProxyParams,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::proxy::generate_proxy(input, params, limits)
    }

    /// Split `input` into `segment_secs`-long segments of `format`.
    pub fn split_by_duration(
        &self,
        input: &[u8],
        segment_secs: f64,
        format: &str,
        limits: &EngineLimits,
    ) -> VideoResult<ops::segment::SplitResult> {
        ops::segment::split_by_duration(input, segment_secs, format, limits)
    }

    /// Split `input` at scene changes (threshold `threshold`).
    pub fn split_by_scenes(
        &self,
        input: &[u8],
        threshold: f64,
        format: &str,
        limits: &EngineLimits,
    ) -> VideoResult<ops::segment::SplitResult> {
        ops::segment::split_by_scenes(input, threshold, format, limits)
    }

    /// Split `input` into `parts` equal-duration segments.
    pub fn split_equal(
        &self,
        input: &[u8],
        parts: u32,
        format: &str,
        limits: &EngineLimits,
    ) -> VideoResult<ops::segment::SplitResult> {
        ops::segment::split_equal(input, parts, format, limits)
    }

    /// Extract `[start_ms, end_ms)` of `input` into `format`.
    pub fn extract_segment(
        &self,
        input: &[u8],
        start_ms: u64,
        end_ms: u64,
        format: &str,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::segment::extract_segment(input, start_ms, end_ms, format, limits)
    }

    /// Measure A/V sync offsets.
    pub fn av_sync_report(&self, input: &[u8]) -> VideoResult<ops::sync::SyncReport> {
        ops::sync::av_sync_report(input)
    }

    /// Apply manual A/V sync correction per `params`.
    pub fn av_sync_fix(
        &self,
        input: &[u8],
        params: &SyncParams,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::sync::av_sync_fix(input, params, limits)
    }

    /// Automatically align audio/video start times.
    pub fn av_sync_auto(&self, input: &[u8], limits: &EngineLimits) -> VideoResult<Vec<u8>> {
        ops::sync::av_sync_auto(input, limits)
    }

    // ------------------------------------------------------------------
    // Validation / waveform / concat / metadata / preview / stabilize
    // ------------------------------------------------------------------

    /// Full media validation report.
    pub fn validate_media(
        &self,
        input: &[u8],
        limits: &EngineLimits,
    ) -> VideoResult<ops::validate::ValidationReport> {
        ops::validate::validate_media(input, limits)
    }

    /// Cheap sanity check: returns `true` when `input` looks like media.
    pub fn validate_quick(&self, input: &[u8]) -> VideoResult<bool> {
        ops::validate::validate_quick(input)
    }

    /// Render an audio waveform/spectrogram image per `params`.
    pub fn audio_waveform_image(
        &self,
        input: &[u8],
        params: &WaveformParams,
    ) -> VideoResult<Vec<u8>> {
        ops::waveform::audio_waveform_image(input, params)
    }

    /// Concatenate heterogeneous inputs into `format`.
    pub fn concat_hetero(
        &self,
        inputs: &[Vec<u8>],
        format: &str,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::concat_filter::concat_hetero(inputs, format, limits)
    }

    /// Structured metadata report for `input`.
    pub fn metadata_report(
        &self,
        input: &[u8],
    ) -> VideoResult<ops::metadata_export::MetadataReport> {
        ops::metadata_export::metadata_report(input)
    }

    /// Export metadata as `format` (json/xml/csv/text).
    pub fn metadata_export(&self, input: &[u8], format: ExportFormat) -> VideoResult<Vec<u8>> {
        ops::metadata_export::metadata_export(input, format)
    }

    /// Set container metadata tags (`(key, value)` pairs).
    pub fn metadata_set(
        &self,
        input: &[u8],
        tags: &[(String, String)],
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::metadata_export::metadata_set(input, tags, limits)
    }

    /// Diff the metadata of `a` and `b`.
    pub fn metadata_compare(
        &self,
        a: &[u8],
        b: &[u8],
    ) -> VideoResult<ops::metadata_export::MetadataDiff> {
        ops::metadata_export::metadata_compare(a, b)
    }

    /// Render an animated preview per `params`.
    pub fn video_preview(
        &self,
        input: &[u8],
        params: &PreviewParams,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::preview::video_preview(input, params, limits)
    }

    /// Whether this ffmpeg build can encode `format` previews.
    pub fn preview_encoder_available(&self, format: PreviewFormat) -> bool {
        ops::preview::preview_encoder_available(format)
    }

    /// Stabilize shaky footage per `params.algo`.
    pub fn stabilize_video(
        &self,
        input: &[u8],
        params: &StabilizeParams,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::stabilize::stabilize_video(input, params, limits)
    }

    /// Stabilize with the best available backend (vidstab → deshake).
    pub fn stabilize_auto(
        &self,
        input: &[u8],
        params: &StabilizeParams,
        limits: &EngineLimits,
    ) -> VideoResult<Vec<u8>> {
        ops::stabilize::stabilize_auto(input, params, limits)
    }
}

impl ProcessingEngine for VideoEngine {
    fn name(&self) -> &'static str {
        "video"
    }

    fn version(&self) -> &'static str {
        // Pinned per the engine contract; the crate version lives in
        // Cargo.toml but the engine API reports its own semver.
        "0.1.0"
    }

    fn kind(&self) -> EngineKind {
        EngineKind::Video
    }

    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            operations: vec![
                EngineOperation::Inspect,
                EngineOperation::Validate,
                EngineOperation::Thumbnail,
                EngineOperation::Trim,
                EngineOperation::Transcode,
                EngineOperation::Concat,
                EngineOperation::ExtractAudio,
                EngineOperation::Remux,
                EngineOperation::Filter,
                EngineOperation::Analyze,
                EngineOperation::Package,
                EngineOperation::ExtractStreams,
                EngineOperation::Subtitles,
                EngineOperation::Slideshow,
                EngineOperation::AudioProcess,
                EngineOperation::MetadataEdit,
                EngineOperation::Quality,
                EngineOperation::Capture,
                EngineOperation::Encode,
                // Reused generic ops the engine also honors:
                EngineOperation::Resize,
                EngineOperation::Rotate,
                EngineOperation::Compress,
                EngineOperation::Watermark,
                EngineOperation::Convert,
            ],
            input_formats: MEDIA_FORMATS.iter().map(|s| s.to_string()).collect(),
            output_formats: MEDIA_FORMATS.iter().map(|s| s.to_string()).collect(),
            // The engine enforces no limits of its own — callers pass
            // EngineLimits per operation.
            limits: EngineLimits::default(),
        }
    }

    fn health<'a>(&'a self) -> EngineFuture<'a, EngineHealth> {
        Box::pin(async move {
            let ffmpeg = tool_available(FFMPEG);
            let ffprobe = tool_available(FFPROBE);
            match (ffmpeg, ffprobe) {
                (true, true) => EngineHealth {
                    status: EngineHealthStatus::Healthy,
                    message: None,
                },
                (false, false) => EngineHealth {
                    status: EngineHealthStatus::Unavailable,
                    message: Some(
                        "neither ffmpeg nor ffprobe found on PATH; video operations unavailable"
                            .to_string(),
                    ),
                },
                _ => {
                    let missing = if ffmpeg { FFPROBE } else { FFMPEG };
                    EngineHealth {
                        status: EngineHealthStatus::Degraded,
                        message: Some(format!(
                            "{missing} not found on PATH; some video operations unavailable"
                        )),
                    }
                }
            }
        })
    }
}
