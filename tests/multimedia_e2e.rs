//! End-to-end tests for the multimedia engine (`kaladoc::crates::
//! multimedia_engine`) against the real `ffmpeg`/`ffprobe` installation
//! on this machine.
//!
//! The suite is deliberately capability-driven: every op that needs a
//! specific encoder/muxer/filter first checks the runtime capability
//! registry (`Runtime::capabilities`) and skips when the build lacks
//! it. This FFmpeg build may not include libx264, so x264-dependent
//! paths assert an honest `MediaError::Unsupported` instead of a
//! silent fallback.
//!
//! Test media is generated with `ffmpeg` itself (lavfi `testsrc` +
//! `sine` sources) using only codecs this build is expected to have
//! (`mpeg4`/`aac`/`flac`/`mjpeg`); fixture generation also verifies
//! the capability registry before invoking ffmpeg.

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use kaladoc::crates::core::engine::ProcessingEngine;
use kaladoc::crates::multimedia_engine::MediaEngine;
use kaladoc::crates::multimedia_engine::audio::normalize::{
    NormalizeOptions, measure_loudness, normalize_loudness,
};
use kaladoc::crates::multimedia_engine::audio::trim::{AudioTrimOptions, trim_audio};
use kaladoc::crates::multimedia_engine::audio::volume::{VolumeOptions, adjust_volume};
use kaladoc::crates::multimedia_engine::conversion::transcode::{TranscodeOptions, transcode};
use kaladoc::crates::multimedia_engine::error::MediaError;
use kaladoc::crates::multimedia_engine::frames::frame_at::{FrameOptions, frame_at};
use kaladoc::crates::multimedia_engine::frames::thumbnail::{ThumbnailOptions, thumbnail};
use kaladoc::crates::multimedia_engine::metadata::cover_art::{add_cover_art, extract_cover_art};
use kaladoc::crates::multimedia_engine::metadata::strip::{StripOptions, strip_metadata};
use kaladoc::crates::multimedia_engine::metadata::tags::{
    TagEdit, TagOptions, edit_tags, read_tags,
};
use kaladoc::crates::multimedia_engine::model::{MediaInput, MediaLimits, MediaOutput};
use kaladoc::crates::multimedia_engine::probe::inspect::inspect;
use kaladoc::crates::multimedia_engine::probe::streams::probe_streams;
use kaladoc::crates::multimedia_engine::runtime::discovery::{Runtime, RuntimeCapabilities};
use kaladoc::crates::multimedia_engine::video::concat::{ConcatOptions, concat};
use kaladoc::crates::multimedia_engine::video::split::{SplitMode, SplitOptions, split};
use kaladoc::crates::multimedia_engine::video::trim::{TrimOptions, trim};

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

/// The discovered FFmpeg runtime, or `None` when ffmpeg/ffprobe are not
/// on `PATH` (in which case every test skips).
fn runtime() -> Option<&'static Runtime> {
    static RT: OnceLock<Option<Runtime>> = OnceLock::new();
    RT.get_or_init(|| Runtime::discover().ok()).as_ref()
}

/// Capability registry for the discovered runtime.
fn caps() -> Option<&'static RuntimeCapabilities> {
    runtime()?.capabilities().ok()
}

fn limits() -> MediaLimits {
    MediaLimits::default()
}

