//! Subtitle stream listing and extraction.
//!
//! [`list_subtitles`] probes the input with `ffprobe` and returns one
//! [`SubtitleInfo`] per `codec_type=subtitle` stream. [`extract_subtitles`]
//! maps a single subtitle stream (`-map 0:s:N`) and transcodes it to a
//! sidecar subtitle file (SRT, ASS/SSA, WebVTT or plain text) — useful
//! for pulling captions out of MP4/MKV/TS containers, where they are
//! often stored as `mov_text`/`hdmv_pgs_subtitle`/etc. and cannot be
//! played as standalone files.
//!
//! Subtitle codecs are text codecs, so "transcoding" here is cheap:
//! FFmpeg decodes the container's subtitle bitstream and re-encodes it
//! as the requested sidecar format.

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::types::MediaStream;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// One subtitle/caption stream found in a media file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtitleInfo {
    /// Stream index as reported by `ffprobe` (`streams[].index`) —
    /// the absolute stream number, not the subtitle ordinal.
    pub index: u32,
    /// Codec identifier (`mov_text`, `subrip`, `hdmv_pgs_subtitle`,
    /// `webvtt`, `ass`, ...).
    pub codec: String,
    /// Language tag from stream tags (`eng`, ...), when present.
    pub language: Option<String>,
    /// Stream title tag, when present.
    pub title: Option<String>,
    /// Whether the stream disposition flags it as default.
    pub is_default: bool,
}

/// Sidecar subtitle format produced by [`extract_subtitles`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubtitleFormat {
    /// SubRip `.srt` — the most widely supported sidecar format.
    #[default]
    Srt,
    /// Advanced SubStation Alpha `.ass`/`.ssa` — rich styling.
    Ass,
    /// WebVTT `.vtt` — web-native captions.
    Vtt,
    /// Plain text `.txt` — unstyled text transcript (SRT payload,
    /// `.txt` name/content-type).
    Txt,
}

impl SubtitleFormat {
    /// FFmpeg subtitle encoder name (`-c:s`).
    fn codec(self) -> &'static str {
        match self {
            // FFmpeg has no dedicated "txt" subtitle codec — plain text
            // output is an SRT bitstream without timing markup of its
            // own; the SRT encoder is the closest honest mapping.
            Self::Srt | Self::Txt => "srt",
            Self::Ass => "ass",
            Self::Vtt => "webvtt",
        }
    }

    /// FFmpeg muxer name (`-f`) for the sidecar file.
    fn muxer(self) -> &'static str {
        match self {
            Self::Srt | Self::Txt => "srt",
            Self::Ass => "ass",
            Self::Vtt => "webvtt",
        }
    }

    /// Output file extension (no dot).
    fn extension(self) -> &'static str {
        match self {
            Self::Srt => "srt",
            Self::Ass => "ass",
            Self::Vtt => "vtt",
            Self::Txt => "txt",
        }
    }

    /// MIME type for the produced [`MediaFile`](crate::crates::multimedia_engine::MediaFile).
    fn content_type(self) -> &'static str {
        match self {
            Self::Srt => "application/x-subrip",
            Self::Ass => "text/x-ssa",
            Self::Vtt => "text/vtt",
            Self::Txt => "text/plain",
        }
    }
}

/// Options for [`extract_subtitles`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubtitleOptions {
    /// Which subtitle stream to extract, given as the
    /// [`SubtitleInfo::index`] (ffprobe stream index) from
    /// [`list_subtitles`]. `None` extracts the first subtitle stream.
    pub index: Option<u32>,
    /// Desired sidecar format. Defaults to [`SubtitleFormat::Srt`].
    pub output_format: SubtitleFormat,
}

/// Best-effort container sniffing for the temp input file — same
/// approach as the other ops: ffprobe/ffmpeg re-probe the content
/// itself, the extension is only a demuxer hint.
fn sniff_extension(input: &[u8]) -> &'static str {
    if input.len() >= 4 {
        match &input[..4] {
            [0x1A, 0x45, 0xDF, 0xA3] => return "mkv", // EBML
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
        // ISO-BMFF: bytes 4..8 are "ftyp".
        if input.len() >= 8 && &input[4..8] == b"ftyp" {
            let brand = &input[8..12.min(input.len())];
            return match brand {
                b"qt  " => "mov",
                _ => "mp4",
            };
        }
    }
    "bin"
}

