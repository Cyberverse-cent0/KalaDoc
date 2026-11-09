//! `ffprobe`-backed media inspection.
//!
//! [`probe`] writes the input to a scratch file, runs
//! `ffprobe -v quiet -print_format json -show_format -show_streams -show_chapters`
//! and navigates the JSON directly — no typed mirror structs, because
//! ffprobe's schema varies by demuxer. The first *primary* video stream
//! (not attached-pic cover art) supplies dimensions/codec/frame-rate, the
//! first audio stream the audio codec, and `format` supplies
//! duration/name/size. Absent fields stay `None`; nothing is fabricated.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::{ChapterInfo, StreamInfo, VideoInfo};
use crate::crates::video_engine::ops::{TempDir, check_input, probe_json, write_input};

/// Inspect `input` and return its [`VideoInfo`].
///
/// `input` is written to a scratch file because ffprobe operates on
/// paths, not stdin buffers. A non-zero ffprobe exit (not a media
/// container, truncated header, ...) is [`VideoError::InvalidInput`].
pub(crate) fn probe(input: &[u8]) -> VideoResult<VideoInfo> {
    check_input(input)?;
    let dir = TempDir::new("probe")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let json = probe_json(&input_path)?;
    Ok(parse_probe(&json, input.len() as u64)?)
}

/// Parse the ffprobe JSON document into [`VideoInfo`]. Split from
/// [`probe`] so tests can exercise parsing without a subprocess.
pub(crate) fn parse_probe(json: &Value, input_len: u64) -> VideoResult<VideoInfo> {
    let streams = json
        .get("streams")
        .and_then(Value::as_array)
        .ok_or_else(|| VideoError::InvalidInput("ffprobe reported no streams".into()))?;
    if streams.is_empty() {
        return Err(VideoError::InvalidInput(
            "ffprobe reported an empty stream list".into(),
        ));
    }

    // Cover art is commonly exposed as a `video` stream (mjpeg/png with
    // disposition.attached_pic=1); it must never win the primary-stream
    // slots for dimensions/codec/frame-rate.
    let video = primary_video_stream(streams);
    let audio = streams
        .iter()
        .find(|s| s.get("codec_type").and_then(Value::as_str) == Some("audio"));

    let (width, height, vcodec, fps) = match video {
        Some(s) => (
            s.get("width")
                .and_then(num_u64)
                .and_then(|v| u32::try_from(v).ok()),
            s.get("height")
                .and_then(num_u64)
                .and_then(|v| u32::try_from(v).ok()),
            s.get("codec_name")
                .and_then(Value::as_str)
                .map(str::to_string),
            frame_rate(s),
        ),
        None => (None, None, None, None),
    };

    let format = json.get("format");
    let duration_secs = format.and_then(|f| f.get("duration")).and_then(parse_f64);
    let format_name = format
        .and_then(|f| f.get("format_name"))
        .and_then(Value::as_str)
        .map(str::to_string);
    // ffprobe reports the *container's* size; when absent, fall back to
    // the actual byte count we wrote — it is the same file.
    let size_bytes = format
        .and_then(|f| f.get("size"))
        .and_then(num_u64)
        .unwrap_or(input_len);

    let has_video = video.is_some();
    let stream_infos: Vec<StreamInfo> = streams.iter().map(stream_info).collect();
    let chapters: Vec<ChapterInfo> = json
        .get("chapters")
        .and_then(Value::as_array)
        .map(|chs| chs.iter().map(chapter_info).collect())
        .unwrap_or_default();
    let tags: BTreeMap<String, String> = format
        .and_then(|f| f.get("tags"))
        .and_then(Value::as_object)
        .map(|t| {
            t.iter()
                .map(|(k, v)| {
                    (
                        k.to_ascii_lowercase(),
                        v.as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let bit_rate = format.and_then(|f| f.get("bit_rate")).and_then(num_u64);
    let format_long_name = format
        .and_then(|f| f.get("format_long_name"))
        .and_then(Value::as_str)
        .map(str::to_string);

    Ok(VideoInfo {
        duration_secs,
        width,
        height,
        fps,
        vcodec,
        acodec: audio
            .and_then(|s| s.get("codec_name"))
            .and_then(Value::as_str)
            .map(str::to_string),
        format_name,
        size_bytes,
        has_audio: audio.is_some(),
        has_video,
        bit_rate,
        format_long_name,
        streams: stream_infos,
        chapters,
        tags,
    })
}

/// Whether `s` is a video stream flagged as attached cover art.
/// ffprobe reports `disposition.attached_pic` as `0`/`1` (occasionally a
/// numeric string on quirky demuxers).
fn is_attached_pic(s: &Value) -> bool {
    s.get("codec_type").and_then(Value::as_str) == Some("video")
        && s.get("disposition")
            .and_then(|d| d.get("attached_pic"))
            .and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|x| x.parse::<i64>().ok()))
            })
            .map(|v| v != 0)
            .unwrap_or(false)
}

/// Pick the primary video stream: skip attached-pic cover art, prefer
/// `disposition.default=1`, then the largest pixel count, keeping
/// document order as the final tiebreak.
fn primary_video_stream<'a>(streams: &'a [Value]) -> Option<&'a Value> {
    streams
        .iter()
        .filter(|s| s.get("codec_type").and_then(Value::as_str) == Some("video"))
        .filter(|s| !is_attached_pic(s))
        .fold(None, |best: Option<&Value>, s| {
            let Some(b) = best else { return Some(s) };
            if disposition_default(s) && !disposition_default(b) {
                return Some(s);
            }
            if disposition_default(s) == disposition_default(b) {
                let sp = pixel_count(s).unwrap_or(0);
                let bp = pixel_count(b).unwrap_or(0);
                if sp > bp {
                    return Some(s);
                }
            }
            Some(b)
        })
}