/// Run `ffmpeg` with the given args; `true` on exit status 0.
fn run_ffmpeg(ffmpeg: &Path, args: &[&str]) -> bool {
    Command::new(ffmpeg)
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Generate a tiny mp4 clip (2 s, 160x120 @ 10 fps, mpeg4 + aac) with
/// ffmpeg's lavfi sources. Returns `None` — causing tests to skip —
/// when the runtime or the needed encoders/muxer are absent or
/// generation fails.
fn test_clip() -> Option<&'static [u8]> {
    static CLIP: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    CLIP.get_or_init(|| {
        let rt = runtime()?;
        let caps = rt.capabilities().ok()?;
        // Honest gating: only generate when this build can actually
        // produce the fixture.
        if !caps.has_encoder("mpeg4")
            || !caps.has_encoder("aac")
            || !caps.has_muxer("mp4")
            || !caps.has_filter("testsrc")
            || !caps.has_filter("sine")
        {
            return None;
        }
        let dir = tempfile::tempdir().ok()?;
        let out = dir.path().join("clip.mp4");
        let out_str = out.to_str()?;
        // -g 5 puts a keyframe every 0.5 s so stream-copy trims/splits
        // land on real seek points.
        let ok = run_ffmpeg(
            &rt.ffmpeg,
            &[
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=2:size=160x120:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-c:v",
                "mpeg4",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-g",
                "5",
                "-shortest",
                out_str,
            ],
        );
        if !ok {
            return None;
        }
        std::fs::read(&out).ok()
    })
    .as_deref()
}

/// Generate a 1 s mono flac file for cover-art tests.
fn flac_audio() -> Option<Vec<u8>> {
    static AUDIO: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    AUDIO
        .get_or_init(|| {
            let rt = runtime()?;
            let caps = rt.capabilities().ok()?;
            if !caps.has_encoder("flac") || !caps.has_muxer("flac") || !caps.has_filter("sine") {
                return None;
            }
            let dir = tempfile::tempdir().ok()?;
            let out = dir.path().join("tone.flac");
            let out_str = out.to_str()?;
            let ok = run_ffmpeg(
                &rt.ffmpeg,
                &[
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=1:sample_rate=44100",
                    "-c:a",
                    "flac",
                    out_str,
                ],
            );
            if !ok {
                return None;
            }
            std::fs::read(&out).ok()
        })
        .clone()
}

/// Generate a tiny 64x64 jpeg for cover-art tests.
fn cover_image() -> Option<Vec<u8>> {
    static IMG: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    IMG.get_or_init(|| {
        let rt = runtime()?;
        let caps = rt.capabilities().ok()?;
        if !caps.has_encoder("mjpeg") || !caps.has_muxer("image2") || !caps.has_filter("color") {
            return None;
        }
        let dir = tempfile::tempdir().ok()?;
        let out = dir.path().join("cover.jpg");
        let out_str = out.to_str()?;
        let ok = run_ffmpeg(
            &rt.ffmpeg,
            &[
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=64x64",
                "-frames:v",
                "1",
                "-c:v",
                "mjpeg",
                out_str,
            ],
        );
        if !ok {
            return None;
        }
        std::fs::read(&out).ok()
    })
    .clone()
}

fn primary_bytes(out: &MediaOutput) -> &[u8] {
    out.primary
        .as_ref()
        .expect("op produced no primary output")
        .bytes
        .as_slice()
}

// ---------------------------------------------------------------------------
// Discovery / engine surface
// ---------------------------------------------------------------------------

#[test]
fn runtime_discover_reports_build_capabilities() {
    let Some(rt) = runtime() else {
        eprintln!("skip: ffmpeg/ffprobe not on PATH");
        return;
    };
    let caps = rt
        .capabilities()
        .expect("one-time capability probe should succeed");
    assert!(!caps.encoders.is_empty(), "no encoders discovered");
    assert!(!caps.muxers.is_empty(), "no muxers discovered");
    assert!(!caps.filters.is_empty(), "no filters discovered");
    assert!(!caps.demuxers.is_empty(), "no demuxers discovered");
    assert!(caps.ffmpeg_version.is_some(), "ffmpeg version missing");
    assert!(rt.ffmpeg.is_file(), "ffmpeg path does not exist");
    assert!(rt.ffprobe.is_file(), "ffprobe path does not exist");
}

#[test]
fn engine_advertises_operations_and_runtime() {
    let engine = MediaEngine::new();
    let advertised = engine.capabilities();
    assert!(!advertised.operations.is_empty());
    assert!(!advertised.input_formats.is_empty());
    assert!(!advertised.output_formats.is_empty());

    let rt = engine.runtime().expect("runtime discovery failed");
    let detail = engine
        .capabilities_detail()
        .expect("capability detail probe failed");
    assert!(!detail.encoders.is_empty());
    assert!(!detail.muxers.is_empty());
    assert!(!detail.filters.is_empty());
    // The engine's advertised formats must be backed by real muxers.
    assert!(detail.has_muxer("mp4"));
    let _ = rt;
}

