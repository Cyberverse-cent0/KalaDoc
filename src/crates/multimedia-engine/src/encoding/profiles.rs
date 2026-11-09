//! Encoding profiles — named presets that expand to ffmpeg codec
//! arguments.
//!
//! An [`EncodingProfile`] bundles the video codec, its quality args,
//! the audio codec, its args, and the container hint into one
//! serde-friendly struct. [`profile_args`] turns a profile into the
//! `-c:v/-crf/.../-c:a/-b:a` argv fragment shared by every op that
//! re-encodes (transcode, slideshow, trim-accurate, ...).
//!
//! [`validate_profile`] checks every named codec against the runtime
//! capability registry so callers get [`MediaError::Unsupported`]
//! up front instead of an opaque ffmpeg failure.

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;

/// A named encoding preset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EncodingProfile {
    /// Profile name (`"h264-fast"`, `"h265-quality"`, ...).
    pub name: String,
    /// Video encoder name (`ffmpeg -encoders` token).
    pub video_codec: String,
    /// Extra video args after `-c:v` (e.g. `crf`, `preset`,
    /// `pix_fmt`). Each element is one argv token.
    #[serde(default)]
    pub video_args: Vec<String>,
    /// Audio encoder name.
    pub audio_codec: String,
    /// Extra audio args after `-c:a` (e.g. `b:a` bitrate).
    #[serde(default)]
    pub audio_args: Vec<String>,
    /// Suggested output container extension (no dot). Advisory —
    /// ops may override via their own `output_format` option.
    pub container: String,
}

/// Built-in profiles. Names are stable API; contents may tune as
/// ffmpeg defaults evolve.
///
/// Built once on first access — `EncodingProfile` owns `String`s, so
/// a plain `const`/`static` table is not possible.
pub fn profiles() -> &'static [EncodingProfile] {
    static PROFILES: std::sync::OnceLock<Vec<EncodingProfile>> = std::sync::OnceLock::new();
    PROFILES.get_or_init(|| {
        vec![
            EncodingProfile {
                name: "h264-fast".to_string(),
                video_codec: "libx264".to_string(),
                video_args: vec![
                    "preset".into(),
                    "veryfast".into(),
                    "crf".into(),
                    "23".into(),
                    "pix_fmt".into(),
                    "yuv420p".into(),
                ],
                audio_codec: "aac".into(),
                audio_args: vec!["b:a".into(), "128k".into()],
                container: "mp4".into(),
            },
            EncodingProfile {
                name: "h264-quality".to_string(),
                video_codec: "libx264".to_string(),
                video_args: vec![
                    "preset".into(),
                    "slow".into(),
                    "crf".into(),
                    "18".into(),
                    "pix_fmt".into(),
                    "yuv420p".into(),
                ],
                audio_codec: "aac".into(),
                audio_args: vec!["b:a".into(), "192k".into()],
                container: "mp4".into(),
            },
            EncodingProfile {
                name: "h265-quality".to_string(),
                video_codec: "libx265".to_string(),
                video_args: vec![
                    "preset".into(),
                    "medium".into(),
                    "crf".into(),
                    "28".into(),
                    "tag:v".into(),
                    "hvc1".into(),
                    "pix_fmt".into(),
                    "yuv420p".into(),
                ],
                audio_codec: "aac".into(),
                audio_args: vec!["b:a".into(), "160k".into()],
                container: "mp4".into(),
            },
            EncodingProfile {
                name: "vp9-web".to_string(),
                video_codec: "libvpx-vp9".to_string(),
                video_args: vec![
                    "crf".into(),
                    "33".into(),
                    "b:v".into(),
                    "0".into(),
                    "row-mt".into(),
                    "1".into(),
                ],
                audio_codec: "libopus".into(),
                audio_args: vec!["b:a".into(), "128k".into()],
                container: "webm".into(),
            },
            EncodingProfile {
                name: "av1-web".to_string(),
                video_codec: "libsvtav1".to_string(),
                video_args: vec!["preset".into(), "8".into(), "crf".into(), "35".into()],
                audio_codec: "libopus".into(),
                audio_args: vec!["b:a".into(), "128k".into()],
                container: "webm".into(),
            },
            EncodingProfile {
                name: "prores".to_string(),
                video_codec: "prores_ks".to_string(),
                video_args: vec![
                    "profile:v".into(),
                    "3".into(), // ProRes HQ
                    "pix_fmt".into(),
                    "yuv422p10le".into(),
                ],
                audio_codec: "pcm_s16le".into(),
                audio_args: vec![],
                container: "mov".into(),
            },
            EncodingProfile {
                name: "ffv1-lossless".to_string(),
                video_codec: "ffv1".to_string(),
                video_args: vec![
                    "level".into(),
                    "3".into(),
                    "coder".into(),
                    "1".into(),
                    "context".into(),
                    "1".into(),
                    "g".into(),
                    "1".into(),
                    "slices".into(),
                    "16".into(),
                    "slicecrc".into(),
                    "1".into(),
                ],
                audio_codec: "flac".into(),
                audio_args: vec![],
                container: "mkv".into(),
            },
        ]
    })
}

