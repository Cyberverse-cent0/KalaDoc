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