// ---------------------------------------------------------------------------
// Probe / inspect
// ---------------------------------------------------------------------------

#[test]
fn inspect_reports_format_streams_and_duration() {
    let Some(clip) = test_clip() else {
        eprintln!("skip: cannot generate mpeg4/aac mp4 fixture");
        return;
    };
    let info = inspect(clip, &limits()).expect("inspect failed on generated clip");

    let format = info.format_name.unwrap_or_default();
    assert!(
        format
            .split(',')
            .any(|f| f.trim() == "mp4" || f.trim() == "mov"),
        "unexpected container format {format:?}"
    );
    assert_eq!(info.video_streams, 1, "expected one video stream");
    assert_eq!(info.audio_streams, 1, "expected one audio stream");
    let dur = info.duration_secs.expect("no container duration");
    assert!(
        (1.5..=2.5).contains(&dur),
        "unexpected duration {dur}s for a 2 s clip"
    );
    let video = info.primary_video.expect("no primary video summary");
    assert_eq!(video.codec.as_deref(), Some("mpeg4"));
    assert_eq!(video.width, Some(160));
    assert_eq!(video.height, Some(120));
    let audio = info.primary_audio.expect("no primary audio summary");
    assert_eq!(audio.codec.as_deref(), Some("aac"));
}

#[test]
fn probe_streams_lists_elementary_streams() {
    let Some(clip) = test_clip() else {
        eprintln!("skip: cannot generate fixture");
        return;
    };
    let report = probe_streams(clip, &limits()).expect("probe_streams failed");
    assert!(report.streams.len() >= 2, "expected video+audio streams");
    assert!(
        report.format_name.as_deref().unwrap_or("").contains("mp4")
            || report.format_name.as_deref().unwrap_or("").contains("mov"),
        "unexpected format {:?}",
        report.format_name
    );
    assert!(
        report
            .streams
            .iter()
            .any(|s| s.codec_type.as_deref() == Some("video")
                && s.codec_name.as_deref() == Some("mpeg4")),
        "no mpeg4 video stream in report"
    );
    assert!(
        report
            .streams
            .iter()
            .any(|s| s.codec_type.as_deref() == Some("audio")
                && s.codec_name.as_deref() == Some("aac")),
        "no aac audio stream in report"
    );
}

// ---------------------------------------------------------------------------
// Video ops
// ---------------------------------------------------------------------------

#[test]
fn trim_stream_copy_produces_shorter_clip() {
    let (Some(rt), Some(clip)) = (runtime(), test_clip()) else {
        eprintln!("skip: ffmpeg or fixture unavailable");
        return;
    };
    let opts = TrimOptions {
        start: 0.5,
        end: Some(1.5),
        accurate: false,
        output_format: Some("mp4".to_string()),
        audio_trim: true,
    };
    let out = trim(clip, &opts, &limits(), rt).expect("stream-copy trim failed");
    assert!(!primary_bytes(&out).is_empty());

    let info = inspect(primary_bytes(&out), &limits()).expect("cannot inspect trim output");
    let dur = info.duration_secs.unwrap_or(f64::MAX);
    // Keyframe-snapped copy cut: must be clearly shorter than 2 s.
    assert!(
        dur < 1.9,
        "trimmed clip duration {dur}s not shorter than source"
    );
}

