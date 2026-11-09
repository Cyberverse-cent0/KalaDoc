//! Serializable media description produced by [`probe`](crate::crates::video_engine::VideoEngine::probe).

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
}