/// Look up a built-in profile by name.
pub fn get_profile(name: &str) -> Option<&'static EncodingProfile> {
    profiles().iter().find(|p| p.name == name)
}

/// List all built-in profile names.
pub fn profile_names() -> Vec<&'static str> {
    profiles().iter().map(|p| p.name.as_str()).collect()
}

/// Expand a profile into ffmpeg argv fragments: `-c:v <codec>
/// <video_args...> -c:a <codec> <audio_args...>`.
///
/// Args are returned as owned strings; callers map them to
/// `OsString` when building the command line.
pub fn profile_args(profile: &EncodingProfile) -> Vec<String> {
    let mut args = Vec::with_capacity(4 + profile.video_args.len() + profile.audio_args.len());
    args.push("-c:v".to_string());
    args.push(profile.video_codec.clone());
    args.extend(profile.video_args.iter().cloned());
    args.push("-c:a".to_string());
    args.push(profile.audio_codec.clone());
    args.extend(profile.audio_args.iter().cloned());
    args
}

/// Validate that every codec named by `profile` exists as an
/// encoder in this FFmpeg build.
///
/// # Errors
///
/// [`MediaError::Unsupported`] naming the missing encoder.
pub fn validate_profile(profile: &EncodingProfile, rt: &Runtime) -> MediaResult<()> {
    let caps = rt.capabilities()?;
    for (kind, codec) in [
        ("video", profile.video_codec.as_str()),
        ("audio", profile.audio_codec.as_str()),
    ] {
        if !caps.has_encoder(codec) {
            return Err(MediaError::Unsupported(format!(
                "profile {:?}: {kind} encoder {codec:?} is not available in this FFmpeg build",
                profile.name
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_have_unique_names_and_valid_shape() {
        let names: std::collections::BTreeSet<_> =
            profiles().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names.len(), profiles().len(), "duplicate profile names");
        for p in profiles() {
            assert!(!p.video_codec.is_empty());
            assert!(!p.audio_codec.is_empty());
            assert!(!p.container.is_empty());
            // video_args/audio_args must be flag/value pairs.
            assert_eq!(p.video_args.len() % 2, 0, "{}: odd video_args", p.name);
            assert_eq!(p.audio_args.len() % 2, 0, "{}: odd audio_args", p.name);
        }
    }

    #[test]
    fn profile_args_expands_pairs() {
        let p = get_profile("h264-fast").unwrap();
        let args = profile_args(p);
        assert_eq!(args[0], "-c:v");
        assert_eq!(args[1], "libx264");
        assert!(args.contains(&"-c:a".to_string()));
        assert!(args.contains(&"aac".to_string()));
    }

    #[test]
    fn validate_profile_checks_encoders() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => {
                eprintln!("skipping: ffmpeg/ffprobe not on PATH");
                return;
            }
        };
        // h264-fast needs libx264+aac — both ubiquitous.
        let p = get_profile("h264-fast").unwrap();
        // Don't assert Ok: minimal builds may lack libx264. Just ensure
        // the error, when present, is Unsupported (never panic).
        match validate_profile(p, &rt) {
            Ok(()) | Err(MediaError::Unsupported(_)) => {}
            Err(e) => panic!("unexpected error kind: {e}"),
        }
    }
}
