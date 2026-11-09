//! Subtitle operations: single-track extraction, format conversion,
//! timing shifts, and burn-in (hard subtitles).

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::VideoResult;

/// Extract one subtitle track. `track` is the 0-based subtitle-stream
/// ordinal (`None` → first subtitle stream). `format` is `srt`,
/// `ass`, or `vtt`. Returns the subtitle text bytes.
pub(crate) fn extract_subtitles(
    input: &[u8],
    track: Option<u32>,
    format: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, track, format, limits);
    Err(
        crate::crates::video_engine::error::VideoError::InvalidInput(
            "extract_subtitles not implemented".to_string(),
        ),
    )
}

/// Convert subtitle text between `srt`, `ass`/`ssa`, and `vtt`.
/// `from_format` may be `None` to auto-detect from content.
/// Parsing is best-effort: unrecognized cue payloads are preserved
/// verbatim where the target format allows.
pub(crate) fn convert_subtitles(
    subtitle_bytes: &[u8],
    from_format: Option<&str>,
    to_format: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (subtitle_bytes, from_format, to_format, limits);
    Err(
        crate::crates::video_engine::error::VideoError::InvalidInput(
            "convert_subtitles not implemented".to_string(),
        ),
    )
}

/// Shift every cue by `offset_secs` (negative = earlier). `format`
/// selects the parser (`srt`/`vtt`/`ass`).
pub(crate) fn shift_subtitles(
    subtitle_bytes: &[u8],
    offset_secs: f64,
    format: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (subtitle_bytes, offset_secs, format, limits);
    Err(
        crate::crates::video_engine::error::VideoError::InvalidInput(
            "shift_subtitles not implemented".to_string(),
        ),
    )
}

/// Burn `subtitle_bytes` into `input` as hard subtitles via the
/// `subtitles` filter. `style` tunes font/size/colors; `None` uses
/// ffmpeg defaults. Re-encodes video.
pub(crate) fn burn_subtitles(
    input: &[u8],
    subtitle_bytes: &[u8],
    style: Option<&crate::crates::video_engine::model::SubtitleStyle>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, subtitle_bytes, style, limits);
    Err(
        crate::crates::video_engine::error::VideoError::InvalidInput(
            "burn_subtitles not implemented".to_string(),
        ),
    )
}