#[test]
fn trim_accurate_respects_libx264_capability() {
    let (Some(rt), Some(clip), Some(caps)) = (runtime(), test_clip(), caps()) else {
        eprintln!("skip: ffmpeg or fixture unavailable");
        return;
    };
    // The accurate path for mp4 output hard-requires libx264 + aac.
    let opts = TrimOptions {
        start: 0.2,
        end: Some(1.0),
        accurate: true,
        output_format: Some("mp4".to_string()),
        audio_trim: true,
    };
    let res = trim(clip, &opts, &limits(), rt);
    if caps.has_encoder("libx264") && caps.has_encoder("aac") {
        let out = res.expect("accurate trim failed despite libx264+aac");
        assert!(!primary_bytes(&out).is_empty());
    } else {
        assert!(
            matches!(res, Err(MediaError::Unsupported(_))),
            "expected Unsupported without libx264, got {res:?}"
        );
    }
}

#[test]
fn concat_stream_copy_joins_two_clips() {
    let (Some(rt), Some(clip), Some(caps)) = (runtime(), test_clip(), caps()) else {
        eprintln!("skip: ffmpeg or fixture unavailable");
        return;
    };
    if !caps.has_demuxer("concat") || !caps.has_muxer("mp4") {
        eprintln!("skip: concat demuxer or mp4 muxer unavailable");
        return;
    }
    let inputs = vec![
        MediaInput {
            name: "a.mp4".to_string(),
            data: clip.to_vec(),
        },
        MediaInput {
            name: "b.mp4".to_string(),
            data: clip.to_vec(),
        },
    ];
    let opts = ConcatOptions {
        output_format: Some("mp4".to_string()),
        reencode: false,
        ..ConcatOptions::default()
    };
    let out = concat(&inputs, &opts, &limits(), rt).expect("concat failed");
    let bytes = primary_bytes(&out);
    assert!(!bytes.is_empty());

    let info = inspect(bytes, &limits()).expect("cannot inspect concat output");
    let dur = info.duration_secs.unwrap_or(0.0);
    assert!(dur > 3.0, "concat of two ~2 s clips reported only {dur}s");
}

#[test]
fn split_stream_copy_produces_segments() {
    let (Some(rt), Some(clip), Some(caps)) = (runtime(), test_clip(), caps()) else {
        eprintln!("skip: ffmpeg or fixture unavailable");
        return;
    };
    if !caps.has_muxer("mp4") {
        eprintln!("skip: mp4 muxer unavailable");
        return;
    }
    let opts = SplitOptions {
        mode: SplitMode::TimeRanges(vec![(0.0, 1.0), (1.0, 2.0)]),
        output_format: Some("mp4".to_string()),
        accurate: false,
        prefix: "seg".to_string(),
    };
    let out = split(clip, &opts, &limits(), rt).expect("split failed");
    let files: Vec<_> = out.files().collect();
    assert!(
        files.len() >= 2,
        "expected >= 2 segments, got {}",
        files.len()
    );
    for f in files {
        assert!(!f.bytes.is_empty(), "empty segment {}", f.name);
        assert!(f.name.ends_with(".mp4"), "bad segment name {}", f.name);
    }
}

#[test]
fn transcode_with_available_encoders() {
    let (Some(rt), Some(clip), Some(caps)) = (runtime(), test_clip(), caps()) else {
        eprintln!("skip: ffmpeg or fixture unavailable");
        return;
    };
    if !caps.has_encoder("mpeg4") || !caps.has_encoder("aac") || !caps.has_muxer("mp4") {
        eprintln!("skip: mpeg4/aac/mp4 unavailable in this build");
        return;
    }
    let opts = TranscodeOptions {
        output_format: "mp4".to_string(),
        video_codec: Some("mpeg4".to_string()),
        audio_codec: Some("aac".to_string()),
        video_bitrate: Some("300k".to_string()),
        audio_bitrate: Some("96k".to_string()),
        ..TranscodeOptions::default()
    };
    let out = transcode(clip, &opts, &limits(), rt).expect("transcode failed");
    let info = inspect(primary_bytes(&out), &limits()).expect("cannot inspect transcode output");
    assert_eq!(info.video_streams, 1);
    assert_eq!(info.audio_streams, 1);
    assert_eq!(
        info.primary_video.and_then(|v| v.codec),
        Some("mpeg4".to_string())
    );
}

// ---------------------------------------------------------------------------
// Frame / thumbnail extraction
// ---------------------------------------------------------------------------

