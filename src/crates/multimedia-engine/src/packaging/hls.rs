//! HLS (HTTP Live Streaming) packaging — produce an `.m3u8`
//! playlist plus media segments playable by HLS clients (Safari,
//! hls.js, ffplay, browsers).
//!
//! [`hls_package`] runs `ffmpeg -f hls` against a temp copy of the
//! input, producing a playlist (`output.m3u8`) and one or more
//! segments (`.ts` for MPEG-TS, `.m4s`/`.mp4`/`.aac` for fMP4).
//! The playlist is the primary output; every segment is returned in
//! [`MediaOutput::extra`].
//!
//! # Segment types
//!
//! - [`HlsSegmentType::MpegTs`] (default): `segment.ts` files —
//!   universally supported, simplest for CDNs and players.
//! - [`HlsSegmentType::Fmp4`]: fragmented-MP4 `init.mp4` +
//!   `segment*.m4s` files — required for some DRM/fMP4 workflows.
//!
//! `hls_time` maps to ffmpeg's `-hls_time`; segments still snap to
//! keyframe boundaries.

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaFile, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Segment container type for HLS output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum HlsSegmentType {
    /// MPEG-TS `.ts` segments — the classic, most-compatible choice.
    #[default]
    MpegTs,
    /// Fragmented MP4 (`init.mp4` + `*.m4s`) segments.
    Fmp4,
}

impl HlsSegmentType {
    fn ffmpeg_name(self) -> &'static str {
        match self {
            Self::MpegTs => "mpegts",
            Self::Fmp4 => "fmp4",
        }
    }
}

/// Options for [`hls_package`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HlsOptions {
    /// Target segment duration in seconds, passed to `-hls_time`.
    /// Must be `>= 1`.
    pub segment_secs: u32,
    /// Segment container type.
    pub segment_type: HlsSegmentType,
    /// `-c:v` codec. `None` selects `libx264`. `"copy"` remuxes
    /// without re-encoding (requires the source codec be HLS-safe).
    pub video_codec: Option<String>,
    /// `-c:a` codec. `None` selects `aac`.
    pub audio_codec: Option<String>,
    /// Playlist type: `"vod"` (default) produces a complete VOD
    /// playlist with `#EXT-X-ENDLIST`; `"event"` omits it for
    /// live/event playlists. Any other value fails validation.
    pub playlist_type: String,
}

impl Default for HlsOptions {
    fn default() -> Self {
        Self {
            segment_secs: 6,
            segment_type: HlsSegmentType::MpegTs,
            video_codec: None,
            audio_codec: None,
            playlist_type: "vod".to_string(),
        }
    }
}

/// Best-effort container sniff for the temp input extension.
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
    }
    "bin"
}

/// MIME type for a produced file name.
fn content_type(name: &str) -> Option<String> {
    if name.ends_with(".m3u8") {
        Some("application/vnd.apple.mpegurl".to_string())
    } else if name.ends_with(".ts") {
        Some("video/mp2t".to_string())
    } else if name.ends_with(".m4s") || name.ends_with(".mp4") {
        Some("video/iso.segment".to_string())
    } else if name.ends_with(".aac") {
        Some("audio/aac".to_string())
    } else {
        None
    }
}

