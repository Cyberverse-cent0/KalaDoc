//! Slideshow and image-sequence → video, plus animated-image output
//! (GIF, animated WebP, APNG).

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, checked_ext, encoder_available,
    find_tool, h264_aac_args, probe_json, read_output, run_tool, tool_failure, write_input,
};
use std::path::Path;

/// Containers [`images_to_video`] can emit.
const SLIDESHOW_TARGETS: &[&str] = &["mp4", "webm", "mkv", "gif", "webp", "apng"];

/// Sniff the image format from magic bytes so the concat demuxer gets
/// a believable extension (ffmpeg's image2 probes content anyway, but
/// a correct suffix avoids edge cases in codec selection).
fn sniff_image_ext(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG") {
        "png"
    } else if bytes.starts_with(b"\xFF\xD8") {
        "jpg"
    } else if bytes.starts_with(b"GIF8") {
        "gif"
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "webp"
    } else if bytes.starts_with(b"BM") {
        "bmp"
    } else {
        "png"
    }
}

/// Probed pixel dimensions of an image file, via ffprobe JSON.
fn image_dims(path: &Path) -> VideoResult<(u32, u32)> {
    let json = probe_json(path)?;
    let stream = json
        .get("streams")
        .and_then(|s| s.as_array())
        .and_then(|streams| {
            streams
                .iter()
                .find(|s| s.get("codec_type").and_then(|c| c.as_str()) == Some("video"))
        })
        .ok_or_else(|| {
            VideoError::InvalidInput("image input has no decodable video stream".to_string())
        })?;
    let w = stream.get("width").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let h = stream.get("height").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    if w == 0 || h == 0 {
        return Err(VideoError::InvalidInput(
            "image input reported zero dimensions".to_string(),
        ));
    }
    Ok((w, h))
}

/// Shared video-scaling prelude for animated outputs: fit inside
/// `max_w`×`max_h` preserving aspect, pad to exact size on black, then
/// resample to `fps`.
fn scale_pad_fps(max_w: u32, max_h: u32, fps: f64) -> String {
    format!(
        "scale={max_w}:{max_h}:force_original_aspect_ratio=decrease,\
         pad={max_w}:{max_h}:(ow-iw)/2:(oh-ih)/2:color=black,fps={fps}"
    )
}

/// Two-stage GIF encoder graph appended after the prelude: split the
/// normalized frames, generate a 128-color palette from one branch and
/// dither-map the other.
fn gif_tail() -> &'static str {
    ",split[a][b];[a]palettegen=max_colors=128[p];[b][p]paletteuse=dither=bayer[v]"
}