#[test]
fn thumbnail_png_extracts_frame() {
    let (Some(rt), Some(clip), Some(caps)) = (runtime(), test_clip(), caps()) else {
        eprintln!("skip: ffmpeg or fixture unavailable");
        return;
    };
    if !caps.has_encoder("png") || !caps.has_muxer("image2") || !caps.has_filter("scale") {
        eprintln!("skip: png encoder/image2 muxer/scale filter unavailable");
        return;
    }
    let opts = ThumbnailOptions {
        at_secs: Some(1.0),
        width: Some(80),
        height: None,
        output_format: "png".to_string(),
        quality: None,
    };
    let out = thumbnail(clip, &opts, &limits(), rt).expect("thumbnail failed");
    let bytes = primary_bytes(&out);
    assert!(bytes.starts_with(b"\x89PNG"), "output is not a PNG");
    let name = &out.primary.as_ref().unwrap().name;
    assert!(name.ends_with(".png"), "unexpected output name {name}");
}

#[test]
fn frame_at_extracts_single_frame() {
    let (Some(rt), Some(clip), Some(caps)) = (runtime(), test_clip(), caps()) else {
        eprintln!("skip: ffmpeg or fixture unavailable");
        return;
    };
    if !caps.has_encoder("png") || !caps.has_muxer("image2") {
        eprintln!("skip: png encoder/image2 muxer unavailable");
        return;
    }
    let opts = FrameOptions {
        at_secs: 1.0,
        output_format: "png".to_string(),
        width: Some(64),
        height: None,
        quality: None,
    };
    let out = frame_at(clip, &opts, &limits(), rt).expect("frame_at failed");
    let bytes = primary_bytes(&out);
    assert!(bytes.starts_with(b"\x89PNG"), "output is not a PNG");
    assert!(
        out.primary.as_ref().unwrap().name.ends_with(".png"),
        "unexpected output name"
    );
}

// ---------------------------------------------------------------------------
// Audio ops
// ---------------------------------------------------------------------------

#[test]
fn adjust_volume_reencodes_audio_only() {
    let (Some(rt), Some(clip), Some(caps)) = (runtime(), test_clip(), caps()) else {
        eprintln!("skip: ffmpeg or fixture unavailable");
        return;
    };
    if !caps.has_encoder("aac") || !caps.has_muxer("mp4") || !caps.has_filter("volume") {
        eprintln!("skip: aac/mp4/volume filter unavailable");
        return;
    }
    let opts = VolumeOptions {
        gain_db: -3.0,
        peak_normalize: false,
        output_format: Some("mp4".to_string()),
        codec: Some("aac".to_string()),
        bitrate: Some("96k".to_string()),
    };
    let out = adjust_volume(clip, &opts, &limits(), rt).expect("adjust_volume failed");
    let info = inspect(primary_bytes(&out), &limits()).expect("cannot inspect volume output");
    assert_eq!(info.audio_streams, 1, "expected one audio stream");
    assert_eq!(info.video_streams, 0, "video should be dropped");
    assert_eq!(
        info.primary_audio.and_then(|a| a.codec),
        Some("aac".to_string())
    );
}

#[test]
fn trim_audio_accurate_uses_available_codec() {
    let (Some(rt), Some(clip), Some(caps)) = (runtime(), test_clip(), caps()) else {
        eprintln!("skip: ffmpeg or fixture unavailable");
        return;
    };
    let opts = AudioTrimOptions {
        start: 0.5,
        end: Some(1.5),
        accurate: true,
        output_format: Some("m4a".to_string()),
        codec: Some("aac".to_string()),
    };
    let res = trim_audio(clip, &opts, &limits(), rt);
    if caps.has_encoder("aac") {
        let out = res.expect("accurate audio trim failed despite aac encoder");
        let info =
            inspect(primary_bytes(&out), &limits()).expect("cannot inspect audio-trim output");
        assert_eq!(info.audio_streams, 1);
        assert_eq!(info.video_streams, 0, "video should be dropped");
    } else {
        assert!(
            matches!(res, Err(MediaError::Unsupported(_))),
            "expected Unsupported without aac, got {res:?}"
        );
    }
}

