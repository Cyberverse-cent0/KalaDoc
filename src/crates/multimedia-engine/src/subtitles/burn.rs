//! Burn subtitles into the video picture.
//!
//! [`burn_subtitles`] muxes a sidecar subtitle file (SRT/ASS/VTT)
//! into the video via ffmpeg's `subtitles` filter — the text becomes
//! part of the pixels, so the output needs no player subtitle
//! support.
//!
//! # Path escaping
//!
//! The `subtitles` filter takes the subtitle file path inside a
//! filtergraph string, where `:` `,` `'` `\` `[` `]` `;` are
//! syntactic. [`escape_filter_path`] percent-escapes them; ffmpeg
//! unescapes when opening the file.
//!
//! # Styling
//!
//! [`BurnOptions::force_style`] is passed through to the filter's
//! `force_style` option verbatim (e.g. `"FontSize=24,PrimaryColour=&H00FFFF&"`).
//! Only the ASS/SSA style keys have effect — SRT inputs ignore most
//! styling because the libass renderer is what actually draws.

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`burn_subtitles`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BurnOptions {
    /// `force_style` string passed to the subtitles filter
    /// (`"FontName=Arial,FontSize=20,PrimaryColour=&H00FFFFFF&"`).
    /// `None` uses libass defaults.
    pub force_style: Option<String>,
    /// Output container extension (no dot). `None` keeps the input
    /// container when possible (re-encode required — burned subs
    /// change the pixels).
    pub output_format: Option<String>,
    /// Video codec override. `None` picks `libx264` for mp4-like
    /// containers.
    pub video_codec: Option<String>,
}

/// Escape a filesystem path for use inside an ffmpeg filtergraph
/// option value: percent-encode the filtergraph metacharacters.
fn escape_filter_path(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | ':' | '\'' | ',' | '[' | ']' | ';' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Best-effort container sniff for the media input.
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
    }
    "mp4"
}

/// Sniff the subtitle sidecar's format from its content for a
/// plausible temp extension (ffmpeg probes content anyway).
fn sniff_subtitle_ext(data: &[u8]) -> &'static str {
    let head: String = data.iter().take(64).map(|b| *b as char).collect();
    if head.contains("[Script Info]") || head.contains("[V4+ Styles]") {
        return "ass";
    }
    if head.contains("WEBVTT") {
        return "vtt";
    }
    "srt"
}

fn check_size(input: &[u8], limits: &MediaLimits) -> MediaResult<()> {
    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }
    Ok(())
}

/// Burn `subtitles` into `input`'s video stream.
///
/// `subtitles` is the sidecar file bytes (SRT/ASS/VTT — detected by
/// content). The video is re-encoded because pixels change.
///
/// # Errors
///
/// - [`MediaError::LimitExceeded`] when either input exceeds
///   `limits.max_input_bytes` or the output exceeds
///   `limits.max_output_bytes`.
/// - [`MediaError::Unsupported`] when the `subtitles` filter or the
///   chosen video encoder is absent.
/// - [`MediaError::ToolFailed`] when ffmpeg cannot decode the media
///   or render the subtitles (e.g. ASS requiring libass fonts).
pub fn burn_subtitles(
    input: &[u8],
    subtitles: &[u8],
    options: &BurnOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    check_size(input, limits)?;
    check_size(subtitles, limits)?;
    if subtitles.is_empty() {
        return Err(MediaError::InvalidInput("empty subtitle input".to_string()));
    }

    let caps = rt.capabilities()?;
    if !caps.has_filter("subtitles") {
        return Err(MediaError::Unsupported(
            "filter \"subtitles\" is not available in this FFmpeg build".to_string(),
        ));
    }

    let out_ext = options
        .output_format
        .as_deref()
        .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| sniff_extension(input).to_string());

    let video_codec = options
        .video_codec
        .as_deref()
        .unwrap_or(match out_ext.as_str() {
            "webm" => "libvpx-vp9",
            _ => "libx264",
        });
    if !caps.has_encoder(video_codec) {
        return Err(MediaError::Unsupported(format!(
            "video encoder {video_codec:?} is not available in this FFmpeg build"
        )));
    }

    let dir = TempDir::new("burn")?;
    let in_ext = sniff_extension(input);
    let in_name = format!("input.{in_ext}");
    dir.write(&in_name, input)?;

    let sub_ext = sniff_subtitle_ext(subtitles);
    let sub_name = format!("subs.{sub_ext}");
    let sub_path = dir.write(&sub_name, subtitles)?;

    let mut filter = format!("subtitles={}", escape_filter_path(&sub_path));
    if let Some(style) = &options.force_style {
        filter.push_str(&format!(":force_style='{}'", style.replace('\'', "\\'")));
    }

    let out_name = format!("output.{out_ext}");
    let mut argv: Vec<std::ffi::OsString> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-y".into(),
        "-i".into(),
        dir.file(&in_name).into_os_string(),
        "-vf".into(),
        filter.into(),
        "-c:v".into(),
        video_codec.into(),
    ];
    if out_ext == "webm" {
        argv.extend(["-c:a".into(), "libopus".into()]);
    } else {
        argv.extend(["-c:a".into(), "aac".into()]);
    }
    argv.push(dir.file(&out_name).into_os_string());

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let bytes = dir.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: "subtitle burn produced a zero-byte file".to_string(),
        });
    }

    let mut result = MediaOutput::single(out_name, bytes);
    if !out.stderr.is_empty() {
        result.warnings.push(out.stderr);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_filter_path_handles_metacharacters() {
        let p = std::path::Path::new("/tmp/a:b,c'd[1].srt");
        let escaped = escape_filter_path(p);
        assert!(escaped.contains("\\:"));
        assert!(escaped.contains("\\,"));
        assert!(escaped.contains("\\'"));
        assert!(escaped.contains("\\["));
    }

    #[test]
    fn burn_rejects_empty_subtitles() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        let err = burn_subtitles(
            b"media".as_slice(),
            b"".as_slice(),
            &BurnOptions::default(),
            &MediaLimits::default(),
            &rt,
        )
        .expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)));
    }
}