/// Package `input` as an HLS stream.
///
/// # Arguments
///
/// - `input` — complete media file bytes.
/// - `options` — see [`HlsOptions`].
/// - `limits` — per-op byte/time/file-count limits.
/// - `rt` — the discovered FFmpeg [`Runtime`]; its capability
///   registry validates the `hls` muxer and chosen codecs.
///
/// # Errors
///
/// - [`MediaError::LimitExceeded`] when `input` exceeds
///   `limits.max_input_bytes`, when ffmpeg produces more than
///   `limits.max_output_files` files, or an output exceeds
///   `limits.max_output_bytes`.
/// - [`MediaError::Unsupported`] when the `hls` muxer or a requested
///   codec is unavailable.
/// - [`MediaError::InvalidInput`] for `segment_secs == 0` or an
///   unknown `playlist_type`.
/// - [`MediaError::ToolFailed`] when ffmpeg cannot package the input.
pub fn hls_package(
    input: &[u8],
    options: &HlsOptions,
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
    if options.segment_secs == 0 {
        return Err(MediaError::InvalidInput(
            "segment_secs must be >= 1".to_string(),
        ));
    }
    if !matches!(options.playlist_type.as_str(), "vod" | "event") {
        return Err(MediaError::InvalidInput(format!(
            "playlist_type must be \"vod\" or \"event\", got {:?}",
            options.playlist_type
        )));
    }

    let caps = rt.capabilities()?;
    if !caps.has_muxer("hls") {
        return Err(MediaError::Unsupported(
            "hls muxer is not available in this FFmpeg build".to_string(),
        ));
    }

    let video_codec = options.video_codec.as_deref().unwrap_or("libx264");
    let audio_codec = options.audio_codec.as_deref().unwrap_or("aac");
    for (kind, codec) in [("video", video_codec), ("audio", audio_codec)] {
        if codec != "copy" && codec != "none" && !caps.has_encoder(codec) {
            return Err(MediaError::Unsupported(format!(
                "{kind} codec {codec:?} is not an encoder in this FFmpeg build"
            )));
        }
    }

    let dir = TempDir::new("hls")?;
    let in_ext = sniff_extension(input);
    let in_name = format!("input.{in_ext}");
    dir.write(&in_name, input)?;

    let m3u8_name = "output.m3u8".to_string();
    let m3u8_path = dir.file(&m3u8_name);

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
        "hls".into(),
        "-hls_time".into(),
        options.segment_secs.to_string().into(),
        "-hls_segment_type".into(),
        options.segment_type.ffmpeg_name().into(),
        "-hls_playlist_type".into(),
        options.playlist_type.clone().into(),
        "-hls_flags".into(),
        "independent_segments".into(),
    ];
    argv.push(m3u8_path.into_os_string());

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    // Collect every file ffmpeg produced besides the input.
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
            "hls packaging produced {} files, exceeding max_output_files {max_files}",
            produced.len()
        )));
    }

    let m3u8_bytes = dir.read_limited(&m3u8_name, limits.max_output_bytes)?;
    if m3u8_bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: "hls muxer produced a zero-byte playlist".to_string(),
        });
    }

    let mut extra = Vec::new();
    for name in produced {
        if name == m3u8_name {
            continue;
        }
        let bytes = dir.read_limited(&name, limits.max_output_bytes)?;
        if bytes.is_empty() {
            continue;
        }
        extra.push(MediaFile {
            content_type: content_type(&name),
            name,
            bytes,
        });
    }

    let mut result = MediaOutput {
        primary: Some(MediaFile {
            name: m3u8_name,
            bytes: m3u8_bytes,
            content_type: content_type("output.m3u8"),
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
    use crate::crates::multimedia_engine::runtime::process;

    fn clip_6s() -> Option<Vec<u8>> {
        let rt = Runtime::discover().ok()?;
        let dir = TempDir::new("hls-test").ok()?;
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
            "testsrc=duration=6:size=64x64:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=6",
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
            "-shortest",
            out.to_str()?,
        ]);
        let res = process::run("ffmpeg", &rt.ffmpeg, &args, None, &MediaLimits::default()).ok()?;
        if !res.success {
            return None;
        }
        std::fs::read(&out).ok()
    }

    #[test]
    fn hls_package_produces_m3u8_and_segments() {
        let Some(input) = clip_6s() else {
            eprintln!("skipping: ffmpeg lavfi/x264/aac unavailable");
            return;
        };
        let rt = Runtime::discover().unwrap();
        let out = hls_package(&input, &HlsOptions::default(), &MediaLimits::default(), &rt)
            .expect("hls packaging failed");

        let primary = out.primary.as_ref().expect("primary output");
        assert_eq!(primary.name, "output.m3u8");
        let playlist = String::from_utf8_lossy(&primary.bytes);
        assert!(playlist.contains("#EXTM3U"), "bad playlist:\n{playlist}");
        assert!(
            playlist.contains("#EXT-X-ENDLIST"),
            "vod playlist missing ENDLIST:\n{playlist}"
        );
        assert!(
            out.extra.iter().any(|f| f.name.ends_with(".ts")),
            "expected .ts segments, got {:?}",
            out.extra.iter().map(|f| &f.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn hls_package_rejects_bad_options() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let input = b"junk".to_vec();
        for opts in [
            HlsOptions {
                segment_secs: 0,
                ..Default::default()
            },
            HlsOptions {
                playlist_type: "live".into(),
                ..Default::default()
            },
        ] {
            assert!(hls_package(&input, &opts, &MediaLimits::default(), &rt).is_err());
        }
    }
}