#[test]
fn loudness_measure_and_normalize() {
    let (Some(rt), Some(clip), Some(caps)) = (runtime(), test_clip(), caps()) else {
        eprintln!("skip: ffmpeg or fixture unavailable");
        return;
    };
    if !caps.has_filter("loudnorm") {
        eprintln!("skip: loudnorm filter unavailable");
        return;
    }
    let report = measure_loudness(clip, &limits(), rt).expect("measure_loudness failed");
    // The sine fixture is non-silent, so integrated loudness must be finite.
    assert!(
        report.measured_i.is_some_and(|v| v.is_finite()),
        "loudnorm reported no finite integrated loudness: {report:?}"
    );

    if caps.has_muxer("mp4") && caps.has_encoder("aac") {
        let opts = NormalizeOptions {
            output_format: "mp4".to_string(),
            audio_codec: Some("aac".to_string()),
            audio_bitrate: Some("96k".to_string()),
            ..NormalizeOptions::default()
        };
        let out =
            normalize_loudness(clip, &opts, &limits(), rt).expect("normalize_loudness failed");
        let info =
            inspect(primary_bytes(&out), &limits()).expect("cannot inspect normalized output");
        assert_eq!(info.audio_streams, 1);
        assert_eq!(info.video_streams, 0);
    }
}

// ---------------------------------------------------------------------------
// Metadata ops
// ---------------------------------------------------------------------------

#[test]
fn edit_tags_then_read_back() {
    let (Some(rt), Some(clip)) = (runtime(), test_clip()) else {
        eprintln!("skip: ffmpeg or fixture unavailable");
        return;
    };
    let opts = TagOptions {
        format_tags: vec![TagEdit {
            key: "title".to_string(),
            value: Some("e2e multimedia title".to_string()),
        }],
        stream_tags: Vec::new(),
    };
    let tagged = edit_tags(clip, &opts, &limits(), rt).expect("edit_tags failed");
    let tagged_bytes = primary_bytes(&tagged);
    assert!(!tagged_bytes.is_empty());

    let report = read_tags(tagged_bytes, &limits(), rt).expect("read_tags failed");
    let title = report
        .format
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("title"))
        .map(|(_, v)| v.clone());
    assert_eq!(
        title.as_deref(),
        Some("e2e multimedia title"),
        "title tag not readable after edit: {:?}",
        report.format
    );

    // strip_metadata should remove it again.
    let stripped = strip_metadata(tagged_bytes, &StripOptions::default(), &limits(), rt)
        .expect("strip_metadata failed");
    let stripped_report =
        read_tags(primary_bytes(&stripped), &limits(), rt).expect("read_tags after strip failed");
    assert!(
        !stripped_report
            .format
            .keys()
            .any(|k| k.eq_ignore_ascii_case("title")),
        "title survived strip_metadata: {:?}",
        stripped_report.format
    );
}

#[test]
fn cover_art_roundtrip_on_flac() {
    let (Some(rt), Some(flac), Some(img)) = (runtime(), flac_audio(), cover_image()) else {
        eprintln!("skip: flac/mjpeg fixtures or ffmpeg unavailable");
        return;
    };
    let with_art =
        add_cover_art(&flac, &img, &limits(), rt).expect("add_cover_art failed on flac+mjpeg");
    let out_bytes = primary_bytes(&with_art);
    assert!(!out_bytes.is_empty());

    // The remuxed file must still probe and carry an attached picture.
    let info = inspect(out_bytes, &limits()).expect("cannot inspect cover-art output");
    assert!(
        info.has_attached_pic,
        "attached picture not detected after add_cover_art"
    );

    let extracted = extract_cover_art(out_bytes, &limits(), rt)
        .expect("extract_cover_art failed")
        .expect("no cover art found to extract");
    let img_bytes = primary_bytes(&extracted);
    assert!(
        img_bytes.starts_with(b"\xFF\xD8") || img_bytes.starts_with(b"\x89PNG"),
        "extracted cover is not a jpeg/png"
    );
}

