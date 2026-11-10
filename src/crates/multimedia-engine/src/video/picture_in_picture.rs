//! Picture-in-picture: overlay a second video onto the main video.
//!
//! [`picture_in_picture`] composites `pip` (the inset) onto `main`
//! with ffmpeg's `overlay` filter:
//!
//! ```text
//! [1:v]trim=end=<main_dur>,setpts=…,scale=iw*<scale>:ih*<scale>,format=rgba[pip];
//! [0:v][pip]overlay=<x>:<y>[vout]
//! ```
//!
//! The inset is scaled by `scale` (a fraction of its own
//! dimensions, default `0.25`) and placed in a corner `margin`
//! pixels from the edge. When the probed main duration is known the
//! inset is `trim`med to it so a longer PiP clip cannot extend the
//! output; a shorter PiP freezes on its last frame (overlay's
//! default `eof_action=repeat`).
//!
//! Only the main input's audio is kept. Both inputs must contain a
//! video stream.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::ProbeReport;
use crate::crates::multimedia_engine::runtime::discovery::{Runtime, RuntimeCapabilities};
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Corner the picture-in-picture inset occupies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PipPosition {
    #[default]
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
}

/// Options for [`picture_in_picture`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PictureInPictureOptions {
    /// Inset placement. Default `TopRight`.
    #[serde(default)]
    pub position: PipPosition,
    /// Inset scale as a fraction of its own dimensions
    /// (`0.25` = quarter size). Must be in `(0.0, 1.0]`.
    #[serde(default = "default_scale")]
    pub scale: f64,
    /// Margin in pixels from the frame edges. Default `10`.
    #[serde(default = "default_margin")]
    pub margin: u32,
    /// Output container extension without a dot. `None` keeps the
    /// main input's sniffed container, falling back to `"mp4"`.
    pub output_format: Option<String>,
}

fn default_scale() -> f64 {
    0.25
}
fn default_margin() -> u32 {
    10
}

impl Default for PictureInPictureOptions {
    fn default() -> Self {
        Self {
            position: PipPosition::TopRight,
            scale: default_scale(),
            margin: default_margin(),
            output_format: None,
        }
    }
}

const VIDEO_ENCODERS: &[&str] = &["libx264", "libopenh264", "h264", "mpeg4"];
const WEBM_ENCODERS: &[&str] = &["libvpx-vp9", "libvpx"];