/// Build a video from `inputs` (PNG/JPEG/etc. images, in order).
/// `fps` is the output frame rate; `duration_per_image` overrides
/// per-image timing (`None` → `1/fps` each). `target_ext` selects
/// the container (`mp4`, `webm`, `gif`, `webp`, `apng`, `mkv`).
///
/// Inputs may be mixed sizes — the largest dimensions win and smaller
/// frames are centered on black via `pad`/`overlay` as needed.
pub(crate) fn images_to_video(
    inputs: &[Vec<u8>],
    fps: f64,
    duration_per_image: Option<f64>,
    target_ext: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    if inputs.is_empty() {
        return Err(VideoError::InvalidInput(
            "images_to_video requires at least one image".to_string(),
        ));
    }
    if !fps.is_finite() || fps <= 0.0 {
        return Err(VideoError::InvalidInput(format!(
            "fps must be a finite positive number, got {fps}"
        )));
    }
    let per_image = match duration_per_image {
        Some(d) => {
            if !d.is_finite() || d <= 0.0 {
                return Err(VideoError::InvalidInput(format!(
                    "duration_per_image must be a finite positive number, got {d}"
                )));
            }
            d
        }
        None => 1.0 / fps,
    };
    let ext = checked_ext(target_ext, SLIDESHOW_TARGETS, "images_to_video")?;
    let program = find_tool(FFMPEG)?;

    // Bound the aggregate input size; per-image content is validated
    // by the ffprobe dimension pass below.
    let total: u64 = inputs.iter().map(|b| b.len() as u64).sum();
    if let Some(max) = limits.max_input_bytes {
        if total > max {
            return Err(VideoError::LimitExceeded(format!(
                "slideshow inputs total {total} bytes, exceeding max_input_bytes {max}"
            )));
        }
    }
    for bytes in inputs {
        check_input(bytes)?;
    }

    let dir = TempDir::new("slideshow")?;

    // Stage every image and probe dimensions so the largest frame wins.
    let mut max_w = 0u32;
    let mut max_h = 0u32;
    let mut names = Vec::with_capacity(inputs.len());
    for (i, bytes) in inputs.iter().enumerate() {
        let name = format!("img_{i:06}.{}", sniff_image_ext(bytes));
        let path = write_input(&dir, &name, bytes)?;
        let (w, h) = image_dims(&path)?;
        max_w = max_w.max(w);
        max_h = max_h.max(h);
        names.push(name);
    }

    // Concat-demuxer playlist. A `duration` directive after each `file`
    // entry sets that image's on-screen time; emitting one for every
    // entry (including the last) gives exact per-image timing.
    let mut list = String::new();
    for name in &names {
        list.push_str(&format!("file '{name}'\nduration {per_image:.3}\n"));
    }
    let list_path = dir.file("list.txt");
    std::fs::write(&list_path, list)?;

    let pre = scale_pad_fps(max_w, max_h, fps);
    let out_path = dir.file(&format!("out.{ext}"));

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-f".to_string(),
        "concat".to_string(),
        "-safe".to_string(),
        "0".to_string(),
        "-i".to_string(),
        list_path.to_string_lossy().into_owned(),
    ];

    match ext.as_str() {
        "gif" => {
            // Single-command palettegen+paletteuse via filter_complex.
            args.push("-filter_complex".to_string());
            args.push(format!("[0:v]{pre}{}", gif_tail()));
            args.push("-map".to_string());
            args.push("[v]".to_string());
            args.extend(["-an".to_string(), "-f".to_string(), "gif".to_string()]);
        }
        "webp" => {
            let enc = if encoder_available("libwebp_anim") {
                "libwebp_anim"
            } else if encoder_available("libwebp") {
                "libwebp"
            } else {
                return Err(VideoError::ExternalTool {
                    tool: FFMPEG.to_string(),
                    detail: "animated WebP output requires libwebp_anim or libwebp; \
                             this ffmpeg build provides neither"
                        .to_string(),
                });
            };
            args.extend([
                "-vf".to_string(),
                pre,
                "-c:v".to_string(),
                enc.to_string(),
                "-lossless".to_string(),
                "0".to_string(),
                "-quality".to_string(),
                "75".to_string(),
                "-loop".to_string(),
                "0".to_string(),
                "-an".to_string(),
            ]);
        }
        "apng" => {
            if !encoder_available("apng") {
                return Err(VideoError::ExternalTool {
                    tool: FFMPEG.to_string(),
                    detail: "APNG output requires the apng encoder; \
                             this ffmpeg build does not provide it"
                        .to_string(),
                });
            }
            args.extend([
                "-vf".to_string(),
                pre,
                "-c:v".to_string(),
                "apng".to_string(),
                "-f".to_string(),
                "apng".to_string(),
                "-an".to_string(),
            ]);
        }
        "webm" => {
            if !encoder_available("libvpx-vp9") {
                return Err(VideoError::ExternalTool {
                    tool: FFMPEG.to_string(),
                    detail: "webm output requires the libvpx-vp9 encoder; \
                             this ffmpeg build does not provide it"
                        .to_string(),
                });
            }
            args.extend([
                "-vf".to_string(),
                pre,
                "-c:v".to_string(),
                "libvpx-vp9".to_string(),
                "-deadline".to_string(),
                "good".to_string(),
                "-cpu-used".to_string(),
                "8".to_string(),
                "-an".to_string(),
                "-f".to_string(),
                "webm".to_string(),
            ]);
        }
        _ => {
            // mp4 / mkv: re-encode to the best available H.264 + AAC.
            args.extend(h264_aac_args()?);
            args.extend([
                "-vf".to_string(),
                pre,
                "-an".to_string(),
                "-f".to_string(),
                if ext == "mkv" { "matroska" } else { "mp4" }.to_string(),
            ]);
        }
    }

    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

