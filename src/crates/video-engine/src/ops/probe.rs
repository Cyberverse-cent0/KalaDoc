//! `ffprobe`-backed media inspection.
//!
//! [`probe`] writes the input to a scratch file, runs
//! `ffprobe -v quiet -print_format json -show_format -show_streams`
//! and navigates the JSON directly — no typed mirror structs, because
//! ffprobe's schema varies by demuxer. The first video stream supplies
//! dimensions/codec/frame-rate, the first audio stream the audio codec,
//! and `format` supplies duration/name/size. Absent fields stay `None`;
//! nothing is fabricated.

use serde_json::Value;

use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::VideoInfo;
use crate::crates::video_engine::ops::{
    TempDir, check_input, probe_json, write_input,
};

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

    let video = streams
        .iter()
        .find(|s| s.get("codec_type").and_then(Value::as_str) == Some("video"));
    let audio = streams
        .iter()
        .find(|s| s.get("codec_type").and_then(Value::as_str) == Some("audio"));

    let (width, height, vcodec, fps) = match video {
        Some(s) => (
            s.get("width").and_then(Value::as_u64).map(|v| v as u32),
            s.get("height").and_then(Value::as_u64).map(|v| v as u32),
            s.get("codec_name")
                .and_then(Value::as_str)
                .map(str::to_string),
            frame_rate(s),
        ),
        None => (None, None, None, None),
    };

    let format = json.get("format");
    let duration_secs = format
        .and_then(|f| f.get("duration"))
        .and_then(parse_f64);
    let format_name = format
        .and_then(|f| f.get("format_name"))
        .and_then(Value::as_str)
        .map(str::to_string);
    // ffprobe reports the *container's* size; when absent, fall back to
    // the actual byte count we wrote — it is the same file.
    let size_bytes = format
        .and_then(|f| f.get("size"))
        .and_then(|v| v.as_str().and_then(|s| s.parse::<u64>().ok()).or_else(|| v.as_u64()))
        .unwrap_or(input_len);

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
    })
}

/// ffprobe emits numbers inconsistently — `duration`/`size` are usually
/// strings, occasionally JSON numbers. Accept both.
fn parse_f64(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
        .filter(|f| f.is_finite() && *f >= 0.0)
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
}