fn pick_encoder(caps: &RuntimeCapabilities, candidates: &[&'static str]) -> Option<&'static str> {
    candidates.iter().copied().find(|e| caps.has_encoder(e))
}

fn sniff_ext(input: &[u8]) -> &'static str {
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
    "mp4"
}

fn check_input(input: &[u8], limits: &MediaLimits, which: &str) -> MediaResult<()> {
    if input.is_empty() {
        return Err(MediaError::InvalidInput(format!("{which} input is empty")));
    }
    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "{which} input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }
    Ok(())
}

fn probe_file(path: &Path, rt: &Runtime, limits: &MediaLimits) -> MediaResult<ProbeReport> {
    let args = process::args(&[
        "-v",
        "error",
        "-show_streams",
        "-show_format",
        "-of",
        "json",
        path.to_str().unwrap_or("input"),
    ]);
    let out = process::run_checked("ffprobe", &rt.ffprobe, &args, None, limits)?;
    serde_json::from_slice::<ProbeReport>(&out.stdout)
        .map_err(|e| MediaError::InvalidInput(format!("ffprobe returned unparseable JSON: {e}")))
}

fn has_video(r: &ProbeReport) -> bool {
    r.streams.iter().any(|s| s.kind() == Some("video"))
}

fn has_audio(r: &ProbeReport) -> bool {
    r.streams.iter().any(|s| s.kind() == Some("audio"))
}

/// Main-stream duration in seconds (format duration, else first
/// video stream duration). `None` when unknown.
fn main_duration(r: &ProbeReport) -> Option<f64> {
    r.format
        .as_ref()
        .and_then(|f| f.duration_secs())
        .or_else(|| {
            r.streams
                .iter()
                .find(|s| s.kind() == Some("video"))
                .and_then(|s| s.duration_secs())
        })
        .filter(|d| d.is_finite() && *d > 0.0)
}

fn resolve_out_ext(main: &[u8], options: &PictureInPictureOptions) -> String {
    options
        .output_format
        .as_deref()
        .map(|s| {
            s.trim()
                .trim_start_matches('.')
                .to_ascii_lowercase()
                .replace(|c: char| !c.is_ascii_alphanumeric(), "")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| sniff_ext(main).to_string())
}

fn video_encoder(ext: &str, caps: &RuntimeCapabilities) -> Option<&'static str> {
    let candidates: &[&str] = if ext == "webm" {
        WEBM_ENCODERS
    } else {
        VIDEO_ENCODERS
    };
    pick_encoder(caps, candidates)
}

fn audio_encoder(ext: &str, caps: &RuntimeCapabilities) -> MediaResult<&'static str> {
    match ext {
        "webm" => {
            if caps.has_encoder("libopus") {
                Ok("libopus")
            } else if caps.has_encoder("libvorbis") {
                Ok("libvorbis")
            } else {
                Err(MediaError::Unsupported(
                    "no WebM audio encoder (libopus/libvorbis) in this FFmpeg build".to_string(),
                ))
            }
        }
        "mp3" => {
            if caps.has_encoder("libmp3lame") {
                Ok("libmp3lame")
            } else {
                Err(MediaError::Unsupported(
                    "no mp3 audio encoder (libmp3lame) in this FFmpeg build".to_string(),
                ))
            }
        }
        "wav" => Ok("pcm_s16le"),
        "flac" => Ok("flac"),
        "ogg" | "oga" => {
            if caps.has_encoder("libvorbis") {
                Ok("libvorbis")
            } else {
                Err(MediaError::Unsupported(
                    "no ogg audio encoder (libvorbis) in this FFmpeg build".to_string(),
                ))
            }
        }
        "opus" => {
            if caps.has_encoder("libopus") {
                Ok("libopus")
            } else {
                Err(MediaError::Unsupported(
                    "no opus audio encoder in this FFmpeg build".to_string(),
                ))
            }
        }
        _ => {
            if caps.has_encoder("aac") {
                Ok("aac")
            } else {
                Err(MediaError::Unsupported(
                    "no AAC audio encoder in this FFmpeg build".to_string(),
                ))
            }
        }
    }
}

fn content_type(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "mp4" | "m4v" | "mov" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "aac" | "adts" => "audio/aac",
        "ogg" | "oga" | "opus" => "audio/ogg",
        _ => return None,
    })
}

/// `overlay=x:y` expression for a corner position + margin.
/// Pure for testing.
fn overlay_xy(position: PipPosition, margin: u32) -> (String, String) {
    match position {
        PipPosition::TopLeft => (margin.to_string(), margin.to_string()),
        PipPosition::TopRight => (format!("W-w-{margin}"), margin.to_string()),
        PipPosition::BottomLeft => (margin.to_string(), format!("H-h-{margin}")),
        PipPosition::BottomRight => (format!("W-w-{margin}"), format!("H-h-{margin}")),
    }
}

/// Build the `-filter_complex` graph. Pure for testing.
/// `main_dur` trims the inset so a longer PiP cannot extend output.
fn filter_graph(options: &PictureInPictureOptions, main_dur: Option<f64>) -> String {
    let scale = options.scale;
    let (x, y) = overlay_xy(options.position, options.margin);
    let mut pip_chain = String::new();
    if let Some(d) = main_dur {
        pip_chain.push_str(&format!("trim=end={d:.3},setpts=PTS-STARTPTS,"));
    }
    pip_chain.push_str(&format!(
        "scale=w='iw*{scale}':h='ih*{scale}':eval=frame,format=rgba"
    ));
    format!("[1:v]{pip_chain}[pip];[0:v][pip]overlay={x}:{y}[vout]")
}

