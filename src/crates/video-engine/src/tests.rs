//! Integration tests for [`VideoEngine`].
//!
//! Test media is generated in-test by ffmpeg itself (lavfi
//! `testsrc` + `sine`) into a unique tempdir — no binary fixtures are
//! committed. Every test skips cleanly when ffmpeg/ffprobe are absent
//! so the suite stays green on machines without the tools.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::crates::core::engine::{
    EngineHealthStatus, EngineLimits, EngineOperation, ProcessingEngine,
};
use crate::crates::video_engine::ops::tool_available;
use crate::crates::video_engine::{VideoEngine, VideoError};

/// 1s 320x240@10fps clip with a 440 Hz sine track — small enough that
/// every test runs in well under a second of encode time.
const CLIP_ARGS: &[&str] = &[
    "-y",
    "-v",
    "error",
    "-f",
    "lavfi",
    "-i",
    "testsrc=duration=1:size=320x240:rate=10",
    "-f",
    "lavfi",
    "-i",
    "sine=frequency=440:duration=1",
    "-pix_fmt",
    "yuv420p",
    "-shortest",
];

fn ffmpeg_available() -> bool {
    tool_available("ffmpeg") && tool_available("ffprobe")
}

/// Unique scratch dir per test; removed on drop.
struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "kaladoc-video-test-{tag}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Generate a tiny valid MP4 clip with ffmpeg; returns its bytes.
fn make_clip(dir: &TestDir, name: &str) -> Vec<u8> {
    let out = dir.0.join(name);
    let status = Command::new("ffmpeg")
        .args(CLIP_ARGS)
        .arg(&out)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .status()
        .expect("failed to spawn ffmpeg");
    assert!(status.success(), "ffmpeg fixture generation failed");
    fs::read(&out).unwrap()
}

fn unlimited() -> EngineLimits {
    EngineLimits::default()
}

#[test]
fn engine_metadata_and_capabilities() {
    let engine = VideoEngine::new();
    assert_eq!(engine.name(), "video");
    assert_eq!(engine.version(), "0.1.0");
    assert_eq!(
        engine.kind(),
        crate::crates::core::engine::EngineKind::Video
    );

    let caps = engine.capabilities();
    for op in [
        EngineOperation::Inspect,
        EngineOperation::Validate,
        EngineOperation::Thumbnail,
        EngineOperation::Trim,
        EngineOperation::Transcode,
        EngineOperation::Concat,
        EngineOperation::ExtractAudio,
    ] {
        assert!(caps.operations.contains(&op), "missing op {op:?}");
    }
    assert_eq!(caps.input_formats, caps.output_formats);
    for f in ["mp4", "mkv", "mov", "webm", "avi"] {
        assert!(caps.input_formats.contains(&f.to_string()));
    }
}

#[test]
fn health_reflects_tool_presence() {
    let engine = VideoEngine::new();
    let health = futures_lite_block_on(engine.health());
    if ffmpeg_available() {
        assert_eq!(health.status, EngineHealthStatus::Healthy);
    } else {
        // On a tool-less machine both binaries are absent → Unavailable.
        assert_eq!(health.status, EngineHealthStatus::Unavailable);
    }
}

