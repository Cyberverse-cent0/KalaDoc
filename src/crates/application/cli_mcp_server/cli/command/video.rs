//! `kaladoc video <op>` — thin adapter over [`VideoEngine`].
//!
//! Reads the input file(s), calls the matching engine method, and
//! either prints the structured report or writes the produced bytes
//! to `--output-file` (single-file ops) / `--output-dir` (multi-file
//! ops). No configuration or database access is needed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::{
    CaptureDevice, FitMode, FramesSpec, MediaOutput, StreamFilter, StreamSelection, SubtitleStyle,
    VideoEngine, VideoError, VisualStyle,
};

use super::super::args::{FitArg, OrientArg, VideoArgs, VisualStyleArg};
use super::{CliError, Context};

pub fn run(ctx: &Context, args: &VideoArgs) -> Result<i32, CliError> {
    let engine = VideoEngine::new();
    let limits = EngineLimits::default();
    match args {
        VideoArgs::Probe { file } => probe(ctx, &engine, file),
        VideoArgs::Thumbnail {
            file,
            at,
            output_file: output,
        } => thumbnail(ctx, &engine, file, *at, output.as_deref()),
        VideoArgs::Trim {
            file,
            start,
            end,
            reencode,
            output_file: output,
        } => trim(
            ctx,
            &engine,
            file,
            *start,
            *end,
            *reencode,
            output.as_deref(),
        ),
        VideoArgs::Transcode {
            file,
            target_ext,
            output_file: output,
        } => transcode(ctx, &engine, file, target_ext, output.as_deref()),
        VideoArgs::Concat {
            files,
            target_ext,
            output_file: output,
        } => concat(ctx, &engine, files, target_ext, output.as_deref()),
        VideoArgs::ExtractAudio {
            file,
            ext,
            output_file: output,
        } => extract_audio(ctx, &engine, file, ext, output.as_deref()),

        // Containers / remux / encode profiles
        VideoArgs::Remux {
            file,
            target_ext,
            output_file: output,
        } => remux(ctx, &engine, file, target_ext, output.as_deref(), &limits),
        VideoArgs::Encode {
            file,
            profile,
            ext,
            output_file: output,
        } => encode(
            ctx,
            &engine,
            file,
            profile,
            ext.as_deref(),
            output.as_deref(),
            &limits,
        ),
        VideoArgs::Presets => presets(ctx, &engine),

        // Geometry
        VideoArgs::Resize {
            file,
            width,
            height,
            fit,
            output_file: output,
        } => geometry1(
            ctx,
            &engine,
            file,
            "resized",
            output.as_deref(),
            &limits,
            |input| {
                if *width == 0 || *height == 0 {
                    return Err(VideoError::InvalidInput(
                        "width and height must be > 0".to_string(),
                    ));
                }
                engine.resize_video(input, *width, *height, fit_mode(*fit), &limits)
            },
        ),
        VideoArgs::Crop {
            file,
            x,
            y,
            width,
            height,
            output_file: output,
        } => geometry1(
            ctx,
            &engine,
            file,
            "cropped",
            output.as_deref(),
            &limits,
            |input| {
                if *width == 0 || *height == 0 {
                    return Err(VideoError::InvalidInput(
                        "width and height must be > 0".to_string(),
                    ));
                }
                engine.crop_video(input, *x, *y, *width, *height, &limits)
            },
        ),
        VideoArgs::Pad {
            file,
            left,
            right,
            top,
            bottom,
            color,
            output_file: output,
        } => geometry1(
            ctx,
            &engine,
            file,
            "padded",
            output.as_deref(),
            &limits,
            |input| {
                engine.pad_video(
                    input,
                    *left,
                    *right,
                    *top,
                    *bottom,
                    color.as_deref(),
                    &limits,
                )
            },
        ),
        VideoArgs::Rotate {
            file,
            degrees,
            output_file: output,
        } => geometry1(
            ctx,
            &engine,
            file,
            "rotated",
            output.as_deref(),
            &limits,
            |input| engine.rotate_video(input, *degrees, &limits),
        ),
        VideoArgs::Flip {
            file,
            vertical,
            output_file: output,
        } => geometry1(
            ctx,
            &engine,
            file,
            "flipped",
            output.as_deref(),
            &limits,
            |input| engine.flip_video(input, !*vertical, &limits),
        ),
        VideoArgs::Letterbox {
            file,
            width,
            height,
            output_file: output,
        } => geometry1(
            ctx,
            &engine,
            file,
            "letterbox",
            output.as_deref(),
            &limits,
            |input| {
                if *width == 0 || *height == 0 {
                    return Err(VideoError::InvalidInput(
                        "width and height must be > 0".to_string(),
                    ));
                }
                engine.letterbox(input, *width, *height, &limits)
            },
        ),
        VideoArgs::Pillarbox {
            file,
            width,
            height,
            output_file: output,
        } => geometry1(
            ctx,
            &engine,
            file,
            "pillarbox",
            output.as_deref(),
            &limits,
            |input| {
                if *width == 0 || *height == 0 {
                    return Err(VideoError::InvalidInput(
                        "width and height must be > 0".to_string(),
                    ));
                }
                engine.pillarbox(input, *width, *height, &limits)
            },
        ),
        VideoArgs::Autocrop {
            file,
            output_file: output,
        } => geometry1(
            ctx,
            &engine,
            file,
            "autocrop",
            output.as_deref(),
            &limits,
            |input| engine.autocrop(input, &limits),
        ),
        VideoArgs::Transpose {
            file,
            counter_clockwise,
            output_file: output,
        } => geometry1(
            ctx,
            &engine,
            file,
            "transpose",
            output.as_deref(),
            &limits,
            |input| engine.transpose(input, !*counter_clockwise, &limits),
        ),
        VideoArgs::Orient {
            file,
            orientation,
            output_file: output,
        } => geometry1(
            ctx,
            &engine,
            file,
            "orient",
            output.as_deref(),
            &limits,
            |input| engine.orient(input, matches!(orientation, OrientArg::Landscape), &limits),
        ),

        // Tempo / time ops
        VideoArgs::Speed {
            file,
            factor,
            output_file: output,
        } => {
            if *factor <= 0.0 {
                return Err(CliError::Usage("--factor must be > 0".to_string()));
            }
            geometry1(
                ctx,
                &engine,
                file,
                "speed",
                output.as_deref(),
                &limits,
                |input| engine.set_speed(input, *factor, &limits),
            )
        }
        VideoArgs::Reverse {
            file,
            output_file: output,
        } => geometry1(
            ctx,
            &engine,
            file,
            "reversed",
            output.as_deref(),
            &limits,
            |input| engine.reverse(input, &limits),
        ),
        VideoArgs::Loop {
            file,
            count,
            output_file: output,
        } => geometry1(
            ctx,
            &engine,
            file,
            "looped",
            output.as_deref(),
            &limits,
            |input| engine.loop_clip(input, *count, &limits),
        ),
        VideoArgs::Freeze {
            file,
            at,
            duration,
            output_file: output,
        } => {
            if *at < 0.0 || *duration <= 0.0 {
                return Err(CliError::Usage(
                    "--at must be >= 0 and --duration > 0".to_string(),
                ));
            }
            geometry1(
                ctx,
                &engine,
                file,
                "freeze",
                output.as_deref(),
                &limits,
                |input| engine.freeze_frame(input, *at, *duration, &limits),
            )
        }

        // Audio processing
        VideoArgs::Volume {
            file,
            gain_db,
            output_file: output,
        } => geometry1(
            ctx,
            &engine,
            file,
            "volume",
            output.as_deref(),
            &limits,
            |input| engine.audio_volume(input, *gain_db, &limits),
        ),
        VideoArgs::Normalize {
            file,
            lufs,
            output_file: output,
        } => geometry1(
            ctx,
            &engine,
            file,
            "normalized",
            output.as_deref(),
            &limits,
            |input| engine.audio_normalize(input, *lufs, &limits),
        ),
        VideoArgs::AudioMerge {
            files,
            output_file: output,
        } => audio_merge(ctx, &engine, files, output.as_deref(), &limits),
        VideoArgs::AudioConcat {
            files,
            ext,
            output_file: output,
        } => audio_concat(ctx, &engine, files, ext, output.as_deref(), &limits),
        VideoArgs::Fade {
            file,
            fade_in,
            fade_out,
            output_file: output,
        } => {
            if *fade_in < 0.0 || *fade_out < 0.0 {
                return Err(CliError::Usage("--in/--out must be >= 0".to_string()));
            }
            geometry1(
                ctx,
                &engine,
                file,
                "fade",
                output.as_deref(),
                &limits,
                |input| engine.audio_fade(input, *fade_in, *fade_out, &limits),
            )
        }
        VideoArgs::RemoveAudio {
            file,
            output_file: output,
        } => geometry1(
            ctx,
            &engine,
            file,
            "noaudio",
            output.as_deref(),
            &limits,
            |input| engine.remove_audio(input, &limits),
        ),
        VideoArgs::Channels {
            file,
            count,
            output_file: output,
        } => {
            if *count == 0 {
                return Err(CliError::Usage("--count must be > 0".to_string()));
            }
            geometry1(
                ctx,
                &engine,
                file,
                "channels",
                output.as_deref(),
                &limits,
                |input| engine.channel_convert(input, *count, &limits),
            )
        }
        VideoArgs::Eq {
            file,
            bass,
            treble,
            mid,
            output_file: output,
        } => geometry1(
            ctx,
            &engine,
            file,
            "eq",
            output.as_deref(),
            &limits,
            |input| engine.audio_eq(input, *bass, *mid, *treble, &limits),
        ),
        VideoArgs::Denoise {
            file,
            strength,
            output_file: output,
        } => geometry1(
            ctx,
            &engine,
            file,
            "denoise",
            output.as_deref(),
            &limits,
            |input| engine.audio_denoise(input, *strength, &limits),
        ),
        VideoArgs::CompressAudio {
            file,
            threshold,
            ratio,
            output_file: output,
        } => {
            if *ratio <= 0.0 {
                return Err(CliError::Usage("--ratio must be > 0".to_string()));
            }
            geometry1(
                ctx,
                &engine,
                file,
                "compress",
                output.as_deref(),
                &limits,
                |input| engine.audio_compress(input, *threshold, *ratio, &limits),
            )
        }
        VideoArgs::Pitch {
            file,
            semitones,
            output_file: output,
        } => geometry1(
            ctx,
            &engine,
            file,
            "pitch",
            output.as_deref(),
            &limits,
            |input| engine.audio_pitch(input, *semitones, &limits),
        ),
        VideoArgs::Spectrogram {
            file,
            width,
            height,
            output_file,
        } => {
            let input = read_input(file)?;
            let bytes = engine
                .audio_spectrogram(&input, width.unwrap_or(800), height.unwrap_or(400), &limits)
                .map_err(map_video_error)?;
            let out = output_file.clone();
            check_output(&out, &[file])?;
            write_output(&out, &bytes)?;
            report(ctx, file, &out, "spectrogram", bytes.len())
        }
        VideoArgs::Mix {
            files,
            weights,
            no_normalize,
            output_file: output,
        } => audio_mix(
            ctx,
            &engine,
            files,
            weights,
            *no_normalize,
            output.as_deref(),
            &limits,
        ),
        VideoArgs::Crossfade {
            files,
            duration,
            output_file: output,
        } => audio_crossfade(ctx, &engine, files, *duration, output.as_deref(), &limits),
        VideoArgs::Delay {
            file,
            ms,
            output_file: output,
        } => geometry1(
            ctx,
            &engine,
            file,
            "delay",
            output.as_deref(),
            &limits,
            |input| engine.audio_delay(input, *ms, &limits),
        ),

        // Frames / images
        VideoArgs::Frames {
            file,
            every,
            fps,
            all,
            keyframes,
            format,
            output_dir,
        } => frames(
            ctx,
            &engine,
            file,
            *every,
            *fps,
            *all,
            *keyframes,
            format,
            output_dir.as_deref(),
            &limits,
        ),
        VideoArgs::ImageSequence {
            file,
            format,
            output_dir,
        } => image_sequence(ctx, &engine, file, format, output_dir.as_deref(), &limits),
        VideoArgs::ContactSheet {
            file,
            cols,
            rows,
            thumb_width,
            interval,
            output_file: output,
        } => contact_sheet(
            ctx,
            &engine,
            file,
            *cols,
            *rows,
            *thumb_width,
            *interval,
            output.as_deref(),
            &limits,
        ),
        VideoArgs::SceneFrames {
            file,
            threshold,
            output_dir,
        } => scene_frames(
            ctx,
            &engine,
            file,
            *threshold,
            output_dir.as_deref(),
            &limits,
        ),
        VideoArgs::Slideshow {
            files,
            fps,
            duration,
            ext,
            output_file: output,
        } => slideshow(
            ctx,
            &engine,
            files,
            *fps,
            *duration,
            ext,
            output.as_deref(),
            &limits,
        ),
        VideoArgs::ToGif {
            file,
            fps,
            width,
            output_file: output,
        } => to_gif(ctx, &engine, file, *fps, *width, output.as_deref(), &limits),
        VideoArgs::ToWebp {
            file,
            fps,
            output_file: output,
        } => to_animation(ctx, &engine, file, *fps, "webp", output.as_deref(), &limits),
        VideoArgs::ToApng {
            file,
            fps,
            output_file: output,
        } => to_animation(ctx, &engine, file, *fps, "apng", output.as_deref(), &limits),
        VideoArgs::AudioToVideo {
            file,
            style,
            width,
            height,
            output_file: output,
        } => audio_to_video(
            ctx,
            &engine,
            file,
            *style,
            *width,
            *height,
            output.as_deref(),
            &limits,
        ),
        VideoArgs::GifToVideo {
            file,
            ext,
            output_file: output,
        } => gif_to_video(ctx, &engine, file, ext, output.as_deref(), &limits),
        VideoArgs::ReplaceAudio {
            video,
            audio,
            output_file: output,
        } => replace_audio(ctx, &engine, video, audio, output.as_deref(), &limits),

        // Extraction / subtitles
        VideoArgs::ExtractStreams {
            file,
            subtitles,
            attachments,
            cover,
            chapters,
            data,
            subtitle_format,
            output_dir,
        } => extract_streams(
            ctx,
            &engine,
            file,
            *subtitles,
            *attachments,
            *cover,
            *chapters,
            *data,
            subtitle_format,
            output_dir.as_deref(),
            &limits,
        ),
        VideoArgs::Subtitles {
            file,
            track,
            format,
            output_file: output,
        } => subtitles(
            ctx,
            &engine,
            file,
            *track,
            format,
            output.as_deref(),
            &limits,
        ),
        VideoArgs::SubsConvert {
            file,
            from,
            to,
            output_file: output,
        } => subs_convert(
            ctx,
            &engine,
            file,
            from.as_deref(),
            to,
            output.as_deref(),
            &limits,
        ),
        VideoArgs::SubsShift {
            file,
            offset,
            format,
            output_file: output,
        } => subs_shift(
            ctx,
            &engine,
            file,
            *offset,
            format,
            output.as_deref(),
            &limits,
        ),
        VideoArgs::BurnSubs {
            file,
            subs_file,
            style,
            output_file: output,
        } => burn_subs(
            ctx,
            &engine,
            file,
            subs_file,
            style.as_deref(),
            output.as_deref(),
            &limits,
        ),

        // Filters / overlays / stream ops
        VideoArgs::Filter {
            file,
            vf,
            af,
            output_file: output,
        } => filter(
            ctx,
            &engine,
            file,
            vf.as_deref(),
            af.as_deref(),
            output.as_deref(),
            &limits,
        ),
        VideoArgs::Filters => filters(ctx, &engine),
        VideoArgs::Overlay {
            file,
            image,
            x,
            y,
            opacity,
            output_file: output,
        } => overlay(
            ctx,
            &engine,
            file,
            image,
            *x,
            *y,
            *opacity,
            output.as_deref(),
            &limits,
        ),
        VideoArgs::Text {
            file,
            text,
            x,
            y,
            size,
            color,
            output_file: output,
        } => draw_text(
            ctx,
            &engine,
            file,
            text,
            *x,
            *y,
            *size,
            color.as_deref(),
            output.as_deref(),
            &limits,
        ),
        VideoArgs::ChromaKey {
            foreground,
            background,
            color,
            similarity,
            blend,
            output_file: output,
        } => chroma_key(
            ctx,
            &engine,
            foreground,
            background,
            color,
            *similarity,
            *blend,
            output.as_deref(),
            &limits,
        ),
        VideoArgs::SelectStreams {
            file,
            ext,
            no_video,
            audio,
            subs,
            output_file: output,
        } => select_streams(
            ctx,
            &engine,
            file,
            ext,
            *no_video,
            audio.as_deref(),
            subs.as_deref(),
            output.as_deref(),
            &limits,
        ),
        VideoArgs::MergeAv {
            video,
            audio,
            ext,
            output_file: output,
        } => merge_av(
            ctx,
            &engine,
            video,
            audio,
            ext.as_deref(),
            output.as_deref(),
            &limits,
        ),
        VideoArgs::SetMetadata {
            file,
            tags,
            output_file: output,
        } => set_metadata(ctx, &engine, file, tags, output.as_deref(), &limits),
        VideoArgs::StripMetadata {
            file,
            ext,
            output_file: output,
        } => strip_metadata(
            ctx,
            &engine,
            file,
            ext.as_deref(),
            output.as_deref(),
            &limits,
        ),
        VideoArgs::CoverArt {
            file,
            image,
            output_file: output,
        } => cover_art(ctx, &engine, file, image, output.as_deref(), &limits),

        // Analysis / loudness / quality
        VideoArgs::Analyze { file } => analyze(ctx, &engine, file, &limits),
        VideoArgs::Scenes { file, threshold } => scenes(ctx, &engine, file, *threshold, &limits),
        VideoArgs::BlackFrames { file } => {
            intervals(ctx, &engine, file, "black-frames", &limits, |input| {
                engine.detect_black_frames(input, &limits)
            })
        }
        VideoArgs::FreezeFrames { file } => {
            intervals(ctx, &engine, file, "freeze-frames", &limits, |input| {
                engine.detect_freeze_frames(input, &limits)
            })
        }
        VideoArgs::Silences { file } => {
            intervals(ctx, &engine, file, "silences", &limits, |input| {
                engine.detect_silence(input, &limits)
            })
        }
        VideoArgs::Loudness { file } => loudness(ctx, &engine, file, &limits),
        VideoArgs::NormalizeLufs {
            file,
            lufs,
            output_file: output,
        } => geometry1(
            ctx,
            &engine,
            file,
            "loudnorm",
            output.as_deref(),
            &limits,
            |input| engine.normalize_loudness(input, *lufs, &limits),
        ),
        VideoArgs::AudioStats { file } => audio_stats(ctx, &engine, file, &limits),
        VideoArgs::Quality {
            reference,
            distorted,
            metrics,
        } => quality(
            ctx,
            &engine,
            reference,
            distorted,
            metrics.as_deref(),
            &limits,
        ),

        // Packaging
        VideoArgs::Hls { file, output_dir } => package(
            ctx,
            &engine,
            file,
            "hls",
            output_dir.as_deref(),
            &limits,
            |e, input, l| e.package_hls(input, l),
        ),
        VideoArgs::Dash { file, output_dir } => package(
            ctx,
            &engine,
            file,
            "dash",
            output_dir.as_deref(),
            &limits,
            |e, input, l| e.package_dash(input, l),
        ),
        VideoArgs::Faststart {
            file,
            output_file: output,
        } => {
            let input = read_input(file)?;
            let bytes = engine.faststart(&input, &limits).map_err(map_video_error)?;
            let out = pick_out(output.as_deref(), marked_default(file, "faststart", "mp4"))?;
            check_output(&out, &[file])?;
            write_output(&out, &bytes)?;
            report(ctx, file, &out, "faststart", bytes.len())
        }

        // Runtime inspection
        VideoArgs::Devices => device_list(ctx, &engine.list_devices().map_err(map_video_error)?),
        VideoArgs::Protocols => {
            device_list(ctx, &engine.list_protocols().map_err(map_video_error)?)
        }
        VideoArgs::Hwaccels => device_list(ctx, &engine.list_hwaccels().map_err(map_video_error)?),
        VideoArgs::Capabilities => capabilities(ctx, &engine),
        VideoArgs::Versions => versions(ctx, &engine),
    }
}