/// Composite `pip` video into the corner of `main` video.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty inputs, `scale` outside
///   `(0.0, 1.0]`, or either input lacks a video stream.
/// - [`MediaError::LimitExceeded`] — input/output size limits.
/// - [`MediaError::Unsupported`] — `overlay`/`scale`/`trim`/`setpts`/
///   `format` filter or encoder missing.
/// - [`MediaError::ToolFailed`] / [`MediaError::Timeout`].
pub fn picture_in_picture(
    main: &[u8],
    pip: &[u8],
    options: &PictureInPictureOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    check_input(main, limits, "main")?;
    check_input(pip, limits, "pip")?;
    if !options.scale.is_finite() || options.scale <= 0.0 || options.scale > 1.0 {
        return Err(MediaError::InvalidInput(format!(
            "scale must be in (0.0, 1.0], got {}",
            options.scale
        )));
    }

    let caps = rt.capabilities()?;
    for f in ["overlay", "scale", "format"] {
        if !caps.has_filter(f) {
            return Err(MediaError::Unsupported(format!(
                "ffmpeg build lacks the '{f}' filter required for picture-in-picture"
            )));
        }
    }

    let dir = TempDir::new("pip")?;
    let main_ext = sniff_ext(main);
    let pip_ext = sniff_ext(pip);
    let main_name = format!("main.{main_ext}");
    let pip_name = format!("pip.{pip_ext}");
    let main_path: PathBuf = dir.write(&main_name, main)?;
    let pip_path = dir.file(&pip_name);
    dir.write(&pip_name, pip)?;

    let main_report = probe_file(&main_path, rt, limits)
        .map_err(|e| MediaError::InvalidInput(format!("main input is not probeable: {e}")))?;
    let pip_report = probe_file(&pip_path, rt, limits)
        .map_err(|e| MediaError::InvalidInput(format!("pip input is not probeable: {e}")))?;
    if !has_video(&main_report) {
        return Err(MediaError::InvalidInput(
            "main input has no video stream".to_string(),
        ));
    }
    if !has_video(&pip_report) {
        return Err(MediaError::InvalidInput(
            "pip input has no video stream".to_string(),
        ));
    }
    let dur = main_duration(&main_report);

    // trim/setpts are only needed when a duration bound exists.
    if dur.is_some() {
        for f in ["trim", "setpts"] {
            if !caps.has_filter(f) {
                return Err(MediaError::Unsupported(format!(
                    "ffmpeg build lacks the '{f}' filter required to bound the PiP inset"
                )));
            }
        }
    }

    let out_ext = resolve_out_ext(main, options);
    let video_enc = match video_encoder(&out_ext, caps) {
        Some(e) => e,
        None => {
            return Err(MediaError::Unsupported(format!(
                "no usable video encoder for .{out_ext} output in this FFmpeg build"
            )));
        }
    };
    let audio_enc = if has_audio(&main_report) {
        Some(audio_encoder(&out_ext, caps)?)
    } else {
        None
    };

    let graph = filter_graph(options, dur);
    let out_name = format!("output.{out_ext}");

    let mut argv: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
        OsString::from("-i"),
        main_path.into_os_string(),
        OsString::from("-i"),
        pip_path.into_os_string(),
        OsString::from("-filter_complex"),
        OsString::from(graph),
        OsString::from("-map"),
        OsString::from("[vout]"),
    ];
    if audio_enc.is_some() {
        argv.extend([OsString::from("-map"), OsString::from("0:a?")]);
    }
    argv.extend([
        OsString::from("-c:v"),
        OsString::from(video_enc),
        OsString::from("-pix_fmt"),
        OsString::from("yuv420p"),
    ]);
    if let Some(enc) = audio_enc {
        argv.extend([OsString::from("-c:a"), OsString::from(enc)]);
        if matches!(enc, "aac" | "libmp3lame" | "libopus" | "libvorbis") {
            argv.extend([OsString::from("-b:a"), OsString::from("128k")]);
        }
    }
    argv.push(dir.file(&out_name).into_os_string());

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;
    let bytes = dir.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: format!("ffmpeg produced an empty {out_name}"),
        });
    }

    let mut result = MediaOutput::single(out_name, bytes);
    if let Some(primary) = result.primary.as_mut() {
        primary.content_type = content_type(&out_ext).map(str::to_string);
    }
    if !out.stderr.is_empty() {
        result.warnings.push(out.stderr);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
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

    #[test]
    fn overlay_xy_covers_all_corners() {
        assert_eq!(
            overlay_xy(PipPosition::TopLeft, 10),
            ("10".to_string(), "10".to_string())
        );
        assert_eq!(
            overlay_xy(PipPosition::TopRight, 10),
            ("W-w-10".to_string(), "10".to_string())
        );
        assert_eq!(
            overlay_xy(PipPosition::BottomLeft, 5),
            ("5".to_string(), "H-h-5".to_string())
        );
        assert_eq!(
            overlay_xy(PipPosition::BottomRight, 20),
            ("W-w-20".to_string(), "H-h-20".to_string())
        );
    }

    #[test]
    fn filter_graph_scales_inset_and_overlays() {
        let opts = PictureInPictureOptions::default();
        let g = filter_graph(&opts, Some(2.0));
        assert!(g.contains("trim=end=2.000"), "got {g}");
        assert!(g.contains("scale=w='iw*0.25':h='ih*0.25'"), "got {g}");
        assert!(g.contains("overlay=W-w-10:10[vout]"), "got {g}");

        let g = filter_graph(&opts, None);
        assert!(!g.contains("trim="), "got {g}");
    }

    fn make_clip(
        dir: &TempDir,
        name: &str,
        color: &str,
        dur: &str,
        rt: &Runtime,
    ) -> Option<Vec<u8>> {
        let path: PathBuf = dir.file(name);
        let mut argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("color=c={color}:size=160x120:rate=15:duration={dur}"),
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:duration={dur}"),
            "-c:v",
            "mpeg4",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
        ]);
        argv.push(path.to_string_lossy().into_owned().into());
        let res = process::run("ffmpeg", &rt.ffmpeg, &argv, None, &limits()).ok()?;
        if !res.success {
            return None;
        }
        std::fs::read(&path).ok()
    }

    fn output_duration(bytes: &[u8]) -> Option<f64> {
        let dir = TempDir::new("pip-dur").ok()?;
        let p = dir.file("out.mp4");
        std::fs::write(&p, bytes).ok()?;
        let out = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(&p)
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }

    #[test]
    fn picture_in_picture_overlays_inset() {
        let rt = rt();
        let caps = match rt.capabilities() {
            Ok(c) => c,
            Err(_) => {
                eprintln!("skipping: capabilities unavailable");
                return;
            }
        };
        if !(caps.has_filter("overlay")
            && caps.has_filter("scale")
            && caps.has_filter("format")
            && caps.has_filter("trim")
            && caps.has_filter("setpts")
            && (caps.has_encoder("libx264")
                || caps.has_encoder("libopenh264")
                || caps.has_encoder("h264")
                || caps.has_encoder("mpeg4")))
        {
            eprintln!("skipping: required filters/encoders unavailable");
            return;
        }
        let dir = TempDir::new("pip-test").unwrap();
        let (Some(main), Some(pip)) = (
            make_clip(&dir, "main.mp4", "red", "2", &rt),
            make_clip(&dir, "pip.mp4", "blue", "1", &rt),
        ) else {
            eprintln!("skipping: lavfi clip generation failed");
            return;
        };
        let opts = PictureInPictureOptions {
            position: PipPosition::BottomLeft,
            scale: 0.5,
            margin: 8,
            output_format: None,
        };
        let out = picture_in_picture(&main, &pip, &opts, &limits(), &rt)
            .expect("picture_in_picture failed");
        let dur = output_duration(&out.primary.unwrap().bytes).expect("probe duration");
        // Main is 2s; the 1s inset freezes on its last frame — output
        // should follow the main duration.
        assert!((dur - 2.0).abs() < 0.5, "expected ~2s output, got {dur}s");
    }

    #[test]
    fn pip_rejects_bad_scale_and_empty() {
        let rt = rt();
        let err = picture_in_picture(
            &[],
            &[0u8; 8],
            &PictureInPictureOptions::default(),
            &limits(),
            &rt,
        )
        .expect_err("empty main must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)));

        for bad in [0.0, -0.5, 1.5, f64::NAN] {
            let opts = PictureInPictureOptions {
                scale: bad,
                ..Default::default()
            };
            let err = picture_in_picture(&[0u8; 8], &[0u8; 8], &opts, &limits(), &rt)
                .expect_err("bad scale must fail");
            assert!(
                matches!(err, MediaError::InvalidInput(_)),
                "scale {bad} should be InvalidInput, got {err:?}"
            );
        }
    }
}
