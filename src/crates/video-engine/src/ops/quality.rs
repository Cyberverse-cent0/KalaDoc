//! Objective quality metrics between a reference and a distorted
//! media file: PSNR, SSIM, and VMAF where the ffmpeg build supports
//! them.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::VideoResult;
use crate::crates::video_engine::model::QualityReport;

/// Compare `reference` against `distorted`. `metrics` selects which
/// of `"psnr"`, `"ssim"`, `"vmaf"` to run (empty → all supported).
/// Metrics the build lacks are reported in
/// [`QualityReport::supported_metrics`] and `notes`, never faked.
///
/// Both inputs are probed first; mismatched durations/geometry are
/// handled by the filters' own scaling rules where possible.
pub(crate) fn compare_quality(
    reference: &[u8],
    distorted: &[u8],
    metrics: &[&str],
    limits: &EngineLimits,
) -> VideoResult<QualityReport> {
    let _ = (reference, distorted, metrics, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "compare_quality not implemented".to_string(),
    ))
}
