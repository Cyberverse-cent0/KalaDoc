//! Channel-layout conversion and sample-rate resampling.
//!
//! [`channels`] remaps audio to mono, stereo, or an arbitrary FFmpeg
//! channel layout; [`resample`] changes the sample rate. Both write
//! the input buffer to a temp file, run `ffmpeg` with an argv vector,
//! and read the produced file back under [`MediaLimits`].
//!
//! Non-audio streams are stream-copied (`-c:v copy -c:s copy -c:d
//! copy -map 0`) so video/subtitle tracks survive untouched; only
//! audio is re-encoded, which the `-ac`/`-channel_layout`/`-af`/
//! `-ar` options require.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// How [`channels`] should remap the audio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelOp {
    /// Downmix to a single channel (`-ac 1`).
    ToMono,
    /// Upmix/keep two channels (`-ac 2`).
    ToStereo,
    /// Target an explicit FFmpeg channel layout (`"5.1"`, `"7.1"`,
    /// `"quad"`, `"octagonal"`, ...).
    ///
    /// `rematrix` is an optional escape hatch: when `Some(expr)`, the
    /// op emits `-af pan=<expr>` where `expr` is a complete
    /// [`pan`](https://ffmpeg.org/ffmpeg-filters.html#pan-1) filter
    /// argument (e.g. `"stereo|c0=c0|c1=c1"`), giving full control
    /// over the channel matrix. When `None`, ffmpeg's automatic
    /// rematrixing is used via `-channel_layout <layout> -ac <n>`
    /// (up/down-mix with its built-in coefficients).
    Custom {
        /// FFmpeg channel-layout name.
        layout: String,
        /// Optional `pan` filter argument overriding the default
        /// rematrix.
        #[serde(default)]
        rematrix: Option<String>,
    },
}

/// Minimum accepted sample rate for [`resample`].
const MIN_SAMPLE_RATE: u32 = 8_000;
/// Maximum accepted sample rate for [`resample`].
const MAX_SAMPLE_RATE: u32 = 192_000;

/// Apply a channel-layout operation to the audio streams of `input`.
///
/// Runs `ffmpeg -i <in> -map 0 -c:v copy -c:s copy -c:d copy` plus the
/// options implied by `op`, then returns the produced file.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty/oversized input, or an
///   empty/unsupported `layout` in [`ChannelOp::Custom`].
/// - [`MediaError::Unsupported`] — the `pan` filter is missing from
///   this FFmpeg build when `rematrix` is requested.
/// - [`MediaError::ToolFailed`] / [`MediaError::Timeout`] — ffmpeg
///   rejected the input or layout.
pub fn channels(
    input: &[u8],
    op: &ChannelOp,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    let mut audio_args: Vec<OsString> = Vec::new();
    match op {
        ChannelOp::ToMono => {
            audio_args.push(OsString::from("-ac"));
            audio_args.push(OsString::from("1"));
        }
        ChannelOp::ToStereo => {
            audio_args.push(OsString::from("-ac"));
            audio_args.push(OsString::from("2"));
        }
        ChannelOp::Custom { layout, rematrix } => {
            let layout = layout.trim();
            if layout.is_empty() {
                return Err(MediaError::InvalidInput(
                    "channel layout must not be empty".to_string(),
                ));
            }
            if let Some(expr) = rematrix.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
                // Honest capability check: `pan` is a standard filter
                // but minimal builds can drop it.
                let caps = rt.capabilities()?;
                if !caps.has_filter("pan") {
                    return Err(MediaError::Unsupported(
                        "pan filter is not available in this FFmpeg build".to_string(),
                    ));
                }
                audio_args.push(OsString::from("-af"));
                audio_args.push(OsString::from(format!("pan={expr}")));
            } else {
                let n = channel_count_for_layout(layout).ok_or_else(|| {
                    MediaError::InvalidInput(format!(
                        "cannot determine channel count for layout '{layout}'"
                    ))
                })?;
                audio_args.push(OsString::from("-channel_layout"));
                audio_args.push(OsString::from(layout));
                audio_args.push(OsString::from("-ac"));
                audio_args.push(OsString::from(n.to_string()));
            }
        }
    }
    run_audio_op(input, &audio_args, limits, rt)
}

/// Resample the audio streams of `input` to `sample_rate` Hz.
///
/// Emits `-ar <sample_rate>`; FFmpeg's auto-inserted resampler
/// handles the conversion. `sample_rate` must be in
/// `8000..=192000`.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty/oversized input, or
///   `sample_rate` outside `8000..=192000`.
/// - [`MediaError::ToolFailed`] / [`MediaError::Timeout`] — ffmpeg
///   rejected the input or rate.
pub fn resample(
    input: &[u8],
    sample_rate: u32,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&sample_rate) {
        return Err(MediaError::InvalidInput(format!(
            "sample_rate must be in {MIN_SAMPLE_RATE}..={MAX_SAMPLE_RATE}, got {sample_rate}"
        )));
    }
    let audio_args = vec![
        OsString::from("-ar"),
        OsString::from(sample_rate.to_string()),
    ];
    run_audio_op(input, &audio_args, limits, rt)
}

