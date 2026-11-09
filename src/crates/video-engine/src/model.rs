//! Serializable media description produced by [`probe`](crate::crates::video_engine::VideoEngine::probe)
//! and the shared types used by the multimedia ops modules.

use std::collections::BTreeMap;

use serde::Serialize;

/// Metadata about a media input, extracted via `ffprobe`.
///
/// Every field except `size_bytes`/`has_audio` is optional: `ffprobe`
/// reports only what the container actually carries, and this engine
/// never invents values — a missing stream attribute stays `None`.
#[derive(Debug, Clone, Serialize)]
pub struct VideoInfo {
    /// Container/stream duration in seconds, when reported.
    pub duration_secs: Option<f64>,
    /// Width of the first video stream in pixels, when present.
    pub width: Option<u32>,
    /// Height of the first video stream in pixels, when present.
    pub height: Option<u32>,
    /// Frame rate of the first video stream (from `avg_frame_rate`,
    /// falling back to `r_frame_rate`), when reported as `num/den`.
    pub fps: Option<f64>,
    /// Codec name of the first video stream (e.g. `"h264"`).
    pub vcodec: Option<String>,
    /// Codec name of the first audio stream (e.g. `"aac"`), when the
    /// input carries audio.
    pub acodec: Option<String>,
    /// Demuxer/format name reported by ffprobe (e.g.
    /// `"mov,mp4,m4a,3gp,3g2,mj2"` for MP4).
    pub format_name: Option<String>,
    /// Input size in bytes as reported by the container. `0` when the
    /// format does not report a size.
    pub size_bytes: u64,
    /// Whether the input contains at least one audio stream.
    pub has_audio: bool,
    /// Whether the input contains at least one video stream.
    pub has_video: bool,
    /// Overall container bit rate in bits per second, when reported.
    pub bit_rate: Option<u64>,
    /// Long format name (e.g. `"QuickTime / MOV"`), when reported.
    pub format_long_name: Option<String>,
    /// Every stream in the container, in demuxer order.
    pub streams: Vec<StreamInfo>,
    /// Embedded chapters, when the container carries them.
    pub chapters: Vec<ChapterInfo>,
    /// Container-level tags/metadata (`title`, `artist`, ...). Keys
    /// are lowercased; values are raw strings.
    pub tags: BTreeMap<String, String>,
}

/// One stream (video, audio, subtitle, attachment, data, ...) inside
/// a media container, as reported by `ffprobe -show_streams`.
#[derive(Debug, Clone, Serialize)]
pub struct StreamInfo {
    /// Stream index within the container.
    pub index: u32,
    /// Stream kind: `"video"`, `"audio"`, `"subtitle"`,
    /// `"attachment"`, `"data"`, or another ffprobe `codec_type`.
    pub codec_type: String,
    /// Codec name (e.g. `"h264"`, `"aac"`, `"mov_text"`).
    pub codec_name: Option<String>,
    /// Codec profile (e.g. `"High"`, `"LC"`), when reported.
    pub profile: Option<String>,
    /// Video: width in pixels.
    pub width: Option<u32>,
    /// Video: height in pixels.
    pub height: Option<u32>,
    /// Video: pixel format (e.g. `"yuv420p"`).
    pub pix_fmt: Option<String>,
    /// Video: display aspect ratio (e.g. `"16:9"`).
    pub display_aspect_ratio: Option<String>,
    /// Video: field order (e.g. `"progressive"`, `"tt"`).
    pub field_order: Option<String>,
    /// Audio: sample rate in Hz.
    pub sample_rate: Option<u32>,
    /// Audio: channel count.
    pub channels: Option<u32>,
    /// Audio: channel layout (e.g. `"stereo"`).
    pub channel_layout: Option<String>,
    /// Stream language tag (e.g. `"eng"`), when tagged.
    pub language: Option<String>,
    /// Whether this is the container's default stream of its kind.
    pub is_default: bool,
    /// Whether this stream is attached cover art.
    pub attached_pic: bool,
    /// Stream bit rate in bits per second, when reported.
    pub bit_rate: Option<u64>,
    /// Duration of this stream in seconds, when reported.
    pub duration_secs: Option<f64>,
    /// Subtitle streams: codec-specific tag identifying the track
    /// (e.g. language/role hints), when reported.
    pub tags: BTreeMap<String, String>,
}