/// `disposition.default` flag (0/1, numeric string tolerated).
fn disposition_default(s: &Value) -> bool {
    s.get("disposition")
        .and_then(|d| d.get("default"))
        .and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|x| x.parse::<i64>().ok()))
        })
        .map(|v| v != 0)
        .unwrap_or(false)
}

/// `width * height` for ordering candidate primary streams.
fn pixel_count(s: &Value) -> Option<u64> {
    let w = s.get("width").and_then(num_u64)?;
    let h = s.get("height").and_then(num_u64)?;
    w.checked_mul(h)
}

/// Map one ffprobe chapter object to [`ChapterInfo`].
///
/// ffprobe emits `start_time`/`end_time` as decimal strings, `id` as a
/// number or string, and the title inside `tags` under whichever case
/// the muxer used (`title`, `TITLE`, ...).
fn chapter_info(c: &Value) -> ChapterInfo {
    let id = c.get("id").and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
    });
    let title = c
        .get("tags")
        .and_then(Value::as_object)
        .and_then(|t| {
            t.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("title"))
                .and_then(|(_, v)| v.as_str())
        })
        .map(str::to_string)
        .or_else(|| c.get("title").and_then(Value::as_str).map(str::to_string));
    ChapterInfo {
        id,
        start_secs: c.get("start_time").and_then(parse_f64).unwrap_or(0.0),
        end_secs: c.get("end_time").and_then(parse_f64).unwrap_or(0.0),
        title,
    }
}

/// Case-insensitive lookup into a tags object.
fn tag_value<'a>(tags: &'a Value, name: &str) -> Option<&'a str> {
    tags.as_object()?.iter().find_map(|(k, v)| {
        if k.eq_ignore_ascii_case(name) {
            v.as_str()
        } else {
            None
        }
    })
}