// ---------------------------------------------------------------------------
// Shared single-file byte-op helper
// ---------------------------------------------------------------------------

/// Read `file`, run `op` on the bytes, write the result to
/// `--output-file` or `<stem>.<marker><same-ext>` next to the input.
/// `op_name` is the operation tag used in the write report.
#[allow(clippy::too_many_arguments)]
fn geometry1(
    ctx: &Context,
    _engine: &VideoEngine,
    file: &Path,
    marker: &str,
    output: Option<&Path>,
    _limits: &EngineLimits,
    op: impl FnOnce(&[u8]) -> crate::crates::video_engine::VideoResult<Vec<u8>>,
) -> Result<i32, CliError> {
    let input = read_input(file)?;
    let bytes = op(&input).map_err(map_video_error)?;
    let out = pick_out(output, marked_same_ext(file, marker))?;
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, marker_to_op(marker), bytes.len())
}

fn marker_to_op(marker: &str) -> &'static str {
    // Map internal markers to stable operation names for reports.
    match marker {
        "resized" => "resize",
        "cropped" => "crop",
        "padded" => "pad",
        "rotated" => "rotate",
        "flipped" => "flip",
        "letterbox" => "letterbox",
        "pillarbox" => "pillarbox",
        "autocrop" => "autocrop",
        "transpose" => "transpose",
        "orient" => "orient",
        "speed" => "speed",
        "reversed" => "reverse",
        "looped" => "loop",
        "freeze" => "freeze",
        "volume" => "volume",
        "normalized" => "normalize",
        "fade" => "fade",
        "noaudio" => "remove-audio",
        "channels" => "channels",
        "eq" => "eq",
        "denoise" => "denoise",
        "compress" => "compress-audio",
        "pitch" => "pitch",
        "delay" => "delay",
        "loudnorm" => "normalize-lufs",
        _ => "video-op",
    }
}