fn check_input_size(input: &[u8], limits: &MediaLimits) -> MediaResult<()> {
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

/// Run `ffprobe -show_streams` and return the raw subtitle streams
/// (still carrying absolute stream index and subtitle ordinal).
fn subtitle_streams(
    input_path: &std::path::Path,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<Vec<(usize, MediaStream)>> {
    let args = process::args(&[
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_streams",
        input_path.to_str().unwrap_or("input"),
    ]);
    let out = process::run("ffprobe", &rt.ffprobe, &args, None, limits)?;
    if !out.success || out.stdout.is_empty() {
        let detail = if out.stderr.is_empty() {
            "ffprobe produced no output".to_string()
        } else {
            out.stderr
        };
        return Err(MediaError::InvalidInput(detail));
    }
    let report: crate::crates::multimedia_engine::probe::types::ProbeReport =
        serde_json::from_slice(&out.stdout)
            .map_err(|e| MediaError::InvalidInput(format!("ffprobe JSON parse failed: {e}")))?;

    // `-map 0:s:N` addresses the N-th *subtitle* stream (0-based), so
    // keep the ordinal alongside the absolute stream index.
    let mut ordinal = 0usize;
    let mut subs = Vec::new();
    for stream in report.streams {
        if stream.kind() == Some("subtitle") {
            subs.push((ordinal, stream));
            ordinal += 1;
        }
    }
    Ok(subs)
}

/// List the subtitle/caption streams in `input`.
///
/// # Arguments
///
/// - `input` — complete media file bytes.
/// - `limits` — per-op limits (input cap, timeout).
/// - `rt` — the discovered FFmpeg [`Runtime`] (provides the `ffprobe`
///   binary path).
///
/// # Errors
///
/// - [`MediaError::LimitExceeded`] when `input` exceeds
///   `limits.max_input_bytes`.
/// - [`MediaError::InvalidInput`] when ffprobe fails or returns
///   unparseable output.
pub fn list_subtitles(
    input: &[u8],
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<Vec<SubtitleInfo>> {
    check_input_size(input, limits)?;

    let dir = TempDir::new("sublist")?;
    let input_name = format!("input.{}", sniff_extension(input));
    let input_path = dir.write(&input_name, input)?;

    let subs = subtitle_streams(&input_path, limits, rt)?;
    Ok(subs
        .into_iter()
        .map(|(_, s)| SubtitleInfo {
            index: s.index.unwrap_or(0),
            is_default: s.is_default(),
            codec: s.codec_name.unwrap_or_default(),
            language: s
                .language
                .clone()
                .or_else(|| s.tags.get("language").cloned()),
            title: s.title.clone().or_else(|| s.tags.get("title").cloned()),
        })
        .collect())
}

/// Extract one subtitle stream from `input` into a sidecar file.
///
/// `options.index` selects the stream by its [`SubtitleInfo::index`]
/// (ffprobe stream index); `None` picks the first subtitle stream.
/// The selected stream is mapped with `-map 0:s:N` and transcoded to
/// `options.output_format` — FFmpeg handles container-native codecs
/// like `mov_text` by decoding and re-encoding to `srt`/`ass`/
/// `webvtt` automatically.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] when the input has no subtitle
///   streams, or `options.index` matches none of them.
/// - [`MediaError::Unsupported`] when this FFmpeg build lacks the
///   requested subtitle encoder or muxer.
/// - [`MediaError::ToolFailed`] when ffmpeg cannot decode/re-encode
///   the stream (e.g. bitmap subtitles like PGS have no text
///   conversion path in most builds).
pub fn extract_subtitles(
    input: &[u8],
    options: &SubtitleOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    check_input_size(input, limits)?;

    let caps = rt.capabilities()?;
    let codec = options.output_format.codec();
    let muxer = options.output_format.muxer();
    if !caps.has_encoder(codec) {
        return Err(MediaError::Unsupported(format!(
            "subtitle encoder {codec:?} is not available in this FFmpeg build"
        )));
    }
    if !caps.has_muxer(muxer) {
        return Err(MediaError::Unsupported(format!(
            "subtitle muxer {muxer:?} is not available in this FFmpeg build"
        )));
    }

    let dir = TempDir::new("subx")?;
    let input_name = format!("input.{}", sniff_extension(input));
    let input_path = dir.write(&input_name, input)?;
    let subs = subtitle_streams(&input_path, limits, rt)?;

    // Resolve the request to a subtitle ordinal for `-map 0:s:N`.
    let (ordinal, stream) = match options.index {
        Some(want) => subs
            .iter()
            .find(|(_, s)| s.index.unwrap_or(0) == want)
            .map(|(o, s)| (*o, s.clone()))
            .ok_or_else(|| {
                MediaError::InvalidInput(format!(
                    "no subtitle stream with index {want} ({} subtitle stream(s) present)",
                    subs.len()
                ))
            })?,
        None => subs.first().map(|(o, s)| (*o, s.clone())).ok_or_else(|| {
            MediaError::InvalidInput("input has no subtitle streams to extract".to_string())
        })?,
    };
    let _ = stream; // codec already probed; kept for future tag passthrough

    let ext = options.output_format.extension();
    let out_name = format!("output.{ext}");
    let out_path = dir.file(&out_name);

    let args = process::args(&[
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        "-i",
        input_path.to_str().unwrap_or("input"),
        "-map",
        &format!("0:s:{ordinal}"),
        "-c:s",
        codec,
        "-f",
        muxer,
        out_path.to_str().unwrap_or("output"),
    ]);

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, limits)?;

    let bytes = dir.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: "subtitle extraction produced a zero-byte file".to_string(),
        });
    }

    let mut result = MediaOutput::single(out_name, bytes);
    if let Some(primary) = result.primary.as_mut() {
        primary.content_type = Some(options.output_format.content_type().to_string());
    }
    if !out.stderr.is_empty() {
        result.warnings.push(out.stderr);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::tempdir::TempDir;
    use std::ffi::OsString;

    /// Build an mp4 with one subtitle stream muxed as `mov_text`:
    /// a 1s 64x64 testsrc video + 440Hz sine audio, plus a two-cue
    /// SRT sidecar. Returns `None` when ffmpeg or the needed encoders
    /// are unavailable so tests skip gracefully on minimal builds.
    fn mp4_with_mov_text_subs() -> Option<Vec<u8>> {
        let rt = Runtime::discover().ok()?;
        let caps = rt.capabilities().ok()?;
        if !(caps.has_encoder("libx264") && caps.has_encoder("aac")) {
            return None;
        }

        let dir = TempDir::new("subx-test").ok()?;

        // Base video+audio mp4.
        let base = dir.file("base.mp4");
        let args = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x64:rate=10:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
            base.to_str()?,
        ]);
        process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, &MediaLimits::default()).ok()?;

        // SRT sidecar to mux in.
        let srt = "1\n00:00:00,000 --> 00:00:00,500\nHello subtitles\n\n\
                   2\n00:00:00,500 --> 00:00:01,000\nSecond cue\n";
        let srt_path = dir.file("sub.srt");
        std::fs::write(&srt_path, srt).ok()?;

        // Mux: copy A/V, convert the SRT to mov_text inside the mp4.
        let muxed = dir.file("muxed.mp4");
        let args = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-i",
            base.to_str()?,
            "-i",
            srt_path.to_str()?,
            "-c",
            "copy",
            "-c:s",
            "mov_text",
            muxed.to_str()?,
        ]);
        process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, &MediaLimits::default()).ok()?;
        std::fs::read(&muxed).ok()
    }

    #[test]
    fn list_and_extract_mov_text_to_srt() {
        let Some(mp4) = mp4_with_mov_text_subs() else {
            eprintln!("skipping: ffmpeg lavfi/x264/aac/mov_text unavailable");
            return;
        };
        let rt = Runtime::discover().unwrap();
        let limits = MediaLimits::default();

        let subs = list_subtitles(&mp4, &limits, &rt).expect("list_subtitles");
        assert_eq!(subs.len(), 1, "expected one subtitle stream: {subs:?}");
        let info = &subs[0];
        assert_eq!(info.codec, "mov_text");
        assert!(info.index > 0, "subtitle stream should have nonzero index");

        // Default extraction (first subtitle, SRT format).
        let opts = SubtitleOptions::default();
        let out = extract_subtitles(&mp4, &opts, &limits, &rt).expect("extract_subtitles");
        let primary = out.primary.as_ref().expect("primary output");
        assert_eq!(primary.name, "output.srt");
        assert_eq!(
            primary.content_type.as_deref(),
            Some("application/x-subrip")
        );

        let text = String::from_utf8_lossy(&primary.bytes);
        // SRT cues carry `HH:MM:SS,mmm` timestamps on line 2.
        assert!(
            text.contains("1\n00:00:00,000 -->"),
            "unexpected srt:\n{text}"
        );
        assert!(text.contains("Hello subtitles"), "unexpected srt:\n{text}");
    }

    #[test]
    fn extract_to_webvtt_and_select_by_index() {
        let Some(mp4) = mp4_with_mov_text_subs() else {
            eprintln!("skipping: ffmpeg lavfi/x264/aac/mov_text unavailable");
            return;
        };
        let rt = Runtime::discover().unwrap();
        let limits = MediaLimits::default();

        let subs = list_subtitles(&mp4, &limits, &rt).expect("list_subtitles");
        assert_eq!(subs.len(), 1);

        let opts = SubtitleOptions {
            index: Some(subs[0].index),
            output_format: SubtitleFormat::Vtt,
        };
        let out = extract_subtitles(&mp4, &opts, &limits, &rt).expect("extract vtt");
        let primary = out.primary.as_ref().expect("primary output");
        assert_eq!(primary.name, "output.vtt");
        assert_eq!(primary.content_type.as_deref(), Some("text/vtt"));

        let text = String::from_utf8_lossy(&primary.bytes);
        assert!(text.starts_with("WEBVTT"), "unexpected vtt:\n{text}");
        // WebVTT timestamps use `HH:MM:SS.mmm` (dot, not comma).
        assert!(
            text.contains("00:00:00.000 --> 00:00:00.500"),
            "unexpected vtt:\n{text}"
        );
    }

    #[test]
    fn extract_fails_without_subtitles_and_bad_index() {
        // An mp4 with no subtitle stream at all.
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => {
                eprintln!("skipping: ffmpeg/ffprobe not on PATH");
                return;
            }
        };
        let caps = match rt.capabilities() {
            Ok(c) => c,
            Err(_) => return,
        };
        if !caps.has_encoder("libx264") || !caps.has_encoder("aac") {
            eprintln!("skipping: libx264/aac encoders unavailable");
            return;
        }

        let dir = TempDir::new("subx-nosub").unwrap();
        let out_path = dir.file("nosub.mp4");
        let args: Vec<OsString> = [
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x64:rate=10:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
            out_path.to_str().unwrap(),
        ]
        .iter()
        .map(OsString::from)
        .collect();
        process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, &MediaLimits::default())
            .expect("fixture encode failed");
        let mp4 = std::fs::read(&out_path).unwrap();

        let limits = MediaLimits::default();
        let subs = list_subtitles(&mp4, &limits, &rt).expect("list_subtitles");
        assert!(subs.is_empty(), "expected no subtitles: {subs:?}");

        let opts = SubtitleOptions::default();
        let err = extract_subtitles(&mp4, &opts, &limits, &rt).expect_err("must fail");
        assert!(
            matches!(err, MediaError::InvalidInput(_)),
            "unexpected error: {err:?}"
        );

        // Bad explicit index on a file that does have subtitles.
        let Some(with_subs) = mp4_with_mov_text_subs() else {
            eprintln!("skipping bad-index check: fixture unavailable");
            return;
        };
        let bad = SubtitleOptions {
            index: Some(9999),
            ..Default::default()
        };
        let err = extract_subtitles(&with_subs, &bad, &limits, &rt).expect_err("must fail");
        assert!(
            matches!(err, MediaError::InvalidInput(_)),
            "unexpected error: {err:?}"
        );
    }
}