/// One embedded chapter marker.
#[derive(Debug, Clone, Serialize)]
pub struct ChapterInfo {
    /// Chapter identifier (ffprobe `id`, often numeric).
    pub id: Option<u64>,
    /// Chapter start time in seconds.
    pub start_secs: f64,
    /// Chapter end time in seconds.
    pub end_secs: f64,
    /// Chapter title, when tagged.
    pub title: Option<String>,
}

/// A named output artifact produced by a multi-file operation
/// (packaging, stream extraction, frame sequences). `name` is a
/// relative path inside the output set (e.g. `"index.m3u8"`,
/// `"segment_000.ts"`) — callers must sanitize before writing.
pub struct MediaFile {
    /// Relative artifact name.
    pub name: String,
    /// Artifact bytes.
    pub bytes: Vec<u8>,
}

/// Result of an operation that produces zero or more named files.
/// Single-file operations keep returning `Vec<u8>`; use this when an
/// operation legitimately emits several artifacts (HLS segments,
/// extracted stream sets, image sequences).
pub struct MediaOutput {
    /// Produced files. Order is deterministic (manifests/playlists
    /// first where the format requires it is NOT guaranteed — callers
    /// should write by name).
    pub files: Vec<MediaFile>,
}

/// How `resize`-family ops should fit the source into the target box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FitMode {
    /// Preserve aspect ratio; fit entirely inside the box.
    Fit,
    /// Preserve aspect ratio; fill the box, cropping overflow.
    Cover,
    /// Preserve aspect ratio; fit inside and pad the remainder.
    Contain,
    /// Ignore aspect ratio; stretch to exactly the box.
    Stretch,
}

/// Which frames [`extract_frames`](crate::crates::video_engine) pulls.
#[derive(Debug, Clone)]
pub struct FramesSpec {
    /// Emit every frame (`true`) — mutually exclusive with the
    /// interval/fps/keyframe selectors.
    pub all: bool,
    /// One frame every N seconds (e.g. `Some(5.0)` → 0s, 5s, 10s...).
    pub every_secs: Option<f64>,
    /// One frame per second tick at this rate (e.g. `Some(0.5)` →
    /// every 2s).
    pub fps: Option<f64>,
    /// Only keyframes/I-frames.
    pub keyframes_only: bool,
    /// Frame image format: `"png"` or `"jpg"`.
    pub format: String,
}

/// Loudness measurement of the audio program (EBU R128 style).
#[derive(Debug, Clone, Serialize)]
pub struct LoudnessReport {
    /// Integrated loudness in LUFS.
    pub integrated_lufs: Option<f64>,
    /// Loudness range in LU.
    pub loudness_range_lu: Option<f64>,
    /// True peak in dBTP.
    pub true_peak_dbtp: Option<f64>,
    /// Per-channel momentary maxima when reported.
    pub channel_lra: Option<f64>,
}

/// Objective quality comparison between a reference and a distorted
/// media file.
#[derive(Debug, Clone, Serialize)]
pub struct QualityReport {
    /// PSNR in dB (video streams), `None` when unavailable.
    pub psnr_db: Option<f64>,
    /// SSIM index 0.0–1.0 (video streams), `None` when unavailable.
    pub ssim: Option<f64>,
    /// VMAF score 0–100, `None` when the build lacks libvmaf.
    pub vmaf: Option<f64>,
    /// Which metrics the ffmpeg build actually supports.
    pub supported_metrics: Vec<String>,
    /// Human-readable notes (e.g. "vmaf unavailable: no libvmaf").
    pub notes: Vec<String>,
}

/// Structured report produced by media analysis operations.
#[derive(Debug, Clone, Serialize)]
pub struct MediaAnalysis {
    /// Detected black-frame intervals `[start_secs, end_secs)`.
    pub black_segments: Vec<(f64, f64)>,
    /// Detected freeze-frame intervals.
    pub freeze_segments: Vec<(f64, f64)>,
    /// Detected silence intervals.
    pub silence_segments: Vec<(f64, f64)>,
    /// Scene-change timestamps with scores (0.0–1.0-ish, filter
    /// dependent), sorted ascending.
    pub scenes: Vec<(f64, f64)>,
    /// Loudness measurement, when audio is present and measurable.
    pub loudness: Option<LoudnessReport>,
    /// Free-form metric map for extra detectors that ran
    /// (e.g. `"mean_volume"`, `"dc_offset"`).
    pub metrics: BTreeMap<String, f64>,
}