fn fit_mode(fit: FitArg) -> FitMode {
    match fit {
        FitArg::Fit => FitMode::Fit,
        FitArg::Cover => FitMode::Cover,
        FitArg::Contain => FitMode::Contain,
        FitArg::Stretch => FitMode::Stretch,
    }
}

// ---------------------------------------------------------------------------
// Existing commands
// ---------------------------------------------------------------------------

fn probe(ctx: &Context, engine: &VideoEngine, file: &Path) -> Result<i32, CliError> {
    let input = read_input(file)?;
    let info = engine.probe(&input).map_err(map_video_error)?;

    if ctx.emit_structured(&info)? {
        return Ok(0);
    }
    println!("file: {}", file.display());
    let v = serde_json::to_value(&info)?;
    for key in [
        "duration_secs",
        "width",
        "height",
        "fps",
        "vcodec",
        "acodec",
        "format_name",
        "format_long_name",
        "bit_rate",
        "size_bytes",
        "has_audio",
        "has_video",
    ] {
        if let Some(val) = v.get(key) {
            if !val.is_null() {
                println!("{key}: {}", render_value(val));
            }
        }
    }
    println!("streams: {}", info.streams.len());
    println!("chapters: {}", info.chapters.len());
    Ok(0)
}

#[derive(Debug, Serialize)]
struct WriteReport {
    input: String,
    output: String,
    operation: &'static str,
    bytes_written: u64,
}