/// Minimal block_on for the `EngineFuture` health probe — avoids
/// pulling a runtime into tests.
fn futures_lite_block_on<F: std::future::Future>(fut: F) -> F::Output {
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    struct Noop;
    impl Wake for Noop {
        fn wake(self: Arc<Self>) {}
    }

    let waker = Waker::from(Arc::new(Noop));
    let mut cx = Context::from_waker(&waker);
    let mut fut = Box::pin(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            // health() never yields; Pending would mean a bug.
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

#[test]
fn probe_reports_media_fields() {
    if !ffmpeg_available() {
        return;
    }
    let dir = TestDir::new("probe");
    let clip = make_clip(&dir, "clip.mp4");
    let engine = VideoEngine::new();

    let info = engine.probe(&clip).expect("probe failed");
    let duration = info.duration_secs.expect("duration missing");
    assert!((duration - 1.0).abs() < 0.2, "duration {duration}");
    assert_eq!(info.width, Some(320));
    assert_eq!(info.height, Some(240));
    assert!(info.fps.is_some_and(|f| (f - 10.0).abs() < 0.5));
    // The fixture's default encoder varies by ffmpeg build (mpeg4,
    // libx264, libopenh264, ...) — assert presence, not a specific codec.
    assert!(info.vcodec.is_some());
    assert!(info.has_audio);
    assert!(info.acodec.is_some());
    assert!(info.format_name.is_some());
    assert!(info.size_bytes > 0);
}

#[test]
fn probe_rejects_non_media() {
    if !tool_available("ffprobe") {
        return;
    }
    let engine = VideoEngine::new();
    let err = engine
        .probe(b"this is definitely not a media file")
        .unwrap_err();
    assert!(
        matches!(err, VideoError::InvalidInput(_)),
        "expected InvalidInput, got {err:?}"
    );
}

#[test]
fn thumbnail_produces_png() {
    if !ffmpeg_available() {
        return;
    }
    let dir = TestDir::new("thumb");
    let clip = make_clip(&dir, "clip.mp4");
    let engine = VideoEngine::new();

    let png = engine.thumbnail(&clip, Some(0.5), &unlimited()).unwrap();
    assert!(png.len() > 100);
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");

    // None → default seek point also works.
    let png2 = engine.thumbnail(&clip, None, &unlimited()).unwrap();
    assert_eq!(&png2[..8], b"\x89PNG\r\n\x1a\n");
}

#[test]
fn trim_produces_shorter_clip() {
    if !ffmpeg_available() {
        return;
    }
    let dir = TestDir::new("trim");
    let clip = make_clip(&dir, "clip.mp4");
    let engine = VideoEngine::new();

    // Re-encode path: frame-accurate, duration must shrink.
    let trimmed = engine
        .trim(&clip, 0.0, Some(0.5), true, &unlimited())
        .unwrap();
    let info = engine.probe(&trimmed).unwrap();
    let d = info.duration_secs.unwrap_or(f64::MAX);
    assert!(d < 0.9, "trimmed duration {d} not shorter");

    // Stream-copy path also yields a valid, shorter file.
    let copied = engine
        .trim(&clip, 0.0, Some(0.5), false, &unlimited())
        .unwrap();
    let info2 = engine.probe(&copied).unwrap();
    let d2 = info2.duration_secs.unwrap_or(f64::MAX);
    assert!(d2 < 1.0, "copied duration {d2} not shorter");
}

#[test]
fn trim_validates_range() {
    if !ffmpeg_available() {
        return;
    }
    let dir = TestDir::new("trimval");
    let clip = make_clip(&dir, "clip.mp4");
    let engine = VideoEngine::new();

    assert!(matches!(
        engine.trim(&clip, -1.0, None, false, &unlimited()),
        Err(VideoError::InvalidInput(_))
    ));
    assert!(matches!(
        engine.trim(&clip, 0.5, Some(0.5), false, &unlimited()),
        Err(VideoError::InvalidInput(_))
    ));
    assert!(matches!(
        engine.trim(&clip, 0.0, Some(0.4), false, &unlimited()),
        Ok(_)
    ));
}

#[test]
fn transcode_mp4_to_mkv() {
    if !ffmpeg_available() {
        return;
    }
    let dir = TestDir::new("transcode");
    let clip = make_clip(&dir, "clip.mp4");
    let engine = VideoEngine::new();

    let mkv = engine.transcode(&clip, "mkv", &unlimited()).unwrap();
    assert!(!mkv.is_empty());
    let info = engine.probe(&mkv).unwrap();
    let fmt = info.format_name.unwrap_or_default();
    assert!(fmt.contains("matroska"), "format_name {fmt}");

    // Unsupported targets are rejected before ffmpeg runs.
    assert!(matches!(
        engine.transcode(&clip, "xyz", &unlimited()),
        Err(VideoError::InvalidInput(_))
    ));
}

#[test]
fn extract_audio_produces_nonempty() {
    if !ffmpeg_available() {
        return;
    }
    let dir = TestDir::new("audio");
    let clip = make_clip(&dir, "clip.mp4");
    let engine = VideoEngine::new();

    for ext in ["mp3", "wav", "aac", "m4a", "ogg"] {
        let out = engine.extract_audio(&clip, ext, &unlimited()).unwrap();
        assert!(out.len() > 100, "{ext} output too small: {}", out.len());
    }
}

#[test]
fn concat_joins_two_clips() {
    if !ffmpeg_available() {
        return;
    }
    let dir = TestDir::new("concat");
    let clip = make_clip(&dir, "clip.mp4");
    let clip2 = make_clip(&dir, "clip2.mp4");
    let engine = VideoEngine::new();

    let joined = engine
        .concat(&[clip.clone(), clip2], "mp4", &unlimited())
        .unwrap();
    let info = engine.probe(&joined).unwrap();
    let d = info.duration_secs.unwrap_or(0.0);
    assert!(d > 1.5, "concat duration {d} too short");
    assert!(d < 2.5, "concat duration {d} too long");

    assert!(matches!(
        engine.concat(&[], "mp4", &unlimited()),
        Err(VideoError::InvalidInput(_))
    ));
}

#[test]
fn duration_limit_is_enforced() {
    if !ffmpeg_available() {
        return;
    }
    let dir = TestDir::new("limit");
    let clip = make_clip(&dir, "clip.mp4");
    let engine = VideoEngine::new();

    // max_duration_secs = 0 rejects any media with a reported duration.
    let limits = EngineLimits {
        max_duration_secs: Some(0),
        ..EngineLimits::default()
    };
    let err = engine.transcode(&clip, "mkv", &limits).unwrap_err();
    assert!(
        matches!(err, VideoError::LimitExceeded(_)),
        "expected LimitExceeded, got {err:?}"
    );

    // A generous limit passes.
    let limits_ok = EngineLimits {
        max_duration_secs: Some(60),
        ..EngineLimits::default()
    };
    assert!(engine.transcode(&clip, "mkv", &limits_ok).is_ok());
}

#[test]
fn empty_input_is_invalid_not_panic() {
    let engine = VideoEngine::new();
    if ffmpeg_available() {
        assert!(matches!(
            engine.probe(&[]),
            Err(VideoError::InvalidInput(_))
        ));
    }
    // Even without tools, empty input must not panic — it is rejected
    // before any PATH probe in probe(), and MissingTool elsewhere is
    // still a structured error.
    let _ = engine.thumbnail(&[], None, &unlimited());
}

#[test]
fn media_capabilities_reports_build() {
    let engine = VideoEngine::new();

    // The engine advertises a non-empty op set for EngineKind::Video.
    let caps = engine.capabilities();
    assert!(!caps.operations.is_empty());
    assert!(caps.operations.contains(&EngineOperation::Transcode));

    // media_capabilities() shells out to ffmpeg — skip when absent.
    if !tool_available("ffmpeg") {
        return;
    }
    let caps = engine
        .media_capabilities()
        .expect("media_capabilities failed");
    assert!(
        !caps.video_encoders.is_empty(),
        "ffmpeg build reported no video encoders"
    );
    assert!(
        caps.has_muxer("mp4") || caps.has_muxer("matroska"),
        "expected common muxers in capability snapshot"
    );

    // The tool resolves to a real executable path on PATH.
    let ffmpeg_path = crate::crates::video_engine::ops::find_tool("ffmpeg")
        .expect("ffmpeg on PATH but find_tool failed");
    assert!(ffmpeg_path.is_file());
}

#[test]
fn pipeline_probe_thumbnail_trim_concat_roundtrip() {
    if !ffmpeg_available() {
        return;
    }
    let dir = TestDir::new("pipeline");
    let clip = make_clip(&dir, "clip.mp4");
    let engine = VideoEngine::new();
    let limits = unlimited();

    // probe → thumbnail → trim → concat, threading Vec<u8> through.
    let info = engine.probe(&clip).unwrap();
    assert!(info.has_video);
    assert!(info.duration_secs.unwrap_or(0.0) > 0.0);

    let png = engine.thumbnail(&clip, Some(0.2), &limits).unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");

    let trimmed = engine.trim(&clip, 0.0, Some(0.5), true, &limits).unwrap();
    let trimmed_info = engine.probe(&trimmed).unwrap();
    assert!(
        trimmed_info.duration_secs.unwrap_or(f64::MAX) < 0.9,
        "trimmed clip not shorter"
    );

    // Concat two copies of the trimmed output back into mp4.
    let joined = engine
        .concat(&[trimmed.clone(), trimmed], "mp4", &limits)
        .unwrap();
    let joined_info = engine.probe(&joined).unwrap();
    assert!(joined_info.has_video);
    let d = joined_info.duration_secs.unwrap_or(0.0);
    // Two ~0.5s segments; allow container timestamp slack.
    assert!(d > 0.6 && d < 1.8, "concat duration {d} unexpected");
}

#[test]
fn input_byte_limit_is_enforced() {
    if !ffmpeg_available() {
        return;
    }
    let dir = TestDir::new("bytelimit");
    let clip = make_clip(&dir, "clip.mp4");
    let engine = VideoEngine::new();

    // A byte cap far below the fixture size must stop transcode before
    // ffmpeg runs (limit is checked on the probed input).
    let limits = EngineLimits {
        max_input_bytes: Some(64),
        ..EngineLimits::default()
    };
    let err = engine.transcode(&clip, "mkv", &limits).unwrap_err();
    assert!(
        matches!(err, VideoError::LimitExceeded(_)),
        "expected LimitExceeded, got {err:?}"
    );

    // The same cap passes once raised above the fixture size.
    let ok = EngineLimits {
        max_input_bytes: Some(clip.len() as u64),
        ..EngineLimits::default()
    };
    assert!(engine.transcode(&clip, "mkv", &ok).is_ok());
}

#[test]
fn malformed_and_empty_inputs_are_invalid() {
    let engine = VideoEngine::new();
    let limits = unlimited();
    let garbage = b"\x00\x01\x02definitely not media".repeat(32);

    // Empty input is rejected by check_input before any tool lookup —
    // safe to assert without ffmpeg installed.
    for result in [
        engine.thumbnail(&[], None, &limits).map(|_| ()),
        engine.trim(&[], 0.0, None, false, &limits).map(|_| ()),
        engine.transcode(&[], "mkv", &limits).map(|_| ()),
    ] {
        assert!(
            matches!(result, Err(VideoError::InvalidInput(_))),
            "expected InvalidInput for empty input, got {result:?}"
        );
    }

    // Unknown target extensions are rejected before probing or any
    // ffmpeg invocation — no tools required.
    assert!(matches!(
        engine.transcode(&garbage, "xyz", &limits),
        Err(VideoError::InvalidInput(_))
    ));
    assert!(matches!(
        engine.transcode(&garbage, ".xyz ", &limits),
        Err(VideoError::InvalidInput(_))
    ));

    // Non-media bytes must surface InvalidInput (not a panic or a
    // bogus success) once the tools exist to inspect them.
    if ffmpeg_available() {
        for result in [
            engine.probe(&garbage).map(|_| ()),
            engine.thumbnail(&garbage, None, &limits).map(|_| ()),
            engine.trim(&garbage, 0.0, None, false, &limits).map(|_| ()),
            engine.transcode(&garbage, "mkv", &limits).map(|_| ()),
            engine
                .concat(&[garbage.clone()], "mp4", &limits)
                .map(|_| ()),
        ] {
            assert!(
                matches!(result, Err(VideoError::InvalidInput(_))),
                "expected InvalidInput for garbage input, got {result:?}"
            );
        }
    }
}

#[test]
fn format_smoke_transcode_mkv_and_remux_ts() {
    if !ffmpeg_available() {
        return;
    }
    let dir = TestDir::new("formats");
    let clip = make_clip(&dir, "clip.mp4");
    let engine = VideoEngine::new();
    let limits = unlimited();

    // mp4 → mkv transcode (stream-copy path for this target).
    let mkv = engine.transcode(&clip, "mkv", &limits).unwrap();
    let info = engine.probe(&mkv).unwrap();
    let fmt = info.format_name.unwrap_or_default();
    assert!(fmt.contains("matroska"), "format_name {fmt}");

    // mp4 → MPEG-TS remux (stream copy; TS accepts H.264+AAC).
    let ts = engine.remux(&clip, "ts", &limits).unwrap();
    let ts_info = engine.probe(&ts).unwrap();
    let ts_fmt = ts_info.format_name.unwrap_or_default();
    assert!(ts_fmt.contains("mpegts"), "expected mpegts, got {ts_fmt}");

    // mkv → mp4 remux keeps the container family honest (moov faststart).
    let back = engine.remux(&mkv, "mp4", &limits).unwrap();
    let back_info = engine.probe(&back).unwrap();
    assert!(back_info.has_video);
}
