//! Embedded-stream extraction: subtitles, attachments, cover art,
//! chapters, and data streams — pulled out of the container without
//! re-encoding where the format allows.
//!
//! [`extract_streams`] probes the input once, then issues one
//! `ffmpeg -map 0:{index}` invocation per selected stream. Stream-copy
//! (`-c copy`) is used everywhere the target container accepts the
//! codec; subtitle tracks are the exception — they are transcoded to
//! the requested text format (`srt`/`ass`/`vtt`) because raw subtitle
//! packets are container-specific and text is the portable form.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::{MediaFile, MediaOutput, StreamInfo, StreamSelection};
use crate::crates::video_engine::ops::probe::parse_probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, find_tool, probe_json,
    read_output, run_tool, tool_failure, write_input,
};

/// Subtitle text formats [`extract_streams`] accepts, mapped to the
/// ffmpeg subtitle encoder that produces them.
const SUBTITLE_FORMATS: &[(&str, &str)] = &[("srt", "srt"), ("ass", "ass"), ("vtt", "webvtt")];

/// Extension for a still-image attachment/cover stream. Returns `None`
/// for codecs with no standalone image muxer — callers fall back to
/// `.mkv`, which carries arbitrary codec payloads.
fn image_ext(codec_name: Option<&str>) -> Option<&'static str> {
    match codec_name.unwrap_or("") {
        "mjpeg" | "jpeg" => Some("jpg"),
        "png" => Some("png"),
        "webp" => Some("webp"),
        "bmp" => Some("bmp"),
        "gif" => Some("gif"),
        "tiff" => Some("tif"),
        _ => None,
    }
}

/// Whether `stream` is attached cover art: a video stream flagged by
/// `disposition.attached_pic` (the probe model already folds that flag
/// into [`StreamInfo::attached_pic`]).
fn is_cover_art(stream: &StreamInfo) -> bool {
    stream.codec_type == "video" && stream.attached_pic
}

/// One planned `ffmpeg` invocation: stream-copy to `out.{ext}` or a
/// subtitle transcode to the text format.
enum ExtractJob {
    /// `ffmpeg -map 0:{index} -c copy` → `name`.
    Copy { index: u32, name: String },
    /// `ffmpeg -map 0:{index} -c:s {encoder}` → `name`.
    Subtitle {
        index: u32,
        name: String,
        encoder: &'static str,
    },
}

/// Build the deterministic output name for a stream: `{kind}_{index}`
/// with a sanitized codec/kind tag so callers can tell tracks apart.
fn stream_name(stream: &StreamInfo, ext: &str) -> String {
    let kind = sanitize_component(&stream.codec_type);
    format!("{}_{}.{}", kind, stream.index, ext)
}

/// Plan every file [`extract_streams`] will emit for `streams`, in
/// demuxer order. Selection flags that match nothing simply produce
/// no jobs — the caller decides whether an empty result is an error.
fn plan_jobs(
    streams: &[StreamInfo],
    selection: &StreamSelection,
    subtitle_encoder: &'static str,
) -> Vec<ExtractJob> {
    let mut jobs = Vec::new();
    for stream in streams {
        match stream.codec_type.as_str() {
            "subtitle" if selection.subtitles => {
                jobs.push(ExtractJob::Subtitle {
                    index: stream.index,
                    name: stream_name(stream, subtitle_encoder_to_ext(subtitle_encoder)),
                    encoder: subtitle_encoder,
                });
            }
            "attachment" if selection.attachments => {
                let ext = image_ext(stream.codec_name.as_deref()).unwrap_or("mkv");
                jobs.push(ExtractJob::Copy {
                    index: stream.index,
                    name: stream_name(stream, ext),
                });
            }
            "video" if selection.cover_art && is_cover_art(stream) => {
                let ext = image_ext(stream.codec_name.as_deref()).unwrap_or("mkv");
                jobs.push(ExtractJob::Copy {
                    index: stream.index,
                    name: stream_name(stream, ext),
                });
            }
            // First-class kinds are never "data": video (including
            // attached pics not selected as cover), audio, subtitle,
            // and attachment streams are only pulled by their own
            // flags. `data` covers every remaining codec_type ffprobe
            // reports (`data`, `unknown`, bin_data-like kinds, ...).
            "video" | "audio" | "subtitle" | "attachment" => {}
            _ if selection.data => {
                jobs.push(ExtractJob::Copy {
                    index: stream.index,
                    name: stream_name(stream, "mkv"),
                });
            }
            _ => {}
        }
    }
    jobs
}