/// Map one ffprobe stream object to [`StreamInfo`].
fn stream_info(s: &Value) -> StreamInfo {
    let codec_type = s
        .get("codec_type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let tags: BTreeMap<String, String> = s
        .get("tags")
        .and_then(Value::as_object)
        .map(|t| {
            t.iter()
                .map(|(k, v)| {
                    (
                        k.to_ascii_lowercase(),
                        v.as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    // `language` commonly arrives as `tags.language`; some demuxers put
    // it at top level or under a differently-cased tag key.
    let language = tag_value(s.get("tags").unwrap_or(&Value::Null), "language")
        .or_else(|| s.get("language").and_then(Value::as_str))
        .map(str::to_string)
        .or_else(|| tags.get("language").cloned());
    StreamInfo {
        index: s.get("index").and_then(num_u64).unwrap_or(0) as u32,
        codec_type: codec_type.clone(),
        codec_name: s
            .get("codec_name")
            .and_then(Value::as_str)
            .map(str::to_string),
        profile: s.get("profile").and_then(Value::as_str).map(str::to_string),
        width: s
            .get("width")
            .and_then(num_u64)
            .and_then(|v| u32::try_from(v).ok()),
        height: s
            .get("height")
            .and_then(num_u64)
            .and_then(|v| u32::try_from(v).ok()),
        pix_fmt: s.get("pix_fmt").and_then(Value::as_str).map(str::to_string),
        display_aspect_ratio: s
            .get("display_aspect_ratio")
            .and_then(Value::as_str)
            .map(str::to_string),
        field_order: s
            .get("field_order")
            .and_then(Value::as_str)
            .map(str::to_string),
        sample_rate: s
            .get("sample_rate")
            .and_then(num_u64)
            .and_then(|v| u32::try_from(v).ok()),
        channels: s
            .get("channels")
            .and_then(num_u64)
            .and_then(|v| u32::try_from(v).ok()),
        channel_layout: s
            .get("channel_layout")
            .and_then(Value::as_str)
            .map(str::to_string),
        language,
        is_default: disposition_default(s),
        attached_pic: is_attached_pic(s),
        bit_rate: s.get("bit_rate").and_then(num_u64),
        duration_secs: s.get("duration").and_then(parse_f64),
        tags,
    }
}

/// ffprobe emits numbers inconsistently — `duration`/`size`/`bit_rate`/
/// `sample_rate` are usually strings, occasionally JSON numbers, and a
/// few demuxers even emit floats for integer fields. Accept all forms.
fn parse_f64(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
        .filter(|f| f.is_finite() && *f >= 0.0)
}

/// [`num_u64`] for callers that also want float-shaped fields rejected
/// cleanly: non-integral floats yield `None` rather than truncating.
fn num_u64(v: &Value) -> Option<u64> {
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    if let Some(f) = v.as_f64() {
        return (f.is_finite() && f >= 0.0 && f.fract() == 0.0).then_some(f as u64);
    }
    v.as_str()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|f| f.is_finite() && *f >= 0.0 && f.fract() == 0.0)
        .map(|f| f as u64)
}

/// Parse an ffprobe frame-rate field (`"30000/1001"`, `"25/1"`, or a
/// bare number). `"0/0"` and malformed values yield `None`.
fn frame_rate(stream: &Value) -> Option<f64> {
    for key in ["avg_frame_rate", "r_frame_rate"] {
        let Some(raw) = stream.get(key).and_then(Value::as_str) else {
            continue;
        };
        if let Some((num, den)) = raw.split_once('/') {
            let (Ok(num), Ok(den)) = (num.parse::<f64>(), den.parse::<f64>()) else {
                continue;
            };
            if den != 0.0 && num.is_finite() && num > 0.0 {
                return Some(num / den);
            }
            continue;
        }
        if let Ok(v) = raw.parse::<f64>() {
            if v.is_finite() && v > 0.0 {
                return Some(v);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_typical_mp4_json() {
        let doc = json!({
            "streams": [
                {
                    "codec_type": "video",
                    "codec_name": "h264",
                    "width": 320,
                    "height": 240,
                    "avg_frame_rate": "10/1",
                    "r_frame_rate": "10/1"
                },
                {
                    "codec_type": "audio",
                    "codec_name": "aac"
                }
            ],
            "format": {
                "duration": "1.000000",
                "format_name": "mov,mp4,m4a,3gp,3g2,mj2",
                "size": "12345"
            }
        });
        let info = parse_probe(&doc, 999).unwrap();
        assert_eq!(info.duration_secs, Some(1.0));
        assert_eq!(info.width, Some(320));
        assert_eq!(info.height, Some(240));
        assert_eq!(info.fps, Some(10.0));
        assert_eq!(info.vcodec.as_deref(), Some("h264"));
        assert_eq!(info.acodec.as_deref(), Some("aac"));
        assert!(info.has_audio);
        assert_eq!(info.size_bytes, 12345);
    }

    #[test]
    fn missing_fields_stay_none() {
        let doc = json!({
            "streams": [{"codec_type": "video", "avg_frame_rate": "0/0"}],
            "format": {"format_name": "matroska,webm"}
        });
        let info = parse_probe(&doc, 42).unwrap();
        assert_eq!(info.duration_secs, None);
        assert_eq!(info.fps, None);
        assert_eq!(info.acodec, None);
        assert!(!info.has_audio);
        // size falls back to the actual input length.
        assert_eq!(info.size_bytes, 42);
    }

    #[test]
    fn empty_streams_is_invalid() {
        let doc = json!({"streams": [], "format": {}});
        assert!(matches!(
            parse_probe(&doc, 1),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn fractional_frame_rate() {
        let doc = json!({
            "streams": [{"codec_type": "video", "avg_frame_rate": "30000/1001"}],
            "format": {}
        });
        let info = parse_probe(&doc, 1).unwrap();
        let fps = info.fps.unwrap();
        assert!((fps - 29.970).abs() < 0.01);
    }

    #[test]
    fn attached_pic_video_is_skipped_for_primary_fields() {
        // Cover art (mjpeg, disposition.attached_pic=1) appears *before*
        // the real video stream in many mp3/flac-with-art files.
        let doc = json!({
            "streams": [
                {
                    "index": 0,
                    "codec_type": "video",
                    "codec_name": "mjpeg",
                    "width": 500,
                    "height": 500,
                    "avg_frame_rate": "90000/1",
                    "disposition": {"default": 0, "attached_pic": 1}
                },
                {
                    "index": 1,
                    "codec_type": "video",
                    "codec_name": "h264",
                    "width": 640,
                    "height": 360,
                    "avg_frame_rate": "24/1",
                    "disposition": {"default": 1, "attached_pic": 0}
                }
            ],
            "format": {}
        });
        let info = parse_probe(&doc, 1).unwrap();
        assert_eq!(info.width, Some(640));
        assert_eq!(info.height, Some(360));
        assert_eq!(info.fps, Some(24.0));
        assert_eq!(info.vcodec.as_deref(), Some("h264"));
        // ...but the attached-pic stream is still reported in `streams`.
        assert_eq!(info.streams.len(), 2);
        assert!(info.streams[0].attached_pic);
        assert!(!info.streams[1].attached_pic);
    }

    #[test]
    fn attached_pic_string_disposition_is_detected() {
        let doc = json!({
            "streams": [{
                "codec_type": "video",
                "codec_name": "png",
                "disposition": {"attached_pic": "1"}
            }],
            "format": {}
        });
        let info = parse_probe(&doc, 1).unwrap();
        // A file with ONLY cover art has no primary video stream.
        assert!(!info.has_video);
        assert_eq!(info.vcodec, None);
        assert!(info.streams[0].attached_pic);
    }

    #[test]
    fn primary_video_prefers_default_then_largest() {
        // Neither flagged default: the larger frame wins.
        let doc = json!({
            "streams": [
                {"codec_type": "video", "codec_name": "h264", "width": 320, "height": 240},
                {"codec_type": "video", "codec_name": "vp9", "width": 1280, "height": 720}
            ],
            "format": {}
        });
        let info = parse_probe(&doc, 1).unwrap();
        assert_eq!(info.width, Some(1280));
        assert_eq!(info.vcodec.as_deref(), Some("vp9"));

        // An explicit default wins even at lower resolution.
        let doc = json!({
            "streams": [
                {
                    "codec_type": "video", "codec_name": "vp9",
                    "width": 1280, "height": 720,
                    "disposition": {"default": 0}
                },
                {
                    "codec_type": "video", "codec_name": "h264",
                    "width": 320, "height": 240,
                    "disposition": {"default": 1}
                }
            ],
            "format": {}
        });
        let info = parse_probe(&doc, 1).unwrap();
        assert_eq!(info.width, Some(320));
        assert_eq!(info.vcodec.as_deref(), Some("h264"));
    }

    #[test]
    fn chapters_parse_string_times_and_varied_id() {
        let doc = json!({
            "streams": [{"codec_type": "video", "codec_name": "h264"}],
            "format": {},
            "chapters": [
                {
                    "id": "0",
                    "start_time": "0.000000",
                    "end_time": "10.500000",
                    "tags": {"title": "Intro"}
                },
                {
                    "id": 1,
                    "start_time": 10.5,
                    "end_time": "20.000000",
                    "tags": {"TITLE": "Main"}
                },
                {
                    "start_time": "20.000000",
                    "end_time": "30.000000"
                }
            ]
        });
        let info = parse_probe(&doc, 1).unwrap();
        assert_eq!(info.chapters.len(), 3);
        let c0 = &info.chapters[0];
        assert_eq!(c0.id, Some(0));
        assert_eq!(c0.start_secs, 0.0);
        assert_eq!(c0.end_secs, 10.5);
        assert_eq!(c0.title.as_deref(), Some("Intro"));
        let c1 = &info.chapters[1];
        assert_eq!(c1.id, Some(1));
        assert_eq!(c1.start_secs, 10.5);
        // Case-insensitive title lookup picks up `TITLE`.
        assert_eq!(c1.title.as_deref(), Some("Main"));
        // Missing id/tags stay absent.
        assert_eq!(info.chapters[2].id, None);
        assert_eq!(info.chapters[2].title, None);
    }

    #[test]
    fn string_numbers_coerced_in_streams() {
        let doc = json!({
            "streams": [{
                "index": "0",
                "codec_type": "audio",
                "codec_name": "aac",
                "sample_rate": "44100",
                "channels": "2",
                "channel_layout": "stereo",
                "bit_rate": "128000",
                "duration": "61.234000",
                "tags": {"language": "eng"}
            }],
            "format": {"bit_rate": "130000", "size": "5000"}
        });
        let info = parse_probe(&doc, 1).unwrap();
        let s = &info.streams[0];
        assert_eq!(s.index, 0);
        assert_eq!(s.sample_rate, Some(44100));
        assert_eq!(s.channels, Some(2));
        assert_eq!(s.channel_layout.as_deref(), Some("stereo"));
        assert_eq!(s.bit_rate, Some(128000));
        assert_eq!(s.duration_secs, Some(61.234));
        assert_eq!(s.language.as_deref(), Some("eng"));
        assert_eq!(info.bit_rate, Some(130000));
        assert_eq!(info.size_bytes, 5000);
    }

    #[test]
    fn language_falls_back_to_top_level_field() {
        let doc = json!({
            "streams": [{
                "codec_type": "video", "codec_name": "h264",
                "language": "jpn"
            }],
            "format": {}
        });
        let info = parse_probe(&doc, 1).unwrap();
        assert_eq!(info.streams[0].language.as_deref(), Some("jpn"));
    }

    #[test]
    fn multi_audio_first_wins_acodec_but_all_reported() {
        let doc = json!({
            "streams": [
                {"codec_type": "video", "codec_name": "h264", "width": 100, "height": 100},
                {"codec_type": "audio", "codec_name": "aac"},
                {"codec_type": "audio", "codec_name": "mp3"}
            ],
            "format": {}
        });
        let info = parse_probe(&doc, 1).unwrap();
        assert_eq!(info.acodec.as_deref(), Some("aac"));
        assert!(info.has_audio);
        assert_eq!(info.streams.len(), 3);
    }

    #[test]
    fn video_only_no_audio() {
        let doc = json!({
            "streams": [{"codec_type": "video", "codec_name": "vp8"}],
            "format": {"format_name": "webm"}
        });
        let info = parse_probe(&doc, 1).unwrap();
        assert!(info.has_video);
        assert!(!info.has_audio);
        assert_eq!(info.acodec, None);
    }

    /// End-to-end check against real ffmpeg/ffprobe: synthesise a 1s
    /// clip with video+audio, write it, and probe it. Skips gracefully
    /// when ffmpeg or the H.264 encoder is unavailable.
    #[test]
    fn probe_real_media() {
        use crate::crates::video_engine::ops::{
            FFMPEG, PROBE_TIMEOUT, encoder_available, find_tool, run_tool,
        };

        if find_tool(FFMPEG).is_err() || !encoder_available("libopenh264") {
            return; // ffmpeg or the H.264 encoder is missing — skip.
        }
        let program = find_tool(FFMPEG).unwrap();
        let dir = TempDir::new("probe-test").unwrap();
        let clip = dir.file("clip.mp4");
        let args: Vec<String> = [
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=160x120:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:v",
            "libopenh264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
        ]
        .iter()
        .map(|s| s.to_string())
        .chain([clip.to_string_lossy().into_owned()])
        .collect();
        let out = run_tool(FFMPEG, &program, &args, PROBE_TIMEOUT).unwrap();
        assert!(
            out.status.success(),
            "ffmpeg failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let bytes = std::fs::read(&clip).unwrap();

        let info = probe(&bytes).unwrap();
        assert_eq!(info.width, Some(160));
        assert_eq!(info.height, Some(120));
        assert!(info.has_video);
        assert!(info.has_audio);
        assert!(info.streams.len() >= 2);
        assert_eq!(info.acodec.as_deref(), Some("aac"));
        let fmt = info.format_name.unwrap_or_default();
        assert!(
            fmt.split(',')
                .any(|t| t == "mp4" || t == "mov" || t == "m4a"),
            "unexpected format_name: {fmt}"
        );
    }
}
