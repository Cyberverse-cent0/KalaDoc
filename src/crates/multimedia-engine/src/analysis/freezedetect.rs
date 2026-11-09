//! Frozen-frame detection with an options struct.
//!
//! The core detector lives in [`super::blackdetect`] (which hosts the
//! `freezedetect` ffmpeg filter alongside `blackdetect`); this module
//! exposes the same capability through a dedicated options type so
//! callers get one consistent per-op options contract.

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::analysis::blackdetect::{DetectionReport, freezedetect};
use crate::crates::multimedia_engine::error::MediaResult;
use crate::crates::multimedia_engine::model::MediaLimits;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;

/// Options for [`detect_freeze`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FreezeOptions {
    /// Noise tolerance in dB passed as `n=`. More negative = stricter
    /// (less noise tolerated before frames count as changed).
    /// Default `-60.0`.
    pub noise_db: f64,
    /// Minimum freeze duration in seconds passed as `d=`.
    /// Default `2.0`.
    pub min_duration_secs: f64,
}

impl Default for FreezeOptions {
    fn default() -> Self {
        Self {
            noise_db: -60.0,
            min_duration_secs: 2.0,
        }
    }
}

/// Detect frozen (unchanged) frames in `input`.
///
/// Delegates to [`super::blackdetect::freezedetect`]; see that
/// function's docs for the stderr-parsing details.
pub fn detect_freeze(
    input: &[u8],
    options: &FreezeOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<DetectionReport> {
    freezedetect(
        input,
        options.noise_db,
        options.min_duration_secs,
        limits,
        rt,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_finite_options() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => {
                eprintln!("skipping: ffmpeg/ffprobe not on PATH");
                return;
            }
        };
        let input = b"not media".to_vec();
        for opts in [
            FreezeOptions {
                noise_db: f64::NAN,
                ..Default::default()
            },
            FreezeOptions {
                min_duration_secs: f64::INFINITY,
                ..Default::default()
            },
        ] {
            assert!(detect_freeze(&input, &opts, &MediaLimits::default(), &rt).is_err());
        }
    }
}
