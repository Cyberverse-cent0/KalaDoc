//! Audio extraction and audio-to-audio conversion.
//!
//! Writes the input bytes to a temp file, probes it with `ffprobe`
//! (rejecting inputs with no audio stream before invoking `ffmpeg`),
//! then runs `ffmpeg -map 0:a:<idx> -c:a <codec> -f <muxer> out`.
//! Video is never carried over: the op maps only the selected audio
//! stream and always passes `-vn`.

use std::ffi::OsString;
use std::path::Path;

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::types::ProbeReport;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`extract_audio`].
#[derive(Debug, Clone)]
pub struct ExtractAudioOptions {
    /// Output container/format: `"mp3"`, `"aac"`, `"wav"`, `"flac"`,
    /// `"ogg"` or `"opus"`. Anything else is rejected with
    /// [`MediaError::Unsupported`].
    pub output_format: String,
    /// Audio codec to encode with. `None` picks a format-appropriate
    /// default; `"copy"` remuxes the stream without re-encoding.
    /// Any other value must be a known FFmpeg *encoder* in this
    /// build — never silently substituted.
    pub audio_codec: Option<String>,
    /// Bit rate passed verbatim to `-b:a` (e.g. `"192k"`, `"256k"`).
    /// Ignored for lossless formats and `copy`.
    pub bitrate: Option<String>,
    /// Which audio stream to extract (`0` = first). Streams are
    /// counted among `codec_type == "audio"` entries only.
    pub stream_index: Option<u32>,
    /// Whether to drop video. Always honoured as `true` semantics —
    /// this op only ever maps audio — but the flag is kept for API
    /// symmetry with other conversion ops.
    pub strip_video: bool,
}

/// One supported output format: container extension, FFmpeg muxer
/// name, default encoder preference list and MIME type.
struct FormatSpec {
    ext: &'static str,
    muxer: &'static str,
    /// Encoder names tried in order when `audio_codec` is `None`.
    default_encoders: &'static [&'static str],
    mime: &'static str,
}

fn format_spec(format: &str) -> MediaResult<FormatSpec> {
    let spec = match format.to_ascii_lowercase().as_str() {
        "mp3" => FormatSpec {
            ext: "mp3",
            muxer: "mp3",
            default_encoders: &["libmp3lame", "mp3"],
            mime: "audio/mpeg",
        },
        "aac" => FormatSpec {
            ext: "aac",
            muxer: "adts",
            default_encoders: &["aac", "libfdk_aac"],
            mime: "audio/aac",
        },
        "wav" => FormatSpec {
            ext: "wav",
            muxer: "wav",
            default_encoders: &["pcm_s16le"],
            mime: "audio/wav",
        },
        "flac" => FormatSpec {
            ext: "flac",
            muxer: "flac",
            default_encoders: &["flac"],
            mime: "audio/flac",
        },
        "ogg" => FormatSpec {
            ext: "ogg",
            muxer: "ogg",
            default_encoders: &["libvorbis", "vorbis"],
            mime: "audio/ogg",
        },
        "opus" => FormatSpec {
            ext: "opus",
            muxer: "opus",
            default_encoders: &["libopus", "opus"],
            mime: "audio/ogg",
        },
        other => {
            return Err(MediaError::Unsupported(format!(
                "unsupported audio output format \"{other}\" \
                 (expected mp3, aac, wav, flac, ogg or opus)"
            )));
        }
    };
    Ok(spec)
}

/// Guess a container extension for the temp input file from ffprobe's
/// `format_name`. ffmpeg probes content anyway; the extension mainly
/// helps raw/extension-sensitive streams.
fn input_extension(report: &ProbeReport) -> &'static str {
    let Some(name) = report.format.as_ref().and_then(|f| f.primary_name()) else {
        return "bin";
    };
    match name {
        "mov" | "mp4" | "m4a" | "3gp" | "3g2" | "mj2" => "mp4",
        "matroska" | "webm" => "mkv",
        "mpegts" => "ts",
        "mpeg" | "mpg" | "mpegvideo" => "mpg",
        "avi" => "avi",
        "flv" | "live_flv" => "flv",
        "asf" | "wmv" | "wma" => "asf",
        "mp3" => "mp3",
        "wav" => "wav",
        "flac" => "flac",
        "ogg" => "ogg",
        "aac" => "aac",
        "caf" => "caf",
        "au" => "au",
        "aiff" => "aiff",
        _ => "bin",
    }
}

