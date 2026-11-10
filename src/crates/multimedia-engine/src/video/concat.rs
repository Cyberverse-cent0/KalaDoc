//! Video concatenation / merging of multiple clips into one file.
//!
//! Two strategies are provided:
//!
//! - **Stream copy** ([`ConcatOptions::reencode`] = `false`, the
//!   default): inputs are written to a temp dir and joined with the
//!   FFmpeg `concat` demuxer (`-f concat -safe 0 -c copy`). Fast and
//!   lossless, but every input must share codec, pixel format, time
//!   base and audio layout — mismatches produce a broken or
//!   unreadable output. Inputs are *not* probed or normalized.
//! - **Re-encode** (`reencode` = `true`): every input is decoded and
//!   passed through a `filter_complex` `concat` filter. All video
//!   streams are scaled to a common size (the first input's probed
//!   dimensions, or 1280x720 when unknown) and a common frame rate
//!   (first input's rate, or 30 fps); audio is resampled to a common
//!   rate/layout and encoded as AAC. [`ConcatOptions::normalize`]
//!   additionally forces canonical parameters (yuv420p pixels,
//!   48 kHz stereo audio) — when `false` the first input's probed
//!   parameters are used instead.
//!
//! [`ConcatOptions::acrossfade`] is reserved for audio crossfades at
//! segment joins. A correct implementation needs per-segment audio
//! retiming so video stays aligned; that is not yet implemented, so
//! passing `Some(..)` returns [`MediaError::Unsupported`] rather than
//! silently ignoring the request.

use std::ffi::OsString;
use std::path::Path;

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaInput, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::{MediaStream, ProbeReport};
use crate::crates::multimedia_engine::runtime::discovery::{Runtime, RuntimeCapabilities};
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`concat`].
#[derive(Debug, Clone)]
pub struct ConcatOptions {
    /// Output container extension without a dot (`"mp4"`, `"mkv"`,
    /// `"webm"`, ...). `None` derives it from the first input's file
    /// name, falling back to `"mp4"`.
    pub output_format: Option<String>,
    /// Re-encode through the `concat` filter instead of stream-copy
    /// through the `concat` demuxer. Required when inputs differ in
    /// codec, resolution, frame rate or audio layout. Defaults to
    /// `false`.
    pub reencode: bool,
    /// When re-encoding, force canonical output parameters:
    /// `yuv420p` pixels, square sample aspect ratio, 48 kHz stereo
    /// AAC audio. When `false`, the first input's probed parameters
    /// are used as the concat target (the minimum unification the
    /// `concat` filter still requires: common size, fps, pixel
    /// format compatibility and audio rate/layout). Ignored on the
    /// stream-copy path. Defaults to `true`.
    pub normalize: bool,
    /// Audio crossfade duration in seconds applied at each join.
    ///
    /// **Not yet implemented** — crossfading overlaps audio segments
    /// and requires retiming video to stay in sync. Passing `Some`
    /// returns [`MediaError::Unsupported`]. `None` (default) means
    /// hard cuts.
    pub acrossfade: Option<f64>,
}

impl Default for ConcatOptions {
    fn default() -> Self {
        Self {
            output_format: None,
            reencode: false,
            normalize: true,
            acrossfade: None,
        }
    }
}