// ---------------------------------------------------------------------------
// Error paths
// ---------------------------------------------------------------------------

#[test]
fn garbage_input_is_invalid_input() {
    let Some(rt) = runtime() else {
        eprintln!("skip: ffmpeg unavailable");
        return;
    };
    let garbage = b"this is definitely not a media file \x00\x01\x02\x03";
    let err = inspect(garbage, &limits()).expect_err("inspect accepted garbage");
    assert!(
        matches!(err, MediaError::InvalidInput(_)),
        "expected InvalidInput, got {err:?}"
    );

    let err = trim(
        garbage,
        &TrimOptions {
            start: 0.0,
            end: None,
            accurate: false,
            output_format: Some("mp4".to_string()),
            audio_trim: true,
        },
        &limits(),
        rt,
    );
    // Garbage may fail as InvalidInput or ToolFailed depending on where
    // ffmpeg rejects it — both are honest errors, never a panic or Ok.
    assert!(err.is_err(), "trim accepted garbage input");
}

#[test]
fn empty_and_oversized_inputs_hit_limits() {
    let (Some(rt), Some(clip)) = (runtime(), test_clip()) else {
        eprintln!("skip: ffmpeg or fixture unavailable");
        return;
    };

    // Empty input → InvalidInput before any tool runs.
    let err = trim(
        &[],
        &TrimOptions {
            start: 0.0,
            end: None,
            accurate: false,
            output_format: Some("mp4".to_string()),
            audio_trim: true,
        },
        &limits(),
        rt,
    )
    .expect_err("empty input accepted");
    assert!(
        matches!(err, MediaError::InvalidInput(_)),
        "expected InvalidInput for empty input, got {err:?}"
    );

    // Tiny max_input_bytes → LimitExceeded.
    let tiny = MediaLimits {
        max_input_bytes: Some(8),
        ..MediaLimits::default()
    };
    let err = trim(
        clip,
        &TrimOptions {
            start: 0.0,
            end: None,
            accurate: false,
            output_format: Some("mp4".to_string()),
            audio_trim: true,
        },
        &tiny,
        rt,
    )
    .expect_err("oversized input accepted");
    assert!(
        matches!(err, MediaError::LimitExceeded(_)),
        "expected LimitExceeded, got {err:?}"
    );
}

#[test]
fn invalid_option_values_are_rejected() {
    let (Some(rt), Some(clip)) = (runtime(), test_clip()) else {
        eprintln!("skip: ffmpeg or fixture unavailable");
        return;
    };

    // Negative seek time.
    let err = frame_at(
        clip,
        &FrameOptions {
            at_secs: -1.0,
            ..FrameOptions::default()
        },
        &limits(),
        rt,
    )
    .expect_err("negative at_secs accepted");
    assert!(matches!(err, MediaError::InvalidInput(_)));

    // end <= start.
    let err = trim(
        clip,
        &TrimOptions {
            start: 1.0,
            end: Some(0.5),
            accurate: false,
            output_format: Some("mp4".to_string()),
            audio_trim: true,
        },
        &limits(),
        rt,
    )
    .expect_err("end <= start accepted");
    assert!(matches!(err, MediaError::InvalidInput(_)));

    // concat needs >= 2 inputs.
    let err = concat(
        &[MediaInput {
            name: "only.mp4".to_string(),
            data: clip.to_vec(),
        }],
        &ConcatOptions::default(),
        &limits(),
        rt,
    )
    .expect_err("single-input concat accepted");
    assert!(matches!(err, MediaError::InvalidInput(_)));

    // Unsupported thumbnail format is an honest Unsupported, not a crash.
    let err = thumbnail(
        clip,
        &ThumbnailOptions {
            output_format: "tiff".to_string(),
            ..ThumbnailOptions::default()
        },
        &limits(),
        rt,
    )
    .expect_err("bogus thumbnail format accepted");
    assert!(matches!(err, MediaError::Unsupported(_)));
}
