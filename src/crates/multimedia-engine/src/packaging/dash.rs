//! MPEG-DASH packaging — produce a `.mpd` manifest plus segmented
//! media files playable by DASH clients (players, browsers, CDN
//! origins).
//!
//! [`dash_package`] runs `ffmpeg -f dash` against a temp copy of the
//! input, producing an MPEG-DASH manifest (`output.mpd`) and one or
//! more media segments. The `.mpd` is the primary output; every
//! segment/init file is returned in [`MediaOutput::extra`].
//!
//! # Segment layout
//!
//! With the defaults (`init_mp4 = true`, `use_template`) ffmpeg emits
//! ISOBMFF fragments named `init-stream*.m4s` (initialization) and
//! `chunk-stream*-NNNNN.m4s` (media). `segment_secs` maps to ffmpeg's
//! `-seg_duration`; `use_timeline 0` keeps the manifest small by
//! relying on `$Number%05d$` templating instead of a `SegmentTimeline`
//! element.
//!
//! # Profile
//!
//! [`DashOptions::profile`] carries the DASH profile URN the caller
//! targets (default
//! `"urn:mpeg:dash:profile:isoff-on-demand:2011"`). FFmpeg's dash
//! muxer does not expose the MPD `profiles` attribute as an option —
//! it writes the on-demand profile automatically for static,
//! single-period manifests — so the field is advisory: it is
//! validated (non-empty, `urn:`-prefixed URN) and documented, but is
//! not passed to ffmpeg.

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaFile, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Default DASH profile URN: MPEG-DASH ISO Base media file format
/// on-demand profile — a static, single-period manifest referencing
/// complete segments.
pub const DEFAULT_DASH_PROFILE: &str = "urn:mpeg:dash:profile:isoff-on-demand:2011";

/// Default segment duration in seconds.
pub const DEFAULT_SEGMENT_SECS: u32 = 6;

/// Options for [`dash_package`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashOptions {
    /// Target segment duration in seconds, passed to `-seg_duration`.
    /// Fractional precision is not exposed; ffmpeg snaps segments to
    /// keyframe boundaries anyway.
    pub segment_secs: u32,
    /// DASH profile URN written into caller intent — see module docs.
    /// Must be non-empty and `urn:`-prefixed.
    pub profile: String,
    /// `-c:v` codec specifier. `None` selects `libx264` (the default
    /// for the DASH/ISOBMFF profile). Any other name must be an
    /// encoder in this FFmpeg build or [`MediaError::Unsupported`]
    /// is returned.
    pub video_codec: Option<String>,
    /// `-c:a` codec specifier. `None` selects `aac`. Same validation
    /// as `video_codec`.
    pub audio_codec: Option<String>,
    /// When `true` (default), pass `-dash_segment_type mp4` so ffmpeg
    /// writes ISOBMFF `init-*.m4s` + `chunk-*.m4s` files. When `false`
    /// the muxer picks its `auto` segment type based on the codecs.
    pub init_mp4: bool,
}

impl Default for DashOptions {
    fn default() -> Self {
        Self {
            segment_secs: DEFAULT_SEGMENT_SECS,
            profile: DEFAULT_DASH_PROFILE.to_string(),
            video_codec: None,
            audio_codec: None,
            init_mp4: true,
        }
    }
}

/// Best-effort container sniffing for the temp input extension —
/// same rationale as `remux`: ffmpeg re-probes content, a wrong guess
/// is harmless.
fn sniff_extension(input: &[u8]) -> &'static str {
    if input.len() >= 4 {
        match &input[..4] {
            [0x1A, 0x45, 0xDF, 0xA3] => return "mkv",
            [b'O', b'g', b'g', b'S'] => return "ogg",
            [b'f', b'L', b'a', b'C'] => return "flac",
            [b'R', b'I', b'F', b'F'] => {
                if input.len() >= 12 && &input[8..12] == b"WAVE" {
                    return "wav";
                }
                if input.len() >= 12 && &input[8..12] == b"AVI " {
                    return "avi";
                }
                return "bin";
            }
            [b'I', b'D', b'3', _] => return "mp3",
            _ => {}
        }
        if input.len() >= 8 && &input[4..8] == b"ftyp" {
            return "mp4";
        }
        if input.starts_with(b"\x89PNG") {
            return "png";
        }
        if input.starts_with(b"\xFF\xD8\xFF") {
            return "jpg";
        }
        if input.len() >= 12 && &input[8..12] == b"WEBP" {
            return "webp";
        }
        if input[0] == 0xFF && (input[1] & 0xE0) == 0xE0 {
            return "mp3";
        }
    }
    "bin"
}