/// Concatenate `inputs` (at least two) into a single media file.
///
/// The primary output is named `output.<ext>` where `<ext>` comes
/// from [`ConcatOptions::output_format`] or the first input's name.
/// See the module docs for the copy vs. re-encode trade-offs.
pub fn concat(
    inputs: &[MediaInput],
    options: &ConcatOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if inputs.len() < 2 {
        return Err(MediaError::InvalidInput(
            "concat requires at least 2 inputs".to_string(),
        ));
    }
    if options.acrossfade.is_some() {
        return Err(MediaError::Unsupported(
            "acrossfade is not yet implemented (audio crossfades need per-segment \
             retiming); use hard cuts or pre-render fades"
                .to_string(),
        ));
    }
    for (i, input) in inputs.iter().enumerate() {
        if input.data.is_empty() {
            return Err(MediaError::InvalidInput(format!(
                "input {i} ({}) is empty",
                input.name
            )));
        }
        if let Some(max) = limits.max_input_bytes
            && (input.data.len() as u64) > max
        {
            return Err(MediaError::LimitExceeded(format!(
                "input {i} ({} bytes) exceeds max_input_bytes {max}",
                input.data.len()
            )));
        }
    }

    let out_ext = options
        .output_format
        .as_deref()
        .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ext_of(&inputs[0].name));

    // Capability checks up front — never invoke filters/codecs this
    // FFmpeg build does not have.
    let caps = rt.capabilities()?;
    if !caps.has_demuxer("concat") {
        return Err(MediaError::Unsupported(
            "ffmpeg build lacks the 'concat' demuxer".to_string(),
        ));
    }
    let muxer = muxer_name(&out_ext);
    if !caps.has_muxer(muxer) {
        return Err(MediaError::Unsupported(format!(
            "ffmpeg build lacks the '{muxer}' muxer for .{out_ext} output"
        )));
    }
    if options.reencode {
        for name in ["concat", "scale", "fps", "setsar"] {
            if !caps.has_filter(name) {
                return Err(MediaError::Unsupported(format!(
                    "ffmpeg build lacks the '{name}' filter required for re-encode concat"
                )));
            }
        }
        if pick_video_encoder(caps).is_none() {
            return Err(MediaError::Unsupported(
                "ffmpeg build lacks an H.264 encoder (libx264/libopenh264/h264/mpeg4) required \
                 for re-encode concat"
                    .to_string(),
            ));
        }
        if !caps.has_encoder("aac") {
            return Err(MediaError::Unsupported(
                "ffmpeg build lacks the 'aac' encoder required for re-encode concat audio"
                    .to_string(),
            ));
        }
    }

    let tmp = TempDir::new("concat")?;

    // Materialize inputs with their real extensions so ffmpeg probes
    // each container correctly.
    let mut paths = Vec::with_capacity(inputs.len());
    for (i, input) in inputs.iter().enumerate() {
        let name = format!("in{i:03}.{}", ext_of(&input.name));
        paths.push(tmp.write(&name, &input.data)?);
    }

    let out_name = format!("output.{out_ext}");
    let out_path = tmp.file(&out_name);

    let video_encoder = if options.reencode {
        pick_video_encoder(caps).expect("checked above")
    } else {
        ""
    };

    if options.reencode {
        concat_reencode(&paths, &out_path, options, video_encoder, limits, rt)?;
    } else {
        concat_copy(&paths, &out_path, limits, rt)?;
    }

    let bytes = tmp.read_limited(&out_name, limits.max_output_bytes)?;
    let mut output = MediaOutput::single(out_name, bytes);
    if let Some(primary) = output.primary.as_mut() {
        primary.content_type = content_type(&out_ext).map(str::to_string);
    }
    Ok(output)
}

/// Stream-copy concat via the `concat` demuxer list file.
fn concat_copy(
    paths: &[std::path::PathBuf],
    out_path: &Path,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<()> {
    let tmp = TempDir::new("concat-list").ok();
    // The list file must live next to nothing in particular; keep it
    // in its own temp dir so paths stay absolute and `-safe 0` is
    // only needed because inputs live outside the list's directory.
    let list_dir = match tmp {
        Some(d) => d,
        None => {
            return Err(MediaError::Io(std::io::Error::other(
                "could not create concat list temp dir",
            )));
        }
    };
    let mut list = String::new();
    for path in paths {
        // ffmpeg concat escaping: a single quote is written '\''.
        let escaped = path.to_string_lossy().replace('\'', "'\\''");
        list.push_str(&format!("file '{escaped}'\n"));
    }
    let list_path = list_dir.file("list.txt");
    std::fs::write(&list_path, list)?;

    let args: Vec<OsString> = [
        OsString::from("-hide_banner"),
        OsString::from("-y"),
        OsString::from("-f"),
        OsString::from("concat"),
        OsString::from("-safe"),
        OsString::from("0"),
        OsString::from("-i"),
        list_path.into_os_string(),
        OsString::from("-c"),
        OsString::from("copy"),
        out_path.as_os_str().to_os_string(),
    ]
    .into();

    process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, limits)?;
    Ok(())
}