/// Derive the channel count for a standard FFmpeg layout name.
///
/// Handles the named layouts (`mono`, `stereo`, `quad`,
/// `hexagonal`, `octagonal`, `downmix`) and the `N.M` numeric
/// convention (`5.1` → 5+1 = 6, `7.1` → 8, `3.0` → 3). Parenthesized
/// qualifiers (`5.1(side)`, `7.1(wide)`) are ignored.
fn channel_count_for_layout(layout: &str) -> Option<u32> {
    let base = layout
        .split('(')
        .next()
        .unwrap_or(layout)
        .trim()
        .to_ascii_lowercase();
    match base.as_str() {
        "mono" => return Some(1),
        "stereo" | "downmix" => return Some(2),
        "quad" => return Some(4),
        "hexagonal" => return Some(6),
        "octagonal" => return Some(8),
        _ => {}
    }
    // Numeric layouts: "N.M[.K...]" — first component is the full
    // channels, remaining components add (the ".1" LFE in 5.1/7.1).
    let mut count: u32 = 0;
    let mut parts = 0u32;
    for part in base.split('.') {
        let n: u32 = part.parse().ok()?;
        count = count.checked_add(n)?;
        parts += 1;
    }
    (parts > 0).then_some(count)
}

/// Shared driver: write `input` to a temp file, run ffmpeg with the
/// copy-for-non-audio + `audio_args` argv, read back `output.<ext>`.
fn run_audio_op(
    input: &[u8],
    audio_args: &[OsString],
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if input.is_empty() {
        return Err(MediaError::InvalidInput("empty input buffer".to_string()));
    }
    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }

    let dir = TempDir::new("audio-op")?;
    let input_ext = output_extension(input, rt)?;
    let input_name = format!("input{input_ext}");
    dir.write(&input_name, input)?;

    let output_name = format!("output{input_ext}");
    let output_path = dir.file(&output_name);

    let mut argv: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
        OsString::from("-i"),
        dir.file(&input_name).into_os_string(),
        OsString::from("-map"),
        OsString::from("0"),
        OsString::from("-c:v"),
        OsString::from("copy"),
        OsString::from("-c:s"),
        OsString::from("copy"),
        OsString::from("-c:d"),
        OsString::from("copy"),
    ];
    argv.extend_from_slice(audio_args);
    argv.push(output_path.into_os_string());

    process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let bytes = dir.read_limited(&output_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: format!("ffmpeg produced an empty {output_name}"),
        });
    }

    let mut out = MediaOutput::single(output_name, bytes);
    if let Some(file) = out.primary.as_mut() {
        file.content_type = content_type_for(&input_ext);
    }
    Ok(out)
}

/// Determine the output container extension: sniff magic bytes
/// first; when unrecognized, ask ffprobe for the format name and map
/// it to a writable extension.
fn output_extension(input: &[u8], _rt: &Runtime) -> MediaResult<&'static str> {
    let sniffed = sniff_extension(input);
    if sniffed != ".bin" {
        return Ok(sniffed);
    }
    let info = crate::crates::multimedia_engine::probe::inspect::inspect(
        input,
        &MediaLimits {
            // Inspect is only used for format detection here.
            timeout: std::time::Duration::from_secs(30),
            ..MediaLimits::default()
        },
    )
    .map_err(|e| MediaError::InvalidInput(format!("cannot determine input container: {e}")))?;
    let name = info.format_name.unwrap_or_default().to_ascii_lowercase();
    for (needle, ext) in [
        ("matroska", ".mkv"),
        ("webm", ".webm"),
        ("mp4", ".mp4"),
        ("mov", ".mov"),
        ("wav", ".wav"),
        ("mp3", ".mp3"),
        ("mpeg audio", ".mp3"),
        ("ogg", ".ogg"),
        ("flac", ".flac"),
        ("aac", ".aac"),
        ("mpegts", ".ts"),
        ("avi", ".avi"),
        ("aiff", ".aiff"),
        ("au", ".au"),
        ("mpeg", ".mpg"),
    ] {
        if name.contains(needle) {
            return Ok(ext);
        }
    }
    Err(MediaError::InvalidInput(format!(
        "cannot map input format '{name}' to an output container"
    )))
}

/// Sniff a container extension from magic bytes (same heuristics as
/// `probe::inspect`). Returns `".bin"` when unrecognized.
fn sniff_extension(data: &[u8]) -> &'static str {
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        return if &data[8..12] == b"qt  " {
            ".mov"
        } else {
            ".mp4"
        };
    }
    if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return ".mkv";
    }
    if data.starts_with(b"RIFF") && data.len() >= 12 {
        return match &data[8..12] {
            b"WAVE" => ".wav",
            b"AVI " => ".avi",
            _ => ".bin",
        };
    }
    if data.starts_with(b"OggS") {
        return ".ogg";
    }
    if data.starts_with(b"fLaC") {
        return ".flac";
    }
    if data.starts_with(b"ID3") || (data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xE0) == 0xE0)
    {
        return ".mp3";
    }
    ".bin"
}