/// Run `ffprobe -show_format -show_streams -of json` on `path`.
/// Any failure means the input is not recognizable media.
fn probe(path: &Path, limits: &MediaLimits, rt: &Runtime) -> MediaResult<ProbeReport> {
    let path_str = path.to_string_lossy().into_owned();
    let args = process::args(&[
        "-v",
        "error",
        "-of",
        "json",
        "-show_format",
        "-show_streams",
        &path_str,
    ]);
    let out =
        process::run_checked("ffprobe", &rt.ffprobe, &args, None, limits).map_err(|e| match e {
            MediaError::ToolFailed { detail, .. } => {
                MediaError::InvalidInput(format!("cannot probe input: {detail}"))
            }
            other => other,
        })?;
    serde_json::from_slice::<ProbeReport>(&out.stdout)
        .map_err(|e| MediaError::InvalidInput(format!("ffprobe returned unparseable JSON: {e}")))
}

/// Resolve the codec to pass to `-c:a`, validating against the
/// runtime capability registry.
fn resolve_codec(
    codec: &Option<String>,
    spec: &FormatSpec,
    caps: &crate::crates::multimedia_engine::runtime::discovery::RuntimeCapabilities,
) -> MediaResult<String> {
    match codec.as_deref() {
        Some(name) if name.eq_ignore_ascii_case("copy") => Ok("copy".to_string()),
        Some(name) => {
            if caps.has_encoder(name) {
                Ok(name.to_string())
            } else {
                Err(MediaError::Unsupported(format!(
                    "audio codec \"{name}\" is not an encoder in this FFmpeg build"
                )))
            }
        }
        None => {
            for &name in spec.default_encoders {
                if caps.has_encoder(name) {
                    return Ok(name.to_string());
                }
            }
            Err(MediaError::Unsupported(format!(
                "no usable audio encoder for format \"{}\" (tried: {})",
                spec.ext,
                spec.default_encoders.join(", ")
            )))
        }
    }
}