/// Re-encode concat: normalize every input to common parameters, then
/// join with the `concat` filter inside one `filter_complex`.
/// H.264 encoders acceptable for re-encode concat, in preference
/// order.
const VIDEO_ENCODERS: &[&str] = &["libx264", "libopenh264", "h264", "mpeg4"];

/// First available H.264 encoder in this build.
fn pick_video_encoder<'a>(caps: &'a RuntimeCapabilities) -> Option<&'a str> {
    VIDEO_ENCODERS
        .iter()
        .copied()
        .find(|e| caps.has_encoder(e))
}

fn concat_reencode(
    paths: &[std::path::PathBuf],
    out_path: &Path,
    options: &ConcatOptions,
    video_encoder: &str,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<()> {
    // Probe every input: we need per-stream presence (video required,
    // audio optional-but-uniform) and the first input's parameters as
    // the concat target.
    let mut reports = Vec::with_capacity(paths.len());
    for path in paths {
        reports.push(probe_file(path, rt, limits)?);
    }

    fn stream_of<'a>(r: &'a ProbeReport, kind: &str) -> Option<&'a MediaStream> {
        r.streams.iter().find(|s| s.kind() == Some(kind))
    }
    fn video_of<'a>(r: &'a ProbeReport) -> Option<&'a MediaStream> {
        stream_of(r, "video")
    }
    fn audio_of<'a>(r: &'a ProbeReport) -> Option<&'a MediaStream> {
        stream_of(r, "audio")
    }

    for (i, r) in reports.iter().enumerate() {
        if video_of(r).is_none() {
            return Err(MediaError::InvalidInput(format!(
                "input {i} has no video stream; concat requires video on every input"
            )));
        }
    }
    // Audio is included only when *every* input has an audio stream —
    // mixing audio-bearing and silent segments would need anullsrc
    // padding per segment (not implemented; documented limitation).
    let with_audio = reports.iter().all(|r| audio_of(r).is_some());

    // Concat target parameters: first input's probed values, with
    // documented fallbacks.
    let first_video = video_of(&reports[0]);
    let (width, height) = first_video
        .and_then(|v| v.width.zip(v.height))
        .unwrap_or((1280, 720));
    let fps = first_video
        .and_then(|v| v.r_frame_rate.as_deref().or(v.avg_frame_rate.as_deref()))
        .filter(|s| valid_rate(s))
        .unwrap_or("30")
        .to_string();
    let first_audio = audio_of(&reports[0]);
    let sample_rate = if options.normalize {
        48000
    } else {
        first_audio
            .and_then(|a| a.sample_rate.as_deref())
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(48000)
    };
    let channel_layout = if options.normalize {
        "stereo".to_string()
    } else {
        first_audio
            .and_then(|a| a.channel_layout.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "stereo".to_string())
    };

    // Build the filter graph. Every video pad goes through scale+fps+
    // setsar (the concat filter requires identical geometry/fps/SAR);
    // `normalize` adds the canonical pixel format. Audio pads get
    // aresample + aformat for the same reason.
    let n = paths.len();
    let a_flag: usize = usize::from(with_audio);
    let mut graph = String::new();
    for i in 0..n {
        graph.push_str(&format!("[{i}:v]scale={width}:{height},fps={fps},setsar=1"));
        if options.normalize {
            graph.push_str(",format=yuv420p");
        }
        graph.push_str(&format!("[v{i}];"));
        if with_audio {
            graph.push_str(&format!("[{i}:a]aresample={sample_rate}"));
            if options.normalize {
                graph.push_str(",aformat=sample_fmts=fltp:channel_layouts=stereo");
            } else {
                graph.push_str(&format!(",aformat=channel_layouts={channel_layout}"));
            }
            graph.push_str(&format!("[a{i}];"));
        }
    }
    for i in 0..n {
        graph.push_str(&format!("[v{i}]"));
        if with_audio {
            graph.push_str(&format!("[a{i}]"));
        }
    }
    graph.push_str(&format!("concat=n={n}:v=1:a={a_flag}[outv]"));
    if with_audio {
        graph.push_str("[outa]");
    }

    let mut args: Vec<OsString> = vec![OsString::from("-hide_banner"), OsString::from("-y")];
    for path in paths {
        args.push(OsString::from("-i"));
        args.push(path.as_os_str().to_os_string());
    }
    args.extend([
        OsString::from("-filter_complex"),
        OsString::from(graph),
        OsString::from("-map"),
        OsString::from("[outv]"),
        OsString::from("-c:v"),
        OsString::from(video_encoder),
        OsString::from("-pix_fmt"),
        OsString::from("yuv420p"),
    ]);
    if video_encoder == "libx264" {
        args.extend([
            OsString::from("-preset"),
            OsString::from("veryfast"),
            OsString::from("-crf"),
            OsString::from("23"),
        ]);
    }
    if with_audio {
        args.extend([
            OsString::from("-map"),
            OsString::from("[outa]"),
            OsString::from("-c:a"),
            OsString::from("aac"),
            OsString::from("-b:a"),
            OsString::from("128k"),
        ]);
    }
    args.push(out_path.as_os_str().to_os_string());

    process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, limits)?;
    Ok(())
}