/// Best-effort MIME type for common containers.
fn content_type_for(ext: &str) -> Option<String> {
    let mime = match ext {
        ".mp4" | ".m4v" => "video/mp4",
        ".mov" => "video/quicktime",
        ".mkv" => "video/x-matroska",
        ".webm" => "video/webm",
        ".avi" => "video/x-msvideo",
        ".mpg" | ".mpeg" | ".ts" => "video/mpeg",
        ".wav" => "audio/wav",
        ".mp3" => "audio/mpeg",
        ".m4a" | ".aac" => "audio/mp4",
        ".ogg" | ".opus" => "audio/ogg",
        ".flac" => "audio/flac",
        ".aiff" => "audio/aiff",
        _ => return None,
    };
    Some(mime.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::process;

    fn runtime() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be on PATH")
    }

    /// Generate a 1s stereo 44.1kHz sine WAV via lavfi.
    fn stereo_wav() -> Option<Vec<u8>> {
        let rt = runtime();
        let dir = TempDir::new("channels-test-in").ok()?;
        let out_path = dir.file("stereo.wav");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100:duration=1",
            "-ac",
            "2",
            "-c:a",
            "pcm_s16le",
            out_path.to_str()?,
        ]);
        let out = process::run("ffmpeg", &rt.ffmpeg, &argv, None, &MediaLimits::default()).ok()?;
        if !out.success {
            return None;
        }
        std::fs::read(&out_path).ok()
    }

    /// Run ffprobe on `bytes` and return the first audio stream's JSON.
    fn probe_audio(rt: &Runtime, name: &str, bytes: &[u8]) -> serde_json::Value {
        let dir = TempDir::new("channels-test-probe").expect("tempdir");
        let path = dir.write(name, bytes).expect("write probe input");
        let argv = process::args(&[
            "-v",
            "error",
            "-show_streams",
            "-of",
            "json",
            path.to_str().expect("utf8 path"),
        ]);
        let out = process::run("ffprobe", &rt.ffprobe, &argv, None, &MediaLimits::default())
            .expect("ffprobe");
        assert!(out.success, "ffprobe failed: {}", out.stderr);
        let doc: serde_json::Value = serde_json::from_slice(&out.stdout).expect("ffprobe json");
        doc["streams"]
            .as_array()
            .expect("streams array")
            .iter()
            .find(|s| s["codec_type"].as_str() == Some("audio"))
            .cloned()
            .expect("audio stream")
    }

    fn primary_bytes(out: MediaOutput) -> Vec<u8> {
        out.primary.expect("primary output").bytes
    }

    #[test]
    fn to_mono_produces_single_channel() {
        let Some(input) = stereo_wav() else {
            eprintln!("skipping: ffmpeg lavfi unavailable");
            return;
        };
        let rt = runtime();
        let out = channels(&input, &ChannelOp::ToMono, &MediaLimits::default(), &rt)
            .expect("channels ToMono");
        let file = out.primary.expect("primary output");
        assert_eq!(file.name, "output.wav");

        let stream = probe_audio(&rt, "mono.wav", &file.bytes);
        assert_eq!(stream["channels"].as_u64(), Some(1));
    }

    #[test]
    fn custom_layout_upmixes_stereo_to_5_1() {
        let Some(input) = stereo_wav() else {
            eprintln!("skipping: ffmpeg lavfi unavailable");
            return;
        };
        let rt = runtime();
        let op = ChannelOp::Custom {
            layout: "5.1".to_string(),
            rematrix: None,
        };
        let out = channels(&input, &op, &MediaLimits::default(), &rt).expect("channels 5.1");
        let stream = probe_audio(&rt, "51.wav", &primary_bytes(out));
        assert_eq!(stream["channels"].as_u64(), Some(6));
    }

    #[test]
    fn resample_changes_sample_rate() {
        let Some(input) = stereo_wav() else {
            eprintln!("skipping: ffmpeg lavfi unavailable");
            return;
        };
        let rt = runtime();
        let out = resample(&input, 22050, &MediaLimits::default(), &rt).expect("resample");
        let stream = probe_audio(&rt, "resampled.wav", &primary_bytes(out));
        assert_eq!(stream["sample_rate"].as_str(), Some("22050"));
        // Channels untouched.
        assert_eq!(stream["channels"].as_u64(), Some(2));
    }

    #[test]
    fn resample_rejects_out_of_range_rate() {
        let rt = runtime();
        for bad in [0u32, 7_999, 192_001, u32::MAX] {
            let err =
                resample(&[1, 2, 3], bad, &MediaLimits::default(), &rt).expect_err("must fail");
            assert!(
                matches!(err, MediaError::InvalidInput(_)),
                "rate {bad}: unexpected error {err:?}"
            );
        }
    }

    #[test]
    fn custom_rejects_empty_layout() {
        let rt = runtime();
        let op = ChannelOp::Custom {
            layout: "   ".to_string(),
            rematrix: None,
        };
        let err = channels(&[1, 2, 3], &op, &MediaLimits::default(), &rt).expect_err("must fail");
        assert!(
            matches!(err, MediaError::InvalidInput(_)),
            "unexpected error {err:?}"
        );
    }
}