/// Convert `input` video to an animated GIF. `fps` caps the output
/// frame rate (default 10); `width` scales proportionally (`None` →
/// source width, capped at 640 for sanity).
pub(crate) fn video_to_gif(
    input: &[u8],
    fps: Option<f64>,
    width: Option<u32>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    let fps = match fps {
        Some(f) if f.is_finite() && f > 0.0 => f,
        Some(f) => {
            return Err(VideoError::InvalidInput(format!(
                "fps must be a finite positive number, got {f}"
            )));
        }
        None => 10.0,
    };
    let width = match width {
        Some(w) if w > 0 => w,
        Some(_) => {
            return Err(VideoError::InvalidInput(
                "width must be a positive number".to_string(),
            ));
        }
        None => 0, // sentinel: auto, capped at 640
    };
    let program = find_tool(FFMPEG)?;

    let info = probe(input)?;
    check_limits(&info, input_len, limits, true)?;

    let dir = TempDir::new("gif")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file("out.gif");

    let scale = if width == 0 {
        // Never upscale: keep source width when smaller than 640.
        "scale=w='min(iw,640)':-2:flags=lanczos".to_string()
    } else {
        format!("scale={width}:-2:flags=lanczos")
    };
    let graph = format!("[0:v]fps={fps},{scale}{}", gif_tail());

    let args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-filter_complex".to_string(),
        graph,
        "-map".to_string(),
        "[v]".to_string(),
        "-an".to_string(),
        "-f".to_string(),
        "gif".to_string(),
        out_path.to_string_lossy().into_owned(),
    ];
    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    let gif = read_output(&out_path)?;
    if !gif.starts_with(b"GIF8") {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "ffmpeg produced a file without a GIF magic header".to_string(),
        });
    }
    Ok(gif)
}

/// Convert `input` video to an animated WebP (`-loop 0`). `fps`
/// optional; quality is fixed at a sane default (libwebp `-quality 75`).
pub(crate) fn video_to_animated_webp(
    input: &[u8],
    fps: Option<f64>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    let fps = match fps {
        Some(f) if f.is_finite() && f > 0.0 => f,
        Some(f) => {
            return Err(VideoError::InvalidInput(format!(
                "fps must be a finite positive number, got {f}"
            )));
        }
        None => 10.0,
    };
    let program = find_tool(FFMPEG)?;

    let enc = if encoder_available("libwebp_anim") {
        "libwebp_anim"
    } else if encoder_available("libwebp") {
        "libwebp"
    } else {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "animated WebP output requires libwebp_anim or libwebp; \
                     this ffmpeg build provides neither"
                .to_string(),
        });
    };

    let info = probe(input)?;
    check_limits(&info, input_len, limits, true)?;

    let dir = TempDir::new("webp")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file("out.webp");

    let args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-vf".to_string(),
        format!("fps={fps}"),
        "-c:v".to_string(),
        enc.to_string(),
        "-lossless".to_string(),
        "0".to_string(),
        "-quality".to_string(),
        "75".to_string(),
        "-loop".to_string(),
        "0".to_string(),
        "-an".to_string(),
        out_path.to_string_lossy().into_owned(),
    ];
    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