fn thumbnail(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    at: Option<f64>,
    output: Option<&Path>,
) -> Result<i32, CliError> {
    let input = read_input(file)?;
    let bytes = engine
        .thumbnail(&input, at, &EngineLimits::default())
        .map_err(map_video_error)?;

    let out = pick_out(output, marked_default(file, "thumb", "png"))?;
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "thumbnail", bytes.len())
}

fn trim(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    start: f64,
    end: Option<f64>,
    reencode: bool,
    output: Option<&Path>,
) -> Result<i32, CliError> {
    if start < 0.0 {
        return Err(CliError::Usage("--start must be >= 0".to_string()));
    }
    if let Some(end) = end {
        if end <= start {
            return Err(CliError::Usage(
                "--end must be greater than --start".to_string(),
            ));
        }
    }
    let input = read_input(file)?;
    let bytes = engine
        .trim(&input, start, end, reencode, &EngineLimits::default())
        .map_err(map_video_error)?;

    let out = pick_out(output, trimmed_default(file))?;
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "trim", bytes.len())
}

fn transcode(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    target_ext: &str,
    output: Option<&Path>,
) -> Result<i32, CliError> {
    let ext = sanitize_extension(target_ext)?;
    let input = read_input(file)?;
    let bytes = engine
        .transcode(&input, &ext, &EngineLimits::default())
        .map_err(map_video_error)?;

    let out = pick_out(output, Ok(file.with_extension(&ext)))?;
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "transcode", bytes.len())
}

fn concat(
    ctx: &Context,
    engine: &VideoEngine,
    files: &[PathBuf],
    target_ext: &str,
    output: Option<&Path>,
) -> Result<i32, CliError> {
    if files.len() < 2 {
        return Err(CliError::Usage(
            "concat requires at least two input files".to_string(),
        ));
    }
    let ext = sanitize_extension(target_ext)?;
    let mut inputs = Vec::with_capacity(files.len());
    for f in files {
        inputs.push(read_input(f)?);
    }
    let bytes = engine
        .concat(&inputs, &ext, &EngineLimits::default())
        .map_err(map_video_error)?;

    let out = pick_out(output, Ok(files[0].with_file_name(format!("concat.{ext}"))))?;
    check_output_multi(&out, files)?;
    write_output(&out, &bytes)?;
    report(ctx, &files[0], &out, "concat", bytes.len())
}

fn extract_audio(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    ext: &str,
    output: Option<&Path>,
) -> Result<i32, CliError> {
    let normalized = sanitize_extension(ext)?;
    let input = read_input(file)?;
    let bytes = engine
        .extract_audio(&input, &normalized, &EngineLimits::default())
        .map_err(map_video_error)?;

    let out = pick_out(output, Ok(file.with_extension(&normalized)))?;
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "extract-audio", bytes.len())
}

// ---------------------------------------------------------------------------
// Containers / remux / encode profiles
// ---------------------------------------------------------------------------

fn remux(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    target_ext: &str,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let ext = sanitize_extension(target_ext)?;
    let input = read_input(file)?;
    let bytes = engine
        .remux(&input, &ext, limits)
        .map_err(map_video_error)?;
    let out = pick_out(output, Ok(file.with_extension(&ext)))?;
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "remux", bytes.len())
}

fn encode(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    profile: &str,
    ext: Option<&str>,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let presets = engine.encode_presets();
    let profile = presets.get(profile).cloned().ok_or_else(|| {
        CliError::Usage(format!(
            "unknown encode preset {profile:?}; run `kaladoc video presets` to list them"
        ))
    })?;
    let ext = match ext {
        Some(e) => sanitize_extension(e)?,
        None => "mp4".to_string(),
    };
    let input = read_input(file)?;
    let bytes = engine
        .transcode_profile(&input, &profile, &ext, limits)
        .map_err(map_video_error)?;
    let out = pick_out(output, Ok(file.with_extension(&ext)))?;
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "encode", bytes.len())
}

fn presets(ctx: &Context, engine: &VideoEngine) -> Result<i32, CliError> {
    let presets: Vec<serde_json::Value> = engine
        .encode_presets()
        .iter()
        .map(|(name, p)| {
            serde_json::json!({
                "name": name,
                "vcodec": p.vcodec,
                "acodec": p.acodec,
                "crf": p.crf,
                "preset": p.preset,
                "video_bitrate": p.video_bitrate,
                "audio_bitrate": p.audio_bitrate,
                "pix_fmt": p.pix_fmt,
            })
        })
        .collect();
    if ctx.emit_structured(&presets)? {
        return Ok(0);
    }
    for p in &presets {
        println!(
            "{} ({} / {})",
            p["name"].as_str().unwrap_or("?"),
            p["vcodec"].as_str().unwrap_or("?"),
            p["acodec"].as_str().unwrap_or("?")
        );
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// Audio multi-input ops
// ---------------------------------------------------------------------------

fn read_inputs(files: &[PathBuf]) -> Result<Vec<Vec<u8>>, CliError> {
    let mut inputs = Vec::with_capacity(files.len());
    for f in files {
        inputs.push(read_input(f)?);
    }
    Ok(inputs)
}

fn audio_merge(
    ctx: &Context,
    engine: &VideoEngine,
    files: &[PathBuf],
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    require_multi(files, "audio-merge")?;
    let inputs = read_inputs(files)?;
    let bytes = engine
        .audio_merge(&inputs, limits)
        .map_err(map_video_error)?;
    let out = pick_out(output, marked_same_ext(&files[0], "merged"))?;
    check_output_multi(&out, files)?;
    write_output(&out, &bytes)?;
    report(ctx, &files[0], &out, "audio-merge", bytes.len())
}

fn audio_concat(
    ctx: &Context,
    engine: &VideoEngine,
    files: &[PathBuf],
    ext: &str,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    require_multi(files, "audio-concat")?;
    let normalized = sanitize_extension(ext)?;
    let inputs = read_inputs(files)?;
    let bytes = engine
        .audio_concat(&inputs, &normalized, limits)
        .map_err(map_video_error)?;
    let out = pick_out(
        output,
        Ok(files[0].with_file_name(format!("concat.{normalized}"))),
    )?;
    check_output_multi(&out, files)?;
    write_output(&out, &bytes)?;
    report(ctx, &files[0], &out, "audio-concat", bytes.len())
}

fn audio_mix(
    ctx: &Context,
    engine: &VideoEngine,
    files: &[PathBuf],
    weights: &str,
    no_normalize: bool,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    require_multi(files, "mix")?;
    let weights = parse_f64_list(weights, "--weights")?;
    if weights.len() != files.len() {
        return Err(CliError::Usage(format!(
            "--weights has {} entries but {} inputs were given",
            weights.len(),
            files.len()
        )));
    }
    let inputs = read_inputs(files)?;
    let bytes = engine
        .audio_mix_weighted(&inputs, &weights, !no_normalize, limits)
        .map_err(map_video_error)?;
    let out = pick_out(output, marked_same_ext(&files[0], "mix"))?;
    check_output_multi(&out, files)?;
    write_output(&out, &bytes)?;
    report(ctx, &files[0], &out, "mix", bytes.len())
}

fn audio_crossfade(
    ctx: &Context,
    engine: &VideoEngine,
    files: &[PathBuf],
    duration: f64,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    require_multi(files, "crossfade")?;
    if duration <= 0.0 {
        return Err(CliError::Usage("--duration must be > 0".to_string()));
    }
    let inputs = read_inputs(files)?;
    let bytes = engine
        .audio_crossfade(&inputs, duration, limits)
        .map_err(map_video_error)?;
    let out = pick_out(output, marked_same_ext(&files[0], "crossfade"))?;
    check_output_multi(&out, files)?;
    write_output(&out, &bytes)?;
    report(ctx, &files[0], &out, "crossfade", bytes.len())
}

// ---------------------------------------------------------------------------
// Frames / images / slideshow
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn frames(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    every: Option<f64>,
    fps: Option<f64>,
    all: bool,
    keyframes: bool,
    format: &str,
    output_dir: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let selectors = [every.is_some(), fps.is_some(), all, keyframes]
        .iter()
        .filter(|b| **b)
        .count();
    if selectors != 1 {
        return Err(CliError::Usage(
            "frames requires exactly one of --every, --fps, --all, --keyframes".to_string(),
        ));
    }
    if let Some(e) = every {
        if e <= 0.0 {
            return Err(CliError::Usage("--every must be > 0".to_string()));
        }
    }
    if let Some(f) = fps {
        if f <= 0.0 {
            return Err(CliError::Usage("--fps must be > 0".to_string()));
        }
    }
    let spec = FramesSpec {
        all,
        every_secs: every,
        fps,
        keyframes_only: keyframes,
        format: sanitize_extension(format)?,
    };
    let input = read_input(file)?;
    let out = engine
        .extract_frames(&input, &spec, limits)
        .map_err(map_video_error)?;
    write_media_output(ctx, file, output_dir, "frames", out, "frames")
}

fn image_sequence(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    format: &str,
    output_dir: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let format = sanitize_extension(format)?;
    let input = read_input(file)?;
    let out = engine
        .video_to_image_sequence(&input, &format, limits)
        .map_err(map_video_error)?;
    write_media_output(ctx, file, output_dir, "seq", out, "image-sequence")
}

fn contact_sheet(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    cols: u32,
    rows: u32,
    thumb_width: Option<u32>,
    interval: Option<f64>,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    if cols == 0 || rows == 0 {
        return Err(CliError::Usage("--cols and --rows must be > 0".to_string()));
    }
    let input = read_input(file)?;
    let bytes = engine
        .contact_sheet(
            &input,
            cols,
            rows,
            thumb_width.unwrap_or(160),
            interval,
            limits,
        )
        .map_err(map_video_error)?;
    let out = pick_out(output, marked_default(file, "sheet", "png"))?;
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "contact-sheet", bytes.len())
}

fn scene_frames(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    threshold: Option<f64>,
    output_dir: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let input = read_input(file)?;
    let out = engine
        .scene_keyframes(&input, threshold.unwrap_or(0.3), limits)
        .map_err(map_video_error)?;
    write_media_output(ctx, file, output_dir, "scenes", out, "scene-frames")
}

fn slideshow(
    ctx: &Context,
    engine: &VideoEngine,
    files: &[PathBuf],
    fps: f64,
    duration: Option<f64>,
    ext: &str,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    if files.is_empty() {
        return Err(CliError::Usage(
            "slideshow requires at least one image".to_string(),
        ));
    }
    if fps <= 0.0 {
        return Err(CliError::Usage("--fps must be > 0".to_string()));
    }
    let ext = sanitize_extension(ext)?;
    let inputs = read_inputs(files)?;
    let bytes = engine
        .images_to_video(&inputs, fps, duration, &ext, limits)
        .map_err(map_video_error)?;
    let out = pick_out(
        output,
        Ok(files[0].with_file_name(format!("slideshow.{ext}"))),
    )?;
    check_output_multi(&out, files)?;
    write_output(&out, &bytes)?;
    report(ctx, &files[0], &out, "slideshow", bytes.len())
}

fn to_gif(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    fps: Option<f64>,
    width: Option<u32>,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let input = read_input(file)?;
    let bytes = engine
        .video_to_gif(&input, fps, width, limits)
        .map_err(map_video_error)?;
    let out = pick_out(output, Ok(file.with_extension("gif")))?;
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "to-gif", bytes.len())
}