/// Probe a media file with ffprobe, returning the parsed report.
fn probe_file(path: &Path, rt: &Runtime, limits: &MediaLimits) -> MediaResult<ProbeReport> {
    let mut args = process::args(&[
        "-v",
        "error",
        "-show_format",
        "-show_streams",
        "-of",
        "json",
    ]);
    args.push(path.as_os_str().to_os_string());
    let out = process::run_checked("ffprobe", &rt.ffprobe, &args, None, limits)?;
    serde_json::from_slice(&out.stdout)
        .map_err(|e| MediaError::InvalidInput(format!("ffprobe returned unparseable JSON: {e}")))
}

/// File extension (lowercased, no dot) from a display name; `"mp4"`
/// when the name has none or the extension looks unusable.
fn ext_of(name: &str) -> String {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if ext.is_empty() || ext.len() > 10 || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        "mp4".to_string()
    } else {
        ext
    }
}

/// Whether `s` looks like an ffprobe frame-rate string (`"30"`,
/// `"30000/1001"`) usable directly as an `fps=` filter value.
fn valid_rate(s: &str) -> bool {
    let (num, den) = s.split_once('/').unwrap_or((s, "1"));
    num.trim().parse::<f64>().is_ok() && den.trim().parse::<f64>().is_ok()
}

/// Map a file extension to the ffmpeg muxer name used for capability
/// checks (`mkv` files are muxed by the `matroska` muxer, etc.).
fn muxer_name(ext: &str) -> &'static str {
    match ext {
        "mp4" | "m4v" | "mov" => "mp4_mov",
        "mkv" => "matroska",
        "webm" => "webm",
        "avi" => "avi",
        "wav" => "wav",
        "mp3" => "mp3",
        "flac" => "flac",
        "ogg" => "ogg",
        "opus" => "opus",
        "m4a" => "ipod",
        "aac" => "adts",
        _ => "mp4",
    }
}

/// Best-effort MIME type for the output container.
fn content_type(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "mp4" | "m4v" | "mov" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" | "opus" => "audio/ogg",
        "m4a" | "aac" => "audio/mp4",
        _ => return None,
    })
}

