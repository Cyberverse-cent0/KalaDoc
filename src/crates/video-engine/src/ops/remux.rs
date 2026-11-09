//! Container remuxing: change the container format without
//! re-encoding streams (`ffmpeg -c copy`).
//!
//! Unlike [`crate::crates::video_engine::ops::transcode`], remux never
//! re-encodes: every stream is copied bit-for-bit (`-map 0 -c copy`),
//! so it is fast and lossless but constrained by what each target
//! muxer accepts. Illegal codec/container combinations (e.g. H.264 in
//! WAV) surface as [`VideoError::InvalidInput`] built from ffmpeg's own
//! muxer error; unexpected tool failures stay
//! [`VideoError::ExternalTool`].

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, checked_ext, ext_for_format,
    failure_detail, find_tool, read_output, run_tool, tool_failure, write_input,
};

/// Container targets `remux` accepts. Covers the common video
/// containers plus audio-only ones (`-c copy` muxes audio streams
/// fine; video streams simply get rejected by the muxer when illegal).
pub(crate) const REMUX_TARGETS: &[&str] = &[
    "mp4", "mov", "mkv", "webm", "avi", "ts", "mpegts", "flv", "ogg", "mp3", "m4a", "wav", "flac",
    "opus", "adts", "aac",
];

/// Explicit ffmpeg muxer name for `ext`. ffmpeg's extension guessing is
/// ambiguous for several of these (`.m4a` could pick `mp4`, `.aac` has
/// no unique muxer, `.ts` must be `mpegts`), so we always pass `-f`.
fn muxer_for(ext: &str) -> &str {
    match ext {
        "mkv" => "matroska",
        "m4a" => "ipod",
        "aac" => "adts",
        other => other,
    }
}

/// Whether a non-zero ffmpeg exit looks like an honest "this codec
/// cannot go in that container" rejection rather than a tool fault.
fn is_container_rejection(detail: &str) -> bool {
    let lower = detail.to_lowercase();
    lower.contains("could not find tag")
        || lower.contains("muxer")
        || lower.contains("codec")
        || lower.contains("could not write header")
        || lower.contains("invalid argument")
}

/// Remux `input` into the container named by `target_ext`, copying all
/// streams without re-encoding. Fails with `InvalidInput` when the
/// target container cannot accept the source codecs — ffmpeg's own
/// muxer error is surfaced honestly in that case.
pub(crate) fn remux(input: &[u8], target_ext: &str, limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    let ext = checked_ext(target_ext, REMUX_TARGETS, "remux")?;
    let program = find_tool(FFMPEG)?;

    // Probe once: limits, media validation, and the probed
    // `format_name` used to name the scratch input file so ffmpeg's
    // demuxer probing starts from a sensible extension.
    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;

    let dir = TempDir::new("remux")?;
    let input_ext = info
        .format_name
        .as_deref()
        .and_then(ext_for_format)
        .unwrap_or("bin");
    let input_path = write_input(&dir, &format!("input.{input_ext}"), input)?;
    let out_path = dir.file(&format!("out.{ext}"));

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        // Copy EVERY stream (default selection drops attached pictures,
        // data streams, and non-default audio) bit-for-bit.
        "-map".to_string(),
        "0".to_string(),
        "-c".to_string(),
        "copy".to_string(),
        // Explicit muxer: extension alone is ambiguous for m4a/aac/ts.
        "-f".to_string(),
        muxer_for(&ext).to_string(),
    ];
    // Faststart moves the moov atom to the front — only meaningful (and
    // only accepted) by the ISO-BMFF family muxers.
    if ext == "mp4" || ext == "mov" {
        args.extend(["-movflags".to_string(), "+faststart".to_string()]);
    }
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        let detail = failure_detail(&output, "ffmpeg exited unsuccessfully");
        if is_container_rejection(&detail) {
            return Err(VideoError::InvalidInput(format!(
                "cannot remux into '{ext}': {detail}"
            )));
        }
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, find_tool};
    use std::fs;

    fn tools_available() -> bool {
        find_tool(FFMPEG).is_ok() && find_tool(FFPROBE).is_ok()
    }

    /// Build a tiny 1s H.264+AAC mp4 via lavfi. Returns `None` when
    /// ffmpeg or an H.264 encoder is unavailable.
    fn make_fixture() -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let enc = crate::crates::video_engine::ops::h264_encoder()?;
        let dir = TempDir::new("remux-test").ok()?;
        let out = dir.file("fixture.mp4");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc=duration=1:size=160x120:rate=10".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "sine=frequency=440:duration=1".to_string(),
            "-c:v".to_string(),
            enc.to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-c:a".to_string(),
            "aac".to_string(),
            "-shortest".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    #[test]
    fn remux_mp4_to_mkv_produces_matroska() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture() else {
            return;
        };
        let out = remux(&fixture, "mkv", &EngineLimits::default()).unwrap();
        let info = probe(&out).unwrap();
        assert!(
            info.format_name
                .as_deref()
                .map(|n| n.to_ascii_lowercase().contains("matroska"))
                .unwrap_or(false),
            "expected matroska container, got {:?}",
            info.format_name
        );
    }

    #[test]
    fn remux_mp4_to_ts_works() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture() else {
            return;
        };
        let out = remux(&fixture, "ts", &EngineLimits::default()).unwrap();
        let info = probe(&out).unwrap();
        assert!(
            info.format_name
                .as_deref()
                .map(|n| n.to_ascii_lowercase().contains("mpegts"))
                .unwrap_or(false),
            "expected mpegts container, got {:?}",
            info.format_name
        );
    }

    #[test]
    fn remux_preserves_duration() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture() else {
            return;
        };
        let src = probe(&fixture).unwrap();
        let out = remux(&fixture, "mkv", &EngineLimits::default()).unwrap();
        let dst = probe(&out).unwrap();
        let (Some(a), Some(b)) = (src.duration_secs, dst.duration_secs) else {
            return;
        };
        // Stream copy can shift container timestamps slightly (TS
        // especially); allow half a second of slack.
        assert!((a - b).abs() < 0.5, "duration changed: {a:.3}s -> {b:.3}s");
    }

    #[test]
    fn remux_rejects_unsupported_target() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture() else {
            return;
        };
        assert!(matches!(
            remux(&fixture, "exe", &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        assert!(matches!(
            remux(&fixture, ".MKV", &EngineLimits::default()),
            Ok(_)
        ));
    }

    #[test]
    fn remux_rejects_garbage_input() {
        if !tools_available() {
            return;
        }
        let garbage = b"this is definitely not a media file".to_vec();
        assert!(matches!(
            remux(&garbage, "mkv", &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }
}