/// Convert `input` video to APNG (animated PNG) when the build's
/// `apng` muxer/encoder exists; honest `ExternalTool` error
/// otherwise.
pub(crate) fn video_to_apng(
    input: &[u8],
    fps: Option<f64>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    let fps = match fps {
        Some(f) if f.is_finite() && f > 0.0 => f,
        Some(f) => {
            return Err(VideoError::InvalidInput(format!(
                "fps must be a finite positive number, got {f}"
            )));
        }
        None => 10.0,
    };
    if !encoder_available("apng") {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "APNG output requires the apng encoder; \
                     this ffmpeg build does not provide it"
                .to_string(),
        });
    }
    let program = find_tool(FFMPEG)?;

    let info = probe(input)?;
    check_limits(&info, input_len, limits, true)?;

    let dir = TempDir::new("apng")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file("out.apng");

    let args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-vf".to_string(),
        format!("fps={fps}"),
        "-c:v".to_string(),
        "apng".to_string(),
        "-f".to_string(),
        "apng".to_string(),
        "-an".to_string(),
        out_path.to_string_lossy().into_owned(),
    ];
    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    let png = read_output(&out_path)?;
    if !png.starts_with(b"\x89PNG") {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "ffmpeg produced a file without a PNG magic header".to_string(),
        });
    }
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, find_tool};
    use std::fs;

    fn tools_available() -> bool {
        find_tool(FFMPEG).is_ok() && find_tool(FFPROBE).is_ok()
    }

    /// Run ffmpeg against lavfi sources; returns output file bytes.
    fn lavfi_to_bytes(args_tail: &[String], out_name: &str) -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let dir = TempDir::new("slideshow-test").ok()?;
        let out = dir.file(out_name);
        let mut args = vec!["-y".to_string(), "-v".to_string(), "error".to_string()];
        args.extend(args_tail.iter().cloned());
        args.push(out.to_string_lossy().into_owned());
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    /// Three solid-color PNGs (red/green/blue, 64x64).
    fn make_pngs() -> Option<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        for (i, color) in ["red", "green", "blue"].iter().enumerate() {
            let png = lavfi_to_bytes(
                &[
                    "-f".to_string(),
                    "lavfi".to_string(),
                    "-i".to_string(),
                    format!("color=c={color}:s=64x64:d=0.1"),
                    "-frames:v".to_string(),
                    "1".to_string(),
                ],
                &format!("img{i}.png"),
            )?;
            out.push(png);
        }
        Some(out)
    }

    /// Tiny H.264+AAC mp4 fixture (0.6s, 96x64).
    fn make_mp4() -> Option<Vec<u8>> {
        let enc = crate::crates::video_engine::ops::h264_encoder()?;
        lavfi_to_bytes(
            &[
                "-f".to_string(),
                "lavfi".to_string(),
                "-i".to_string(),
                "testsrc=duration=0.6:size=96x64:rate=10".to_string(),
                "-f".to_string(),
                "lavfi".to_string(),
                "-i".to_string(),
                "sine=frequency=440:duration=0.6".to_string(),
                "-c:v".to_string(),
                enc.to_string(),
                "-pix_fmt".to_string(),
                "yuv420p".to_string(),
                "-c:a".to_string(),
                "aac".to_string(),
                "-shortest".to_string(),
            ],
            "fixture.mp4",
        )
    }

    #[test]
    fn images_to_video_mp4_duration() {
        if !tools_available() {
            return;
        }
        let Some(pngs) = make_pngs() else { return };
        let limits = EngineLimits::default();
        let mp4 =
            images_to_video(&pngs, 10.0, Some(0.4), "mp4", &limits).expect("images_to_video mp4");
        assert!(!mp4.is_empty());
        let info = probe(&mp4).expect("probe slideshow mp4");
        assert!(info.has_video, "slideshow output has no video stream");
        let dur = info.duration_secs.unwrap_or(0.0);
        // 3 images × 0.4s = 1.2s, allow container rounding slack.
        assert!(
            (dur - 1.2).abs() < 0.35,
            "duration {dur} far from expected ~1.2s"
        );
    }

    #[test]
    fn images_to_video_gif_magic() {
        if !tools_available() {
            return;
        }
        let Some(pngs) = make_pngs() else { return };
        let limits = EngineLimits::default();
        let gif = images_to_video(&pngs, 10.0, None, "gif", &limits).expect("images_to_video gif");
        assert!(gif.starts_with(b"GIF8"), "output is not a GIF");
    }

    #[test]
    fn video_to_gif_produces_gif() {
        if !tools_available() {
            return;
        }
        let Some(mp4) = make_mp4() else { return };
        let limits = EngineLimits::default();
        let gif = video_to_gif(&mp4, Some(8.0), Some(64), &limits).expect("video_to_gif");
        assert!(gif.starts_with(b"GIF8"));
    }

    #[test]
    fn video_to_animated_webp_and_apng() {
        if !tools_available() {
            return;
        }
        let Some(mp4) = make_mp4() else { return };
        let limits = EngineLimits::default();
        let webp =
            video_to_animated_webp(&mp4, Some(8.0), &limits).expect("video_to_animated_webp");
        assert!(webp.len() > 12 && &webp[..4] == b"RIFF" && &webp[8..12] == b"WEBP");
        if encoder_available("apng") {
            let apng = video_to_apng(&mp4, Some(8.0), &limits).expect("video_to_apng");
            assert!(apng.starts_with(b"\x89PNG"));
        }
    }
}