/// Which embedded stream kinds [`extract_streams`] should pull out.
#[derive(Debug, Clone, Default)]
pub struct StreamSelection {
    /// Subtitle streams (converted to the requested text format).
    pub subtitles: bool,
    /// Binary attachments.
    pub attachments: bool,
    /// Attached cover art (first attached picture stream).
    pub cover_art: bool,
    /// Chapters, exported as a JSON sidecar.
    pub chapters: bool,
    /// Data/other streams (best-effort).
    pub data: bool,
}

/// A capture device, source, or protocol reported by the ffmpeg
/// build — enumeration only; this engine does not perform live
/// capture in v1.
#[derive(Debug, Clone, Serialize)]
pub struct CaptureDevice {
    /// Device kind: `"video"`, `"audio"`, `"source"`,
    /// `"format"`, `"protocol"`, `"hwaccel"`.
    pub kind: String,
    /// Device/source name usable in `-f`/`-i` arguments.
    pub name: String,
    /// Human-readable description, when available.
    pub description: Option<String>,
}

/// Encoding profile for professional/intermediate delivery.
/// Each field maps to an ffmpeg output option; `None` fields are
/// omitted so the ffmpeg default applies.
#[derive(Debug, Clone)]
pub struct EncodeProfile {
    /// Video codec name as ffmpeg knows it (`"prores"`, `"dnxhd"`,
    /// `"ffv1"`, `"huffyuv"`, `"mjpeg"`, `"mpeg2video"`, ...).
    pub vcodec: String,
    /// Audio codec name (`"aac"`, `"pcm_s16le"`, `"flac"`, ...),
    /// or `"none"` to drop audio.
    pub acodec: String,
    /// Constant-rate-factor style quality knob when the codec
    /// supports it (CRF/QP). Mapped to `-crf`.
    pub crf: Option<u32>,
    /// Encoder preset (`"slow"`, `"fast"`, ...).
    pub preset: Option<String>,
    /// Target video bitrate, e.g. `"20M"`.
    pub video_bitrate: Option<String>,
    /// Target audio bitrate, e.g. `"192k"`.
    pub audio_bitrate: Option<String>,
    /// Pixel format override (`"yuv422p10le"`, ...).
    pub pix_fmt: Option<String>,
    /// Extra raw ffmpeg output arguments appended verbatim
    /// (advanced escape hatch — callers must not pass untrusted
    /// input here).
    pub extra_args: Vec<String>,
}

/// Styling knobs for [`burn_subtitles`]. `None` fields fall back to
/// ffmpeg `subtitles` filter defaults.
#[derive(Debug, Clone)]
pub struct SubtitleStyle {
    /// Font family name (must exist on the host for drawtext/subtitles
    /// filters that rasterize).
    pub font_name: Option<String>,
    /// Font size in pixels.
    pub font_size: Option<u32>,
    /// Primary text color `0xRRGGBB`.
    pub primary_color: Option<String>,
    /// Outline/border color `0xRRGGBB`.
    pub outline_color: Option<String>,
    /// Outline width in pixels.
    pub outline_width: Option<u32>,
    /// Drop shadow offset in pixels.
    pub shadow_offset: Option<u32>,
    /// Vertical margin from the bottom edge in pixels.
    pub margin_v: Option<u32>,
    /// ASS alignment value (1–11); `None` = bottom-center.
    pub alignment: Option<u8>,
}

/// Which stream-selection rules [`select_streams`] applies.
#[derive(Debug, Clone)]
pub struct StreamFilter {
    /// Keep the video stream (`false` → video-less output).
    pub keep_video: bool,
    /// Audio streams to keep: `None` keeps all, `Some(vec)` keeps
    /// only those 0-based audio stream ordinals.
    pub audio_ordinals: Option<Vec<u32>>,
    /// Subtitle streams to keep (0-based ordinals across subtitle
    /// streams only), `None` keeps all.
    pub subtitle_ordinals: Option<Vec<u32>>,
    /// Drop attached pictures/data streams.
    pub drop_attachments: bool,
}