// `muxer_name` returns composite names for the mp4/mov family because
// ffmpeg registers a single `mp4` muxer covering both; keep the match
// honest by checking the real muxer name instead.
#[allow(dead_code)]
fn _muxer_name_note() {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn rt() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be on PATH for tests")
    }

    fn limits() -> MediaLimits {
        MediaLimits {
            timeout: Duration::from_secs(120),
            ..MediaLimits::default()
        }
    }

    /// Render a 1s 320x240@30 clip with a sine tone via lavfi.
    fn make_clip(dir: &TempDir, name: &str, freq: u32, rt: &Runtime) -> MediaInput {
        let path: PathBuf = dir.file(name);
        let caps = rt.capabilities().expect("capabilities");
        let video_enc = pick_video_encoder(&caps).expect("no usable video encoder in test build");
        let mut args: Vec<OsString> = [
            OsString::from("-hide_banner"),
            OsString::from("-y"),
            OsString::from("-f"),
            OsString::from("lavfi"),
            OsString::from("-i"),
            OsString::from(format!("testsrc=duration=1:size=320x240:rate=30")),
            OsString::from("-f"),
            OsString::from("lavfi"),
            OsString::from("-i"),
            OsString::from(format!("sine=frequency={freq}:duration=1")),
            OsString::from("-c:v"),
            OsString::from(video_enc),
            OsString::from("-pix_fmt"),
            OsString::from("yuv420p"),
            OsString::from("-c:a"),
            OsString::from("aac"),
            OsString::from("-shortest"),
        ]
        .into();
        args.push(path.as_os_str().to_os_string());
        process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, &limits())
            .expect("failed to generate test clip");
        MediaInput {
            name: name.to_string(),
            data: std::fs::read(&path).expect("clip written"),
        }
    }

    /// Write output bytes to a temp file and return its probed
    /// container duration in seconds.
    fn output_duration(output: &MediaOutput, rt: &Runtime, tag: &str) -> f64 {
        let dir = TempDir::new("concat-out").unwrap();
        let name = format!("out-{tag}.mp4");
        let bytes = &output.primary.as_ref().unwrap().bytes;
        dir.write(&name, bytes).unwrap();
        let report = probe_file(&dir.file(&name), rt, &limits()).unwrap();
        report
            .format
            .as_ref()
            .and_then(|f| f.duration_secs())
            .expect("output has no duration")
    }

    #[test]
    fn concat_copy_joins_two_clips() {
        let rt = rt();
        let dir = TempDir::new("concat-test").unwrap();
        let a = make_clip(&dir, "a.mp4", 440, &rt);
        let b = make_clip(&dir, "b.mp4", 550, &rt);

        let out =
            concat(&[a, b], &ConcatOptions::default(), &limits(), &rt).expect("copy concat failed");

        let primary = out.primary.as_ref().unwrap();
        assert_eq!(primary.name, "output.mp4");
        let dur = output_duration(&out, &rt, "copy");
        assert!(
            (dur - 2.0).abs() < 0.6,
            "expected ~2s concatenated duration, got {dur}"
        );
    }

    #[test]
    fn concat_reencode_normalizes_and_joins() {
        let rt = rt();
        let dir = TempDir::new("concat-test").unwrap();
        let a = make_clip(&dir, "a.mp4", 440, &rt);
        let b = make_clip(&dir, "b.mp4", 550, &rt);

        let options = ConcatOptions {
            reencode: true,
            normalize: true,
            ..ConcatOptions::default()
        };
        let out = concat(&[a, b], &options, &limits(), &rt).expect("re-encode concat failed");

        let dur = output_duration(&out, &rt, "reencode");
        assert!(
            (dur - 2.0).abs() < 0.6,
            "expected ~2s concatenated duration, got {dur}"
        );

        // Spot-check the normalized geometry/fps on the re-encoded file.
        let dir2 = TempDir::new("concat-check").unwrap();
        let name = "check.mp4";
        dir2.write(name, &out.primary.as_ref().unwrap().bytes)
            .unwrap();
        let report = probe_file(&dir2.file(name), &rt, &limits()).unwrap();
        let v = report
            .streams
            .iter()
            .find(|s| s.kind() == Some("video"))
            .expect("no video stream");
        assert_eq!((v.width, v.height), (Some(320), Some(240)));
        let fps = v.frame_rate().expect("no frame rate");
        assert!((fps - 30.0).abs() < 0.5, "unexpected fps {fps}");
    }

    #[test]
    fn concat_validates_inputs() {
        let rt = rt();
        let limits = limits();

        // Fewer than two inputs is rejected.
        let one = MediaInput {
            name: "a.mp4".into(),
            data: vec![0u8; 16],
        };
        let err = concat(&[one], &ConcatOptions::default(), &limits, &rt)
            .expect_err("single input must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)));

        // acrossfade is documented-but-unimplemented: must be an
        // honest Unsupported error, never a silent ignore.
        let dir = TempDir::new("concat-test").unwrap();
        let a = make_clip(&dir, "a.mp4", 440, &rt);
        let b = make_clip(&dir, "b.mp4", 550, &rt);
        let opts = ConcatOptions {
            acrossfade: Some(0.5),
            ..ConcatOptions::default()
        };
        let err = concat(&[a, b], &opts, &limits, &rt).expect_err("acrossfade must fail");
        assert!(matches!(err, MediaError::Unsupported(_)));
    }
}