/// MIME type for a produced file name.
fn content_type(name: &str) -> Option<String> {
    if name.ends_with(".mpd") {
        Some("application/dash+xml".to_string())
    } else if name.ends_with(".m4s") || name.ends_with(".mp4") {
        Some("video/iso.segment".to_string())
    } else {
        None
    }
}

/// Package `input` as an MPEG-DASH stream.
///
/// # Arguments
///
/// - `input` — complete media file bytes.
/// - `options` — see [`DashOptions`].
/// - `limits` — per-op byte/time/file-count limits.
/// - `rt` — the discovered FFmpeg [`Runtime`]; its capability
///   registry validates the chosen codecs and the `dash` muxer.
///
/// # Errors
///
/// - [`MediaError::LimitExceeded`] when `input` exceeds
///   `limits.max_input_bytes`, when ffmpeg produces more than
///   `limits.max_output_files` files, or an output file exceeds
///   `limits.max_output_bytes`.
/// - [`MediaError::Unsupported`] when the `dash` muxer or a requested
///   codec is unavailable in this FFmpeg build.
/// - [`MediaError::InvalidInput`] when `options.profile` is empty or
///   not an `urn:` URN.
/// - [`MediaError::ToolFailed`] when ffmpeg cannot package the input
///   (e.g. no audio stream and an explicit `-c:a` is fine, but a
///   fundamentally undecodable input fails).
pub fn dash_package(
    input: &[u8],
    options: &DashOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }

    let profile = options.profile.trim();
    if profile.is_empty() || !profile.starts_with("urn:") {
        return Err(MediaError::InvalidInput(format!(
            "DASH profile must be a non-empty urn: URI, got {profile:?}"
        )));
    }
    if options.segment_secs == 0 {
        return Err(MediaError::InvalidInput(
            "segment_secs must be >= 1".to_string(),
        ));
    }

    let caps = rt.capabilities()?;
    if !caps.has_muxer("dash") {
        return Err(MediaError::Unsupported(
            "dash muxer is not available in this FFmpeg build".to_string(),
        ));
    }

    const DEFAULT_VIDEO_ENCODERS: &[&str] = &["libx264", "libopenh264", "h264", "mpeg4"];
    let video_codec = match options.video_codec.as_deref() {
        Some(c) => c,
        None => DEFAULT_VIDEO_ENCODERS
            .iter()
            .copied()
            .find(|e| caps.has_encoder(e))
            .ok_or_else(|| {
                MediaError::Unsupported(
                    "no usable video encoder (libx264/libopenh264/h264/mpeg4) in this FFmpeg \
                     build"
                        .to_string(),
                )
            })?,
    };
    let audio_codec = options.audio_codec.as_deref().unwrap_or("aac");
    for (kind, codec) in [("video", video_codec), ("audio", audio_codec)] {
        if codec != "copy" && codec != "none" && !caps.has_encoder(codec) {
            return Err(MediaError::Unsupported(format!(
                "{kind} codec {codec:?} is not an encoder in this FFmpeg build"
            )));
        }
    }

    let dir = TempDir::new("dash")?;
    let in_ext = sniff_extension(input);
    let in_name = format!("input.{in_ext}");
    dir.write(&in_name, input)?;

    let mpd_name = "output.mpd".to_string();
    let mpd_path = dir.file(&mpd_name);

    let mut argv: Vec<std::ffi::OsString> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-y".into(),
        "-i".into(),
        dir.file(&in_name).into_os_string(),
        "-c:v".into(),
        video_codec.into(),
        "-c:a".into(),
        audio_codec.into(),
        "-f".into(),
        "dash".into(),
        "-seg_duration".into(),
        options.segment_secs.to_string().into(),
        "-use_template".into(),
        "1".into(),
        "-use_timeline".into(),
        "0".into(),
    ];
    if options.init_mp4 {
        argv.extend(["-dash_segment_type".into(), "mp4".into()]);
    }
    argv.push(mpd_path.into_os_string());

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    // Collect every file ffmpeg produced besides the input: the .mpd
    // manifest is primary, segments/sidecars are extras.
    let mut produced: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(dir.path())? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name != in_name && entry.file_type()?.is_file() {
            produced.push(name);
        }
    }
    produced.sort();

    if let Some(max_files) = limits.max_output_files
        && (produced.len() as u32) > max_files
    {
        return Err(MediaError::LimitExceeded(format!(
            "dash packaging produced {} files, exceeding max_output_files {max_files}",
            produced.len()
        )));
    }

    let mpd_bytes = dir.read_limited(&mpd_name, limits.max_output_bytes)?;
    if mpd_bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: "dash muxer produced a zero-byte manifest".to_string(),
        });
    }

    let mut extra = Vec::new();
    for name in produced {
        if name == mpd_name {
            continue;
        }
        let bytes = dir.read_limited(&name, limits.max_output_bytes)?;
        if bytes.is_empty() {
            continue;
        }
        let content_type = content_type(&name);
        extra.push(MediaFile {
            name,
            bytes,
            content_type,
        });
    }

    let mut result = MediaOutput {
        primary: Some(MediaFile {
            name: mpd_name,
            bytes: mpd_bytes,
            content_type: content_type("output.mpd"),
        }),
        extra,
        warnings: Vec::new(),
    };
    if !out.stderr.is_empty() {
        result.warnings.push(out.stderr);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::discovery::Runtime;
    use crate::crates::multimedia_engine::runtime::process;

    /// Generate an ~8s H.264/AAC clip via lavfi for DASH packaging.
    fn clip_8s() -> Vec<u8> {
        let rt = Runtime::discover().expect("ffmpeg/ffprobe on PATH");
        let dir = TempDir::new("dash-test").unwrap();
        let out = dir.file("fixture.mp4");
        let args = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=8:size=64x64:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=8",
            "-c:v",
            ["libx264", "libopenh264", "h264", "mpeg4"]
                .iter()
                .copied()
                .find(|e| rt.capabilities().unwrap().has_encoder(e))
                .expect("no usable video encoder in test build"),
            "-c:a",
            "aac",
            "-shortest",
            out.to_str().unwrap(),
        ]);
        process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, &MediaLimits::default())
            .expect("fixture encode failed");
        let bytes = std::fs::read(&out).unwrap();
        assert!(!bytes.is_empty());
        bytes
    }

    #[test]
    fn dash_package_produces_mpd_and_m4s_segments() {
        let rt = Runtime::discover().unwrap();
        let input = clip_8s();
        let out = dash_package(
            &input,
            &DashOptions::default(),
            &MediaLimits::default(),
            &rt,
        )
        .expect("dash packaging failed");

        let primary = out.primary.as_ref().expect("primary output");
        assert_eq!(primary.name, "output.mpd");
        let mpd = String::from_utf8_lossy(&primary.bytes);
        assert!(mpd.contains("<MPD"), "manifest missing <MPD>: {mpd}");

        let m4s: Vec<_> = out
            .extra
            .iter()
            .filter(|f| f.name.ends_with(".m4s"))
            .collect();
        assert!(
            !m4s.is_empty(),
            "expected at least one .m4s segment, got {:?}",
            out.extra.iter().map(|f| &f.name).collect::<Vec<_>>()
        );
        // 8s @ 6s segments → at least one init + one media segment.
        assert!(
            m4s.iter().any(|f| f.name.starts_with("init-")),
            "expected an init segment"
        );
        assert!(
            m4s.iter().any(|f| f.name.starts_with("chunk-")),
            "expected a media chunk segment"
        );
    }

    #[test]
    fn dash_package_rejects_bad_profile_and_file_cap() {
        let rt = Runtime::discover().unwrap();
        let input = clip_8s();

        let bad = DashOptions {
            profile: "not-a-urn".into(),
            ..Default::default()
        };
        match dash_package(&input, &bad, &MediaLimits::default(), &rt) {
            Err(MediaError::InvalidInput(_)) => {}
            other => panic!("expected InvalidInput, got {:?}", other.map(|_| ())),
        }

        let limits = MediaLimits {
            max_output_files: Some(1),
            ..MediaLimits::default()
        };
        match dash_package(&input, &DashOptions::default(), &limits, &rt) {
            Err(MediaError::LimitExceeded(_)) => {}
            other => panic!("expected LimitExceeded, got {:?}", other.map(|_| ())),
        }
    }
}