fn to_animation(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    fps: Option<f64>,
    kind: &str,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let input = read_input(file)?;
    let bytes = match kind {
        "webp" => engine.video_to_animated_webp(&input, fps, limits),
        "apng" => engine.video_to_apng(&input, fps, limits),
        _ => unreachable!(),
    }
    .map_err(map_video_error)?;
    let op = if kind == "webp" { "to-webp" } else { "to-apng" };
    let out = pick_out(output, Ok(file.with_extension(kind)))?;
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, op, bytes.len())
}

fn audio_to_video(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    style: VisualStyleArg,
    width: u32,
    height: u32,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    if width == 0 || height == 0 {
        return Err(CliError::Usage("--width/--height must be > 0".to_string()));
    }
    let style = match style {
        VisualStyleArg::Waveform => VisualStyle::Waveform,
        VisualStyleArg::Spectrum => VisualStyle::Spectrum,
        VisualStyleArg::Both => VisualStyle::Both,
    };
    let input = read_input(file)?;
    let bytes = engine
        .audio_to_video(&input, style, width, height, limits)
        .map_err(map_video_error)?;
    let out = pick_out(output, marked_default(file, "audiovid", "mp4"))?;
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "audio-to-video", bytes.len())
}

fn gif_to_video(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    ext: &str,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let ext = sanitize_extension(ext)?;
    let input = read_input(file)?;
    let bytes = engine
        .gif_to_video(&input, &ext, limits)
        .map_err(map_video_error)?;
    let out = pick_out(output, Ok(file.with_extension(&ext)))?;
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "gif-to-video", bytes.len())
}

fn replace_audio(
    ctx: &Context,
    engine: &VideoEngine,
    video: &Path,
    audio: &Path,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let video_bytes = read_input(video)?;
    let audio_bytes = read_input(audio)?;
    let bytes = engine
        .replace_audio_track(&video_bytes, &audio_bytes, limits)
        .map_err(map_video_error)?;
    let out = pick_out(output, marked_same_ext(video, "replaced"))?;
    check_output_multi(&out, &[video.to_path_buf(), audio.to_path_buf()])?;
    write_output(&out, &bytes)?;
    report(ctx, video, &out, "replace-audio", bytes.len())
}

// ---------------------------------------------------------------------------
// Extraction / subtitles
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn extract_streams(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    subtitles: bool,
    attachments: bool,
    cover: bool,
    chapters: bool,
    data: bool,
    subtitle_format: &str,
    output_dir: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    if !(subtitles || attachments || cover || chapters || data) {
        return Err(CliError::Usage(
            "extract-streams needs at least one of --subtitles, --attachments, --cover, --chapters, --data"
                .to_string(),
        ));
    }
    let selection = StreamSelection {
        subtitles,
        attachments,
        cover_art: cover,
        chapters,
        data,
    };
    let format = sanitize_extension(subtitle_format)?;
    let input = read_input(file)?;
    let out = engine
        .extract_streams(&input, &selection, &format, limits)
        .map_err(map_video_error)?;
    write_media_output(ctx, file, output_dir, "streams", out, "extract-streams")
}

fn subtitles(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    track: Option<u32>,
    format: &str,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let format = sanitize_extension(format)?;
    let input = read_input(file)?;
    let bytes = engine
        .extract_subtitles(&input, track, &format, limits)
        .map_err(map_video_error)?;
    let out = pick_out(output, marked_default(file, "subs", &format))?;
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "subtitles", bytes.len())
}

fn subs_convert(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    from: Option<&str>,
    to: &str,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let to = sanitize_extension(to)?;
    let bytes_in = read_input(file)?;
    let bytes = engine
        .convert_subtitles(&bytes_in, from, &to, limits)
        .map_err(map_video_error)?;
    let out = pick_out(output, Ok(file.with_extension(&to)))?;
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "subs-convert", bytes.len())
}

fn subs_shift(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    offset: f64,
    format: &str,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let format = sanitize_extension(format)?;
    let bytes_in = read_input(file)?;
    let bytes = engine
        .shift_subtitles(&bytes_in, offset, &format, limits)
        .map_err(map_video_error)?;
    let out = pick_out(output, marked_default(file, "shifted", &format))?;
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "subs-shift", bytes.len())
}

/// JSON shape accepted by `--style` on `burn-subs`. Mirrors
/// [`SubtitleStyle`]; `SubtitleStyle` itself is not `Deserialize`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct StyleJson {
    font_name: Option<String>,
    font_size: Option<u32>,
    primary_color: Option<String>,
    outline_color: Option<String>,
    outline_width: Option<u32>,
    shadow_offset: Option<u32>,
    margin_v: Option<u32>,
    alignment: Option<u8>,
}