/// File extension matching a subtitle encoder (`webvtt` → `vtt`).
fn subtitle_encoder_to_ext(encoder: &str) -> &'static str {
    match encoder {
        "webvtt" => "vtt",
        "ass" => "ass",
        _ => "srt",
    }
}

/// Run one [`ExtractJob`] against `input_path` inside `dir`, returning
/// the produced [`MediaFile`]. Tool failures propagate as
/// [`VideoError::ExternalTool`] with the stderr tail.
fn run_job(
    program: &std::path::Path,
    dir: &TempDir,
    input_path: &std::path::Path,
    job: &ExtractJob,
) -> VideoResult<MediaFile> {
    let (index, name) = match job {
        ExtractJob::Copy { index, name } => (*index, name.clone()),
        ExtractJob::Subtitle { index, name, .. } => (*index, name.clone()),
    };
    let out_path = dir.file(&name);

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-map".to_string(),
        format!("0:{index}"),
    ];
    match job {
        ExtractJob::Copy { .. } => {
            args.push("-c".to_string());
            args.push("copy".to_string());
        }
        ExtractJob::Subtitle { encoder, .. } => {
            args.push("-c:s".to_string());
            args.push((*encoder).to_string());
        }
    }
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    let bytes = read_output(&out_path)?;
    Ok(MediaFile { name, bytes })
}

/// Extract the stream kinds selected by `selection` into named
/// files. Subtitle streams are converted to `subtitle_format`
/// (`srt`, `ass`, or `vtt` — best effort per source codec). Chapters
/// export as `chapters.json`. Cover art exports as an image file
/// with a sniffed extension. Attachments keep their codec name.
///
/// Enforces `limits.max_outputs`; names are sanitized relative
/// paths safe for callers to write under a directory.
pub(crate) fn extract_streams(
    input: &[u8],
    selection: &StreamSelection,
    subtitle_format: &str,
    limits: &EngineLimits,
) -> VideoResult<MediaOutput> {
    let input_len = check_input(input)?;
    let program = find_tool(FFMPEG)?;

    // Subtitle tracks are transcoded to text; validate the requested
    // format up front so a typo fails before ffmpeg runs.
    let (_, encoder) = SUBTITLE_FORMATS
        .iter()
        .find(|(f, _)| *f == subtitle_format.trim().to_ascii_lowercase())
        .copied()
        .ok_or_else(|| {
            VideoError::InvalidInput(format!(
                "extract_streams subtitle_format '{subtitle_format}' is not supported; \
                 allowed: {}",
                SUBTITLE_FORMATS
                    .iter()
                    .map(|(f, _)| *f)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
    let subtitle_encoder: &'static str = encoder;

    // Probe once: limits, stream table, and chapters all come from the
    // same document, and the scratch file doubles as ffmpeg's `-i`.
    let dir = TempDir::new("extract")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let json = probe_json(&input_path)?;
    let info = parse_probe(&json, input_len)?;
    check_limits(&info, input_len, limits, false)?;

    let jobs = plan_jobs(&info.streams, selection, subtitle_encoder);
    // Cover-art streams are video-kind; `data` must not also claim
    // them when both flags are set — plan_jobs already routes them to
    // the cover arm first, so nothing to dedupe here.

    if let Some(max) = limits.max_outputs {
        let planned = jobs.len() + usize::from(selection.chapters);
        if planned as u64 > max as u64 {
            return Err(VideoError::LimitExceeded(format!(
                "selection would produce {planned} files, exceeding max_outputs {max}"
            )));
        }
    }

    let mut files = Vec::with_capacity(jobs.len() + 1);
    for job in &jobs {
        files.push(run_job(&program, &dir, &input_path, job)?);
    }

    if selection.chapters {
        files.push(MediaFile {
            name: "chapters.json".to_string(),
            bytes: chapters_json(&info.chapters)?,
        });
    }

    if files.is_empty() {
        return Err(VideoError::InvalidInput(
            "extract_streams selection matched no streams in the input".to_string(),
        ));
    }
    Ok(MediaOutput { files })
}

/// Export chapters as a JSON document matching the probe model
/// (`[{id, start_secs, end_secs, title}]`).
pub(crate) fn chapters_json(
    chapters: &[crate::crates::video_engine::model::ChapterInfo],
) -> VideoResult<Vec<u8>> {
    serde_json::to_vec_pretty(chapters).map_err(Into::into)
}

/// Sanitize a stream-derived filename component (codec names,
/// languages, indices) into `[a-z0-9_-]`.
pub(crate) fn sanitize_component(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}
