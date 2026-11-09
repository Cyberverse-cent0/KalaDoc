//! Generic filter-graph passthrough plus a named preset catalog.
//!
//! [`apply_filters`] accepts raw ffmpeg `-vf`/`-af` filter strings —
//! powerful but dangerous if fed untrusted input, so callers must
//! sanitize (the CLI exposes presets, not raw graphs, by default).
//! [`FILTER_PRESETS`] maps friendly names to vetted filter chains.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::VideoResult;

/// Apply `video_filter` (`-vf`) and/or `audio_filter` (`-af`) to
/// `input`, re-encoding via the standard path. At least one filter
/// must be non-empty. Filter strings are passed as single argv
/// elements — never through a shell.
pub(crate) fn apply_filters(
    input: &[u8],
    video_filter: Option<&str>,
    audio_filter: Option<&str>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, video_filter, audio_filter, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "apply_filters not implemented".to_string(),
    ))
}

/// A named, vetted filter preset: `name` → (video filter, audio
/// filter). Both may be empty for analysis-only presets.
pub(crate) struct FilterPreset {
    /// Stable preset identifier (CLI/API surface).
    pub name: &'static str,
    /// ffmpeg `-vf` chain, `""` for none.
    pub video_filter: &'static str,
    /// ffmpeg `-af` chain, `""` for none.
    pub audio_filter: &'static str,
    /// One-line description for help output.
    pub description: &'static str,
}

/// Catalog of safe presets. Include at least: `grayscale`, `sepia`,
/// `negate`, `blur`, `sharpen`, `denoise`, `deinterlace`,
/// `vignette`-style, `fade-in`, `fade-out`, `volume-boost`,
/// `mono`, `normalize`. Verify each filter exists via
/// `ffmpeg -filters` before advertising it — presets referencing
/// missing filters must be omitted, not shipped broken.
pub(crate) fn filter_presets() -> Vec<FilterPreset> {
    Vec::new()
}

/// Look up a preset by name (case-insensitive).
pub(crate) fn preset_by_name(name: &str) -> Option<FilterPreset> {
    filter_presets()
        .into_iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
}