fn burn_subs(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    subs_file: &Path,
    style: Option<&str>,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let style_parsed: Option<SubtitleStyle> = match style {
        Some(s) => {
            let j: StyleJson = serde_json::from_str(s)?;
            Some(SubtitleStyle {
                font_name: j.font_name,
                font_size: j.font_size,
                primary_color: j.primary_color,
                outline_color: j.outline_color,
                outline_width: j.outline_width,
                shadow_offset: j.shadow_offset,
                margin_v: j.margin_v,
                alignment: j.alignment,
            })
        }
        None => None,
    };
    let input = read_input(file)?;
    let subs = read_input(subs_file)?;
    let bytes = engine
        .burn_subtitles(&input, &subs, style_parsed.as_ref(), limits)
        .map_err(map_video_error)?;
    let out = pick_out(output, marked_same_ext(file, "burned"))?;
    check_output_multi(&out, &[file.to_path_buf(), subs_file.to_path_buf()])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "burn-subs", bytes.len())
}

// ---------------------------------------------------------------------------
// Filters / overlays / stream ops
// ---------------------------------------------------------------------------

fn filter(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    vf: Option<&str>,
    af: Option<&str>,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let vf_empty = vf.map(str::trim).unwrap_or("").is_empty();
    let af_empty = af.map(str::trim).unwrap_or("").is_empty();
    if vf_empty && af_empty {
        return Err(CliError::Usage(
            "filter requires at least one of --vf or --af".to_string(),
        ));
    }
    let input = read_input(file)?;
    let bytes = engine
        .apply_filters(&input, vf, af, limits)
        .map_err(map_video_error)?;
    let out = pick_out(output, marked_same_ext(file, "filtered"))?;
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "filter", bytes.len())
}

fn filters(ctx: &Context, engine: &VideoEngine) -> Result<i32, CliError> {
    let presets: Vec<serde_json::Value> = engine
        .filter_presets()
        .iter()
        .map(|p| {
            serde_json::json!({
                "name": p.name,
                "video_filter": p.video_filter,
                "audio_filter": p.audio_filter,
                "description": p.description,
            })
        })
        .collect();
    if ctx.emit_structured(&presets)? {
        return Ok(0);
    }
    for p in &presets {
        println!(
            "{}\t{}\t{}",
            p["name"].as_str().unwrap_or("?"),
            p["video_filter"].as_str().unwrap_or(""),
            p["audio_filter"].as_str().unwrap_or("")
        );
    }
    Ok(0)
}

#[allow(clippy::too_many_arguments)]
fn overlay(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    image: &Path,
    x: i32,
    y: i32,
    opacity: Option<f64>,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let input = read_input(file)?;
    let overlay_bytes = read_input(image)?;
    let bytes = engine
        .overlay_image(&input, &overlay_bytes, x, y, opacity, limits)
        .map_err(map_video_error)?;
    let out = pick_out(output, marked_same_ext(file, "overlay"))?;
    check_output_multi(&out, &[file.to_path_buf(), image.to_path_buf()])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "overlay", bytes.len())
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    text: &str,
    x: i32,
    y: i32,
    size: Option<u32>,
    color: Option<&str>,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let input = read_input(file)?;
    let bytes = engine
        .draw_text(&input, text, x, y, size.unwrap_or(24), color, limits)
        .map_err(map_video_error)?;
    let out = pick_out(output, marked_same_ext(file, "text"))?;
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "text", bytes.len())
}

#[allow(clippy::too_many_arguments)]
fn chroma_key(
    ctx: &Context,
    engine: &VideoEngine,
    foreground: &Path,
    background: &Path,
    color: &str,
    similarity: Option<f64>,
    blend: Option<f64>,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let fg = read_input(foreground)?;
    let bg = read_input(background)?;
    let bytes = engine
        .chroma_key(
            &fg,
            &bg,
            color,
            similarity.unwrap_or(0.3),
            blend.unwrap_or(0.1),
            limits,
        )
        .map_err(map_video_error)?;
    let out = pick_out(output, marked_same_ext(foreground, "keyed"))?;
    check_output_multi(&out, &[foreground.to_path_buf(), background.to_path_buf()])?;
    write_output(&out, &bytes)?;
    report(ctx, foreground, &out, "chroma-key", bytes.len())
}

fn select_streams(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    ext: &str,
    no_video: bool,
    audio: Option<&str>,
    subs: Option<&str>,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let ext = sanitize_extension(ext)?;
    let filter = StreamFilter {
        keep_video: !no_video,
        audio_ordinals: parse_csv_u32(audio)?,
        subtitle_ordinals: parse_csv_u32(subs)?,
        drop_attachments: false,
    };
    let input = read_input(file)?;
    let bytes = engine
        .select_streams(&input, &filter, &ext, limits)
        .map_err(map_video_error)?;
    let out = pick_out(output, Ok(file.with_extension(&ext)))?;
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "select-streams", bytes.len())
}

fn merge_av(
    ctx: &Context,
    engine: &VideoEngine,
    video: &Path,
    audio: &Path,
    ext: Option<&str>,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let ext = match ext {
        Some(e) => sanitize_extension(e)?,
        None => video
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mp4")
            .to_string(),
    };
    let video_bytes = read_input(video)?;
    let audio_bytes = read_input(audio)?;
    let bytes = engine
        .merge_av(&video_bytes, &audio_bytes, &ext, limits)
        .map_err(map_video_error)?;
    let out = pick_out(output, marked_default(video, "merged", &ext))?;
    check_output_multi(&out, &[video.to_path_buf(), audio.to_path_buf()])?;
    write_output(&out, &bytes)?;
    report(ctx, video, &out, "merge-av", bytes.len())
}

fn set_metadata(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    tags: &[String],
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let mut map = BTreeMap::new();
    for tag in tags {
        let (k, v) = tag
            .split_once('=')
            .ok_or_else(|| CliError::Usage(format!("--tag must be key=value, got {tag:?}")))?;
        let key = k.trim();
        if key.is_empty() {
            return Err(CliError::Usage(format!("empty tag key in {tag:?}")));
        }
        map.insert(key.to_string(), v.to_string());
    }
    let input = read_input(file)?;
    let bytes = engine
        .set_metadata(&input, &map, limits)
        .map_err(map_video_error)?;
    let out = pick_out(output, marked_same_ext(file, "tagged"))?;
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "set-metadata", bytes.len())
}

fn strip_metadata(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    ext: Option<&str>,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let ext = match ext {
        Some(e) => sanitize_extension(e)?,
        None => file
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mp4")
            .to_string(),
    };
    let input = read_input(file)?;
    let bytes = engine
        .strip_metadata(&input, &ext, limits)
        .map_err(map_video_error)?;
    let out = pick_out(output, marked_default(file, "stripped", &ext))?;
    check_output(&out, &[file])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "strip-metadata", bytes.len())
}

fn cover_art(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    image: &Path,
    output: Option<&Path>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let input = read_input(file)?;
    let cover = read_input(image)?;
    let bytes = engine
        .set_cover_art(&input, &cover, limits)
        .map_err(map_video_error)?;
    let out = pick_out(output, marked_same_ext(file, "cover"))?;
    check_output_multi(&out, &[file.to_path_buf(), image.to_path_buf()])?;
    write_output(&out, &bytes)?;
    report(ctx, file, &out, "cover-art", bytes.len())
}

// ---------------------------------------------------------------------------
// Analysis / loudness / quality
// ---------------------------------------------------------------------------

fn analyze(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let input = read_input(file)?;
    let report_data = engine.analyze(&input, limits).map_err(map_video_error)?;
    if ctx.emit_structured(&report_data)? {
        return Ok(0);
    }
    let v = serde_json::to_value(&report_data)?;
    let count = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_array())
            .map(|a| a.len())
            .unwrap_or(0)
    };
    println!("black_segments: {}", count("black_segments"));
    println!("freeze_segments: {}", count("freeze_segments"));
    println!("silence_segments: {}", count("silence_segments"));
    println!("scenes: {}", count("scenes"));
    println!("metrics: {}", count("metrics"));
    if let Some(lufs) = v.pointer("/loudness/integrated_lufs") {
        println!("integrated_lufs: {}", render_value(lufs));
    }
    Ok(0)
}