/// Extract one audio track from `input` into the requested format.
///
/// The input may be a video container or an audio-only file
/// (audio→audio conversion). Exactly one file is produced.
pub fn extract_audio(
    input: &[u8],
    options: &ExtractAudioOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if let Some(max) = limits.max_input_bytes
        && input.len() as u64 > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }
    if let Some(max_files) = limits.max_output_files
        && max_files < 1
    {
        return Err(MediaError::LimitExceeded(
            "max_output_files is 0; cannot produce output".to_string(),
        ));
    }

    let spec = format_spec(&options.output_format)?;
    let caps = rt.capabilities()?;
    if !caps.has_muxer(spec.muxer) {
        return Err(MediaError::Unsupported(format!(
            "muxer \"{}\" (for {} output) is not in this FFmpeg build",
            spec.muxer, spec.ext
        )));
    }
    let codec = resolve_codec(&options.audio_codec, &spec, caps)?;
    let stream_index = options.stream_index.unwrap_or(0);

    let tmp = TempDir::new("extract-audio")?;

    // Two-pass: we need the probe to learn the container (for the temp
    // file extension) and the audio stream count before invoking
    // ffmpeg, so write a generic name first, probe, then — if the
    // extension matters — ffmpeg is tolerant of "bin" for virtually
    // all containers because it sniffs content.
    let probe_name = "input.bin";
    tmp.write(probe_name, input)?;
    let probe_path = tmp.file(probe_name);
    let report = probe(&probe_path, limits, rt)?;

    let audio_count = report
        .streams
        .iter()
        .filter(|s| s.kind() == Some("audio"))
        .count();
    if audio_count == 0 {
        return Err(MediaError::InvalidInput(
            "input contains no audio stream".to_string(),
        ));
    }
    if stream_index as usize >= audio_count {
        return Err(MediaError::InvalidInput(format!(
            "audio stream index {stream_index} out of range: input has {audio_count} audio stream(s)"
        )));
    }

    // Re-write under a container-appropriate extension when it differs;
    // cheap and helps raw/extension-sensitive demuxers.
    let in_ext = input_extension(&report);
    let in_name = if in_ext == "bin" {
        probe_name.to_string()
    } else {
        format!("input.{in_ext}")
    };
    if in_name != probe_name {
        let data = tmp.read_limited(probe_name, None)?;
        tmp.write(&in_name, &data)?;
    }
    let in_path = tmp.file(&in_name);

    let out_name = format!("output.{}", spec.ext);
    let out_path = tmp.file(&out_name);

    let mut argv: Vec<OsString> = vec![
        OsString::from("-y"),
        OsString::from("-hide_banner"),
        OsString::from("-i"),
        in_path.as_os_str().to_os_string(),
        OsString::from("-map"),
        OsString::from(format!("0:a:{stream_index}")),
        // This op never carries video; explicit even though the map
        // already excludes it.
        OsString::from("-vn"),
        OsString::from("-c:a"),
        OsString::from(codec.as_str()),
    ];
    if codec != "copy"
        && let Some(br) = options.bitrate.as_deref()
    {
        argv.push(OsString::from("-b:a"));
        argv.push(OsString::from(br));
    }
    argv.push(OsString::from("-f"));
    argv.push(OsString::from(spec.muxer));
    argv.push(out_path.as_os_str().to_os_string());

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;
    let warnings: Vec<String> = out
        .stderr
        .lines()
        .filter(|l| l.contains("deprecated") || l.contains("Past duration"))
        .map(str::to_string)
        .collect();

    let bytes = tmp.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::InvalidInput(
            "ffmpeg produced an empty audio output".to_string(),
        ));
    }

    let mut result = MediaOutput::single(out_name, bytes);
    if let Some(primary) = result.primary.as_mut() {
        primary.content_type = Some(spec.mime.to_string());
    }
    result.warnings = warnings;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::time::Duration;

    fn limits() -> MediaLimits {
        MediaLimits {
            timeout: Duration::from_secs(120),
            ..MediaLimits::default()
        }
    }

    fn runtime() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be installed")
    }

    /// Generate a 1s mp4 with mpeg4 video + 440Hz sine AAC audio via
    /// lavfi, returning the file bytes.
    fn sine_av_mp4(rt: &Runtime) -> Vec<u8> {
        let dir = TempDir::new("testgen").unwrap();
        let out_path = dir.file("in.mp4");
        let mut argv = process::args(&[
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=160x120:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:v",
            "mpeg4",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
        ]);
        argv.push(out_path.as_os_str().to_os_string());
        process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, &limits()).unwrap();
        std::fs::read(&out_path).unwrap()
    }

    /// Generate a 1s video-only mp4 (no audio stream).
    fn video_only_mp4(rt: &Runtime) -> Vec<u8> {
        let dir = TempDir::new("testgen").unwrap();
        let out_path = dir.file("video.mp4");
        let mut argv = process::args(&[
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=160x120:rate=10",
            "-c:v",
            "mpeg4",
            "-pix_fmt",
            "yuv420p",
        ]);
        argv.push(out_path.as_os_str().to_os_string());
        process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, &limits()).unwrap();
        std::fs::read(&out_path).unwrap()
    }

    #[test]
    fn extracts_mp3_with_id3_or_frame_sync() {
        let rt = runtime();
        let input = sine_av_mp4(&rt);
        let opts = ExtractAudioOptions {
            output_format: "mp3".to_string(),
            audio_codec: None,
            bitrate: Some("128k".to_string()),
            stream_index: None,
            strip_video: true,
        };
        let out = extract_audio(&input, &opts, &limits(), &rt).unwrap();
        let file = out.primary.expect("primary output");
        assert!(file.name.ends_with(".mp3"), "name: {}", file.name);
        let b = &file.bytes;
        assert!(b.len() > 100);
        // MP3: either an ID3v2 tag ("ID3") or a MPEG frame sync
        // (0xFF 0xEx/0xFx).
        let is_mp3 =
            b.starts_with(b"ID3") || (b.len() > 1 && b[0] == 0xFF && (b[1] & 0xE0) == 0xE0);
        assert!(is_mp3, "bad mp3 magic: {:02x?}", &b[..4]);
    }

    #[test]
    fn extracts_wav_with_riff_magic() {
        let rt = runtime();
        let input = sine_av_mp4(&rt);
        let opts = ExtractAudioOptions {
            output_format: "wav".to_string(),
            audio_codec: None,
            bitrate: None,
            stream_index: Some(0),
            strip_video: true,
        };
        let out = extract_audio(&input, &opts, &limits(), &rt).unwrap();
        let file = out.primary.expect("primary output");
        assert!(file.name.ends_with(".wav"), "name: {}", file.name);
        let b = &file.bytes;
        assert!(b.len() > 100);
        assert_eq!(&b[0..4], b"RIFF", "bad wav magic: {:02x?}", &b[..4]);
        assert_eq!(&b[8..12], b"WAVE", "missing WAVE tag");
    }

    #[test]
    fn rejects_input_without_audio_stream() {
        let rt = runtime();
        let input = video_only_mp4(&rt);
        let opts = ExtractAudioOptions {
            output_format: "mp3".to_string(),
            audio_codec: None,
            bitrate: None,
            stream_index: None,
            strip_video: true,
        };
        let err = extract_audio(&input, &opts, &limits(), &rt).unwrap_err();
        match err {
            MediaError::InvalidInput(msg) => {
                assert!(msg.contains("no audio"), "unexpected message: {msg}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }
}