fn scenes(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    threshold: Option<f64>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let input = read_input(file)?;
    let scenes = engine
        .scene_timestamps(&input, threshold.unwrap_or(0.3), limits)
        .map_err(map_video_error)?;
    let items: Vec<serde_json::Value> = scenes
        .iter()
        .map(|(t, s)| serde_json::json!({ "time": t, "score": s }))
        .collect();
    if ctx.emit_structured(&items)? {
        return Ok(0);
    }
    for item in &items {
        println!(
            "{}\t{}",
            item["time"].as_f64().unwrap_or(0.0),
            item["score"].as_f64().unwrap_or(0.0)
        );
    }
    Ok(0)
}

fn intervals(
    ctx: &Context,
    _engine: &VideoEngine,
    file: &Path,
    kind: &'static str,
    limits: &EngineLimits,
    detect: impl FnOnce(&[u8]) -> crate::crates::video_engine::VideoResult<Vec<(f64, f64)>>,
) -> Result<i32, CliError> {
    let _ = limits;
    let input = read_input(file)?;
    let segs = detect(&input).map_err(map_video_error)?;
    emit_intervals(ctx, kind, &segs)
}

fn emit_intervals(ctx: &Context, kind: &str, segs: &[(f64, f64)]) -> Result<i32, CliError> {
    let v = serde_json::json!({ "kind": kind, "intervals": segs });
    if ctx.emit_structured(&v)? {
        return Ok(0);
    }
    for (start, end) in segs {
        println!("{start:.3} -> {end:.3}");
    }
    Ok(0)
}

fn loudness(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let input = read_input(file)?;
    let report = engine
        .measure_loudness(&input, limits)
        .map_err(map_video_error)?;
    if emit_kv(
        ctx,
        &report,
        &[
            "integrated_lufs",
            "loudness_range_lu",
            "true_peak_dbtp",
            "channel_lra",
        ],
    )? {
        return Ok(0);
    }
    Ok(0)
}

fn audio_stats(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let input = read_input(file)?;
    let stats = engine
        .audio_stats(&input, limits)
        .map_err(map_video_error)?;
    if ctx.emit_structured(&stats)? {
        return Ok(0);
    }
    for (k, v) in &stats {
        println!("{k}: {v}");
    }
    Ok(0)
}

fn quality(
    ctx: &Context,
    engine: &VideoEngine,
    reference: &Path,
    distorted: &Path,
    metrics: Option<&str>,
    limits: &EngineLimits,
) -> Result<i32, CliError> {
    let metric_list: Vec<&str> = match metrics {
        Some(m) => m
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect(),
        None => vec!["psnr", "ssim", "vmaf"],
    };
    if metric_list.is_empty() {
        return Err(CliError::Usage("--metrics is empty".to_string()));
    }
    let reference_bytes = read_input(reference)?;
    let distorted_bytes = read_input(distorted)?;
    let report = engine
        .compare_quality(&reference_bytes, &distorted_bytes, &metric_list, limits)
        .map_err(map_video_error)?;
    if emit_kv(ctx, &report, &["psnr_db", "ssim", "vmaf"])? {
        return Ok(0);
    }
    if !report.supported_metrics.is_empty() {
        println!("supported_metrics: {}", report.supported_metrics.join(", "));
    }
    for note in &report.notes {
        println!("note: {note}");
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// Packaging
// ---------------------------------------------------------------------------

fn package(
    ctx: &Context,
    engine: &VideoEngine,
    file: &Path,
    kind: &'static str,
    output_dir: Option<&Path>,
    limits: &EngineLimits,
    pack: impl FnOnce(
        &VideoEngine,
        &[u8],
        &EngineLimits,
    ) -> crate::crates::video_engine::VideoResult<MediaOutput>,
) -> Result<i32, CliError> {
    let input = read_input(file)?;
    let out = pack(engine, &input, limits).map_err(map_video_error)?;
    write_media_output(ctx, file, output_dir, kind, out, kind)
}

// ---------------------------------------------------------------------------
// Runtime inspection
// ---------------------------------------------------------------------------

fn device_list(ctx: &Context, devices: &[CaptureDevice]) -> Result<i32, CliError> {
    if ctx.emit_structured(&devices)? {
        return Ok(0);
    }
    for d in devices {
        match &d.description {
            Some(desc) if !desc.is_empty() => println!("{} {} — {desc}", d.kind, d.name),
            _ => println!("{} {}", d.kind, d.name),
        }
    }
    Ok(0)
}

fn capabilities(ctx: &Context, engine: &VideoEngine) -> Result<i32, CliError> {
    let ffmpeg = engine.ffmpeg_version().map_err(map_video_error)?;
    let ffprobe = engine.ffprobe_version().map_err(map_video_error)?;
    let hwaccel = engine.detect_hwaccel();
    let devices = engine.list_devices().map_err(map_video_error)?;
    let protocols = engine.list_protocols().map_err(map_video_error)?;
    let hwaccels = engine.list_hwaccels().map_err(map_video_error)?;
    let encode_presets: Vec<&str> = engine.encode_presets().keys().copied().collect();
    let filter_presets: Vec<&str> = engine.filter_presets().iter().map(|p| p.name).collect();
    let report = serde_json::json!({
        "ffmpeg": ffmpeg,
        "ffprobe": ffprobe,
        "detected_hwaccel": hwaccel,
        "devices": devices,
        "protocols": protocols,
        "hwaccels": hwaccels,
        "encode_presets": encode_presets,
        "filter_presets": filter_presets,
    });
    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    println!("ffmpeg: {ffmpeg}");
    println!("ffprobe: {ffprobe}");
    println!("hwaccel: {}", hwaccel.as_deref().unwrap_or("(software)"));
    println!("devices: {}", devices.len());
    println!("protocols: {}", protocols.len());
    println!("hwaccels: {}", hwaccels.len());
    println!("encode_presets: {}", encode_presets.len());
    println!("filter_presets: {}", filter_presets.len());
    Ok(0)
}

#[derive(Debug, Serialize)]
struct VersionsReport {
    ffmpeg: String,
    ffprobe: String,
    hwaccel: Option<String>,
}

fn versions(ctx: &Context, engine: &VideoEngine) -> Result<i32, CliError> {
    let report = VersionsReport {
        ffmpeg: engine.ffmpeg_version().map_err(map_video_error)?,
        ffprobe: engine.ffprobe_version().map_err(map_video_error)?,
        hwaccel: engine.detect_hwaccel(),
    };
    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    println!("ffmpeg: {}", report.ffmpeg);
    println!("ffprobe: {}", report.ffprobe);
    println!(
        "hwaccel: {}",
        report.hwaccel.as_deref().unwrap_or("(software)")
    );
    Ok(0)
}

// ---------------------------------------------------------------------------
// Multi-file output writer
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct MultiWriteReport {
    input: String,
    output_dir: String,
    operation: &'static str,
    files: Vec<String>,
    bytes_written: u64,
}

/// Write a [`MediaOutput`] into `output_dir` (default
/// `<stem>-<marker>/` next to the input). Artifact names are
/// flattened to basenames — `..` and absolute paths are rejected.
fn write_media_output(
    ctx: &Context,
    input: &Path,
    output_dir: Option<&Path>,
    marker: &str,
    out: MediaOutput,
    operation: &'static str,
) -> Result<i32, CliError> {
    let dir = match output_dir {
        Some(d) => d.to_path_buf(),
        None => default_out_dir(input, marker)?,
    };
    std::fs::create_dir_all(&dir).map_err(|e| {
        CliError::Io(std::io::Error::new(
            e.kind(),
            format!("cannot create {}: {e}", dir.display()),
        ))
    })?;
    let mut files = Vec::with_capacity(out.files.len());
    let mut seen = std::collections::HashSet::new();
    let mut bytes = 0u64;
    for f in out.files {
        let name = artifact_name(&f.name)?;
        if !seen.insert(name.clone()) {
            return Err(CliError::Usage(format!(
                "duplicate artifact name {name:?} in output set"
            )));
        }
        let path = dir.join(&name);
        write_output(&path, &f.bytes)?;
        bytes += f.bytes.len() as u64;
        files.push(path.display().to_string());
    }
    let report = MultiWriteReport {
        input: input.display().to_string(),
        output_dir: dir.display().to_string(),
        operation,
        files,
        bytes_written: bytes,
    };
    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    println!(
        "{} {} -> {} ({} files, {} bytes)",
        report.operation,
        report.input,
        report.output_dir,
        report.files.len(),
        report.bytes_written
    );
    for f in &report.files {
        println!("  {f}");
    }
    Ok(0)
}

/// Sanitize a [`MediaFile`](crate::crates::video_engine::MediaFile)
/// name to a plain basename: rejects absolute paths and names that
/// don't reduce to a single safe file name component.
fn artifact_name(name: &str) -> Result<String, CliError> {
    let p = Path::new(name);
    if p.is_absolute() {
        return Err(CliError::Usage(format!(
            "unsafe artifact path (absolute): {name:?}"
        )));
    }
    let base = p
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| CliError::Usage(format!("unsafe artifact path: {name:?}")))?;
    if base.is_empty() || base == "." || base == ".." {
        return Err(CliError::Usage(format!("unsafe artifact path: {name:?}")));
    }
    Ok(base.to_string())
}

/// `<parent>/<stem>-<marker>/` next to the input.
fn default_out_dir(file: &Path, marker: &str) -> Result<PathBuf, CliError> {
    let stem = file
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| CliError::Usage(format!("bad input path {}", file.display())))?;
    let parent = file.parent().unwrap_or_else(|| Path::new("."));
    Ok(parent.join(format!("{stem}-{marker}")))
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn read_input(file: &Path) -> Result<Vec<u8>, CliError> {
    if !file.exists() {
        return Err(CliError::NotFound(file.display().to_string()));
    }
    std::fs::read(file).map_err(|e| {
        CliError::Io(std::io::Error::new(
            e.kind(),
            format!("cannot read {}: {e}", file.display()),
        ))
    })
}

fn write_output(path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    std::fs::write(path, bytes).map_err(|e| {
        CliError::Io(std::io::Error::new(
            e.kind(),
            format!("cannot write {}: {e}", path.display()),
        ))
    })
}

/// Refuse to overwrite any input file.
fn check_output(out: &Path, inputs: &[&Path]) -> Result<(), CliError> {
    for input in inputs {
        if out == *input {
            return Err(CliError::Usage(format!(
                "output would overwrite the input {}; pass --output-file",
                input.display()
            )));
        }
    }
    Ok(())
}

fn check_output_multi(out: &Path, inputs: &[PathBuf]) -> Result<(), CliError> {
    for input in inputs {
        if out == input.as_path() {
            return Err(CliError::Usage(format!(
                "output would overwrite the input {}; pass --output-file",
                input.display()
            )));
        }
    }
    Ok(())
}

/// `<stem>.<marker>.<ext>` next to the input; falls back to appending
/// `.<marker>.<ext>` when the input has no usable name/extension.
fn marked_default(file: &Path, marker: &str, ext: &str) -> Result<PathBuf, CliError> {
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| CliError::Usage(format!("bad input path {}", file.display())))?;
    let out_name = if let Some((stem, _)) = name.rsplit_once('.') {
        format!("{stem}.{marker}.{ext}")
    } else {
        format!("{name}.{marker}.{ext}")
    };
    Ok(file.with_file_name(out_name))
}

/// `<stem>.<marker>.<same-ext>` — for ops that keep the input
/// container. Inputs without an extension default to `mp4`.
fn marked_same_ext(file: &Path, marker: &str) -> Result<PathBuf, CliError> {
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
    marked_default(file, marker, ext)
}

/// `<stem>.trimmed.<same-ext>` — trimmed output keeps the input
/// container; inputs without an extension default to `mp4`.
fn trimmed_default(file: &Path) -> Result<PathBuf, CliError> {
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
    marked_default(file, "trimmed", ext)
}

/// Lowercased extension usable as a file extension (`" MKV "` →
/// `"mkv"`). Rejects empty, over-long, or non-alphanumeric values so a
/// hostile extension cannot escape the output directory via the
/// default output path. The engine validates the actual format.
fn sanitize_extension(ext: &str) -> Result<String, CliError> {
    let cleaned = ext.trim().to_ascii_lowercase();
    let ok = !cleaned.is_empty()
        && cleaned.len() <= 16
        && cleaned.chars().all(|c| c.is_ascii_alphanumeric());
    if !ok {
        return Err(CliError::Usage(format!("invalid extension: {ext:?}")));
    }
    Ok(cleaned)
}

fn require_multi(files: &[PathBuf], op: &str) -> Result<(), CliError> {
    if files.len() < 2 {
        return Err(CliError::Usage(format!(
            "{op} requires at least two input files"
        )));
    }
    Ok(())
}

fn parse_f64_list(s: &str, flag: &str) -> Result<Vec<f64>, CliError> {
    s.split(',')
        .map(|t| {
            let t = t.trim();
            t.parse::<f64>()
                .map_err(|_| CliError::Usage(format!("{flag}: invalid number {t:?}")))
        })
        .collect()
}

fn parse_csv_u32(s: Option<&str>) -> Result<Option<Vec<u32>>, CliError> {
    match s {
        None => Ok(None),
        Some(raw) => {
            let mut out = Vec::new();
            for t in raw.split(',') {
                let t = t.trim();
                if t.is_empty() {
                    continue;
                }
                out.push(
                    t.parse::<u32>()
                        .map_err(|_| CliError::Usage(format!("invalid stream ordinal {t:?}")))?,
                );
            }
            Ok(Some(out))
        }
    }
}

fn pick_out(
    output: Option<&Path>,
    default: Result<PathBuf, CliError>,
) -> Result<PathBuf, CliError> {
    match output {
        Some(p) => Ok(p.to_path_buf()),
        None => default,
    }
}

/// Render a JSON value for table output: strings unquoted, everything
/// else via `Display` (`null`, numbers, booleans).
fn render_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Emit `value` via [`Context::emit_structured`]; on table output,
/// print the listed top-level keys as `key: value` lines (nulls
/// skipped). Returns `true` when structured output was written.
fn emit_kv<T: Serialize>(ctx: &Context, value: &T, keys: &[&str]) -> Result<bool, CliError> {
    if ctx.emit_structured(value)? {
        return Ok(true);
    }
    let v = serde_json::to_value(value)?;
    for key in keys {
        if let Some(val) = v.get(key) {
            if !val.is_null() {
                println!("{key}: {}", render_value(val));
            }
        }
    }
    Ok(false)
}

fn report(
    ctx: &Context,
    input: &Path,
    output: &Path,
    operation: &'static str,
    bytes: usize,
) -> Result<i32, CliError> {
    let report = WriteReport {
        input: input.display().to_string(),
        output: output.display().to_string(),
        operation,
        bytes_written: bytes as u64,
    };
    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    println!(
        "{} {} -> {} ({} bytes)",
        report.operation, report.input, report.output, report.bytes_written
    );
    Ok(0)
}

/// Maps [`VideoError`] onto the CLI contract: a missing ffmpeg/ffprobe
/// binary is `unsupported`, bad input/limit violations are
/// `validation`, external-tool failures are `internal`.
fn map_video_error(e: VideoError) -> CliError {
    match e {
        VideoError::MissingTool { tool } => {
            CliError::Unsupported(format!("required video tool not found: {tool}"))
        }
        VideoError::InvalidInput(msg) | VideoError::LimitExceeded(msg) => CliError::Validation(msg),
        VideoError::ExternalTool { tool, detail } => {
            CliError::Internal(format!("{tool} failed: {detail}"))
        }
        VideoError::Io(err) => CliError::Io(err),
        VideoError::Json(err) => CliError::Json(err),
    }
}
