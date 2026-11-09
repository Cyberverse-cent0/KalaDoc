//! Overlay and compositing ops: image/logo overlay, drawtext,
//! chroma key.
//!
//! All three ops re-encode: the output container matches the probed
//! input container (see [`ext_for_format`]) and the video codec is the
//! best available H.264 encoder ([`h264_aac_args`], libx264 →
//! libopenh264 fallback), except `webm` inputs which get VP9+Opus.
//! Audio streams are carried through (`-map "0:a?"` / `"1:a?"`) and
//! re-encoded to AAC alongside the video.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, encoder_available, ext_for_format,
    find_tool, h264_aac_args, read_output, run_tool, tool_failure, write_input,
};

/// Sniff a still-image payload and return the extension ffmpeg's
/// `image2` demuxer expects. The demuxer mostly auto-detects by
/// content, but giving the file a truthful extension avoids
/// misdetection on ambiguous payloads.
fn sniff_image_ext(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        Some("jpg")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else {
        None
    }
}

/// Normalize a caller-supplied color for `fontcolor`/`colorkey`:
/// `"RRGGBB"` or `"#RRGGBB"` becomes ffmpeg's `0xRRGGBB`; `0x...`
/// and named colors pass through untouched. Anything else is passed
/// through as-is — ffmpeg rejects it honestly at filter parse time.
fn color_arg(color: &str) -> String {
    let t = color.trim();
    let hex = t.strip_prefix('#').unwrap_or(t);
    if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        format!("0x{hex}")
    } else {
        t.to_string()
    }
}

/// Validate a 0.0–1.0 range parameter (`opacity`, `similarity`,
/// `blend`). `NaN`/infinities and out-of-range values are
/// [`VideoError::InvalidInput`].
fn checked_unit(value: f64, name: &str) -> VideoResult<f64> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(VideoError::InvalidInput(format!(
            "{name} must be a finite number in 0.0..=1.0, got {value}"
        )));
    }
    Ok(value)
}

/// Resolve the output extension for a probed input: the container
/// family is preserved (never changes muxer mid-stream), and an
/// unnameable container is an honest [`VideoError::InvalidInput`].
fn output_ext(info: &crate::crates::video_engine::model::VideoInfo) -> VideoResult<String> {
    info.format_name
        .as_deref()
        .and_then(ext_for_format)
        .map(str::to_string)
        .ok_or_else(|| {
            VideoError::InvalidInput(
                "cannot determine output container from probed format_name".to_string(),
            )
        })
}

/// `-c:v`/`-c:a` args for the re-encoded output. `webm` needs VP9 +
/// Opus (H.264 cannot mux there); everything else uses the best H.264
/// encoder plus AAC. Fails honestly when the required encoders are
/// absent from this ffmpeg build.
fn reencode_args(ext: &str) -> VideoResult<Vec<String>> {
    if ext == "webm" {
        for enc in ["libvpx-vp9", "libopus"] {
            if !encoder_available(enc) {
                return Err(VideoError::ExternalTool {
                    tool: FFMPEG.to_string(),
                    detail: format!(
                        "webm output requires the '{enc}' encoder; \
                         this ffmpeg build does not provide it"
                    ),
                });
            }
        }
        return Ok(vec![
            "-c:v".into(),
            "libvpx-vp9".into(),
            "-deadline".into(),
            "good".into(),
            "-cpu-used".into(),
            "8".into(),
            "-pix_fmt".into(),
            "yuv420p".into(),
            "-c:a".into(),
            "libopus".into(),
        ]);
    }
    h264_aac_args()
}

/// Escape a filesystem path for embedding inside a single-quoted
/// ffmpeg filtergraph option value (`textfile='...'`). TempDir paths
/// are already safe, but filtergraph metacharacters (`\`, `:`, `'`)
/// are backslash-escaped so a hostile tempdir location cannot break
/// out of the quoting.
fn filter_path(path: &std::path::Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace(':', "\\:")
        .replace('\'', "\\'")
}

/// Probe `input`, enforce limits, and require a primary video stream.
/// Returns the [`VideoInfo`] for callers that also need the format.
fn checked_video_input(
    input: &[u8],
    limits: &EngineLimits,
) -> VideoResult<crate::crates::video_engine::model::VideoInfo> {
    let input_len = check_input(input)?;
    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;
    if !info.has_video {
        return Err(VideoError::InvalidInput(
            "input has no video stream to composite onto".to_string(),
        ));
    }
    Ok(info)
}

/// Shared tail of every op: write the ffmpeg args, run, require
/// success, read the output file back.
fn run_encode(
    program: &std::path::Path,
    args: &[String],
    out_path: &std::path::Path,
) -> VideoResult<Vec<u8>> {
    let output = run_tool(FFMPEG, program, args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(out_path)
}

/// Overlay `overlay` (PNG/JPEG/WebP/GIF with optional alpha) onto
/// `input` at `(x, y)` — negative values anchor from the right/bottom
/// edge per the ffmpeg `overlay` filter (`x=-w-10` = 10px from right).
/// `opacity` (0.0–1.0) pre-multiplies the overlay alpha via
/// `colorchannelmixer=aa=` on a `format=rgba` chain; `None` leaves
/// the image's own alpha untouched. The single-frame overlay input
/// relies on the overlay filter's default `eof_action=repeat`, which
/// holds the last (only) frame for the whole video — no `-loop` or
/// `-shortest` is used, so the base video duration is preserved.
pub(crate) fn overlay_image(
    input: &[u8],
    overlay: &[u8],
    x: i32,
    y: i32,
    opacity: Option<f64>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let info = checked_video_input(input, limits)?;
    check_input(overlay)?;
    if let Some(o) = opacity {
        checked_unit(o, "opacity")?;
    }
    let img_ext = sniff_image_ext(overlay).ok_or_else(|| {
        VideoError::InvalidInput(
            "overlay image must be PNG, JPEG, WebP, or GIF (magic bytes unrecognized)".to_string(),
        )
    })?;
    let program = find_tool(FFMPEG)?;
    let ext = output_ext(&info)?;

    let dir = TempDir::new("overlay")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let overlay_path = write_input(&dir, &format!("overlay.{img_ext}"), overlay)?;
    let out_path = dir.file(&format!("out.{ext}"));

    // Alpha-normalize the overlay, optionally scaling alpha by the
    // requested opacity, then composite. `eof_action=repeat` keeps
    // the still image over every frame of the base video.
    let mut ov_chain = "format=rgba".to_string();
    if let Some(o) = opacity {
        ov_chain.push_str(&format!(",colorchannelmixer=aa={o}"));
    }
    let graph = format!("[1:v]{ov_chain}[ov];[0:v][ov]overlay=x={x}:y={y}:eof_action=repeat[v]");

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-i".to_string(),
        overlay_path.to_string_lossy().into_owned(),
        "-filter_complex".to_string(),
        graph,
        "-map".to_string(),
        "[v]".to_string(),
        "-map".to_string(),
        "0:a?".to_string(),
    ];
    args.extend(reencode_args(&ext)?);
    args.push(out_path.to_string_lossy().into_owned());

    run_encode(&program, &args, &out_path)
}

/// Default pixel size when `font_size == 0`.
const DEFAULT_FONT_SIZE: u32 = 48;

/// Draw `text` onto `input` at `(x, y)` using ffmpeg's `drawtext`
/// filter with a 2px black outline for legibility.
///
/// **Escaping**: the text is written to a temp file and referenced via
/// `textfile='{path}'` — no `:`, `'`, `%`, or `\` escaping of the text
/// itself is needed (the file path is metacharacter-escaped by
/// [`filter_path`]). Note ffmpeg still applies its `%{...}`/strftime
/// expansion to `textfile` contents, so literal `%{` sequences in the
/// text are interpreted by ffmpeg.
///
/// `font_size` is in pixels; `0` selects [`DEFAULT_FONT_SIZE`] (48).
/// `color` accepts `"RRGGBB"`/`"#RRGGBB"` (normalized to `0xRRGGBB`)
/// or any ffmpeg color name; `None` uses white.
pub(crate) fn draw_text(
    input: &[u8],
    text: &str,
    x: i32,
    y: i32,
    font_size: u32,
    color: Option<&str>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let info = checked_video_input(input, limits)?;
    if text.is_empty() {
        return Err(VideoError::InvalidInput("text is empty".to_string()));
    }
    let program = find_tool(FFMPEG)?;
    let ext = output_ext(&info)?;

    let dir = TempDir::new("drawtext")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    // Raw bytes: textfile is read verbatim, so no encoding tricks.
    let text_path = write_input(&dir, "text.txt", text.as_bytes())?;
    let out_path = dir.file(&format!("out.{ext}"));

    let font_size = if font_size == 0 {
        DEFAULT_FONT_SIZE
    } else {
        font_size
    };
    let fontcolor = color.map(color_arg).unwrap_or_else(|| "white".to_string());
    let graph = format!(
        "[0:v]drawtext=textfile='{tf}':x={x}:y={y}:fontsize={font_size}:\
         fontcolor={fontcolor}:borderw=2:bordercolor=black[v]",
        tf = filter_path(&text_path),
    );

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-filter_complex".to_string(),
        graph,
        "-map".to_string(),
        "[v]".to_string(),
        "-map".to_string(),
        "0:a?".to_string(),
    ];
    args.extend(reencode_args(&ext)?);
    args.push(out_path.to_string_lossy().into_owned());

    run_encode(&program, &args, &out_path)
}

/// Composite `foreground` over `background`, removing pixels matching
/// `color` via the `colorkey` filter.
///
/// Input mapping: `foreground` is `-i 0` (the clip *with* the
/// green/blue screen — it gets `colorkey`), `background` is `-i 1`
/// (the backdrop). The keyed foreground is centered over the
/// background via `overlay=(W-w)/2:(H-h)/2`; mismatched sizes are
/// legal — the foreground is clipped or letterboxed by the overlay
/// filter, it is not rescaled. `similarity`/`blend` are colorkey's
/// 0.0–1.0 range parameters. Output audio comes from `background`
/// (`-map "1:a?"`); the output container matches the probed
/// `background` format.
pub(crate) fn chroma_key(
    foreground: &[u8],
    background: &[u8],
    color: &str,
    similarity: f64,
    blend: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let fg_info = checked_video_input(foreground, limits)?;
    let bg_info = checked_video_input(background, limits)?;
    let similarity = checked_unit(similarity, "similarity")?;
    let blend = checked_unit(blend, "blend")?;
    let _ = fg_info; // geometry intentionally not constrained — see docs.
    let program = find_tool(FFMPEG)?;
    let ext = output_ext(&bg_info)?;

    let dir = TempDir::new("chromakey")?;
    let fg_path = write_input(&dir, "foreground.bin", foreground)?;
    let bg_path = write_input(&dir, "background.bin", background)?;
    let out_path = dir.file(&format!("out.{ext}"));

    let key_color = color_arg(color);
    let graph = format!(
        "[0:v]colorkey={key_color}:{similarity}:{blend}[ck];\
         [1:v][ck]overlay=(W-w)/2:(H-h)/2[v]"
    );

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        fg_path.to_string_lossy().into_owned(),
        "-i".to_string(),
        bg_path.to_string_lossy().into_owned(),
        "-filter_complex".to_string(),
        graph,
        "-map".to_string(),
        "[v]".to_string(),
        "-map".to_string(),
        "1:a?".to_string(),
    ];
    args.extend(reencode_args(&ext)?);
    args.push(out_path.to_string_lossy().into_owned());

    run_encode(&program, &args, &out_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, h264_encoder};
    use std::fs;

    fn tools_available() -> bool {
        find_tool(FFMPEG).is_ok() && find_tool(FFPROBE).is_ok()
    }

    /// Run ffmpeg with `args` inside `dir`; `None` on any failure.
    fn ffmpeg(dir: &TempDir, name: &str, args: &[String]) -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let out = dir.file(name);
        let mut full = vec!["-y".to_string(), "-v".to_string(), "error".to_string()];
        full.extend(args.iter().cloned());
        full.push(out.to_string_lossy().into_owned());
        let output = run_tool(FFMPEG, &program, &full, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    /// 1s 160x120@10fps H.264+AAC mp4 (video+audio). `None` when
    /// ffmpeg or an H.264 encoder is unavailable.
    fn make_clip(tag: &str) -> Option<Vec<u8>> {
        let dir = TempDir::new(tag).ok()?;
        let enc = h264_encoder()?;
        ffmpeg(
            &dir,
            "clip.mp4",
            &[
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                "testsrc=duration=1:size=160x120:rate=10".into(),
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                "sine=frequency=440:duration=1".into(),
                "-c:v".into(),
                enc.to_string(),
                "-pix_fmt".into(),
                "yuv420p".into(),
                "-c:a".into(),
                "aac".into(),
                "-shortest".into(),
            ],
        )
    }

    /// Solid-color video clip (green screen fixture). Video only.
    fn make_color_clip(tag: &str, color: &str) -> Option<Vec<u8>> {
        let dir = TempDir::new(tag).ok()?;
        let enc = h264_encoder()?;
        ffmpeg(
            &dir,
            "color.mp4",
            &[
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                format!("color={color}:size=160x120:duration=1:rate=10"),
                "-c:v".into(),
                enc.to_string(),
                "-pix_fmt".into(),
                "yuv420p".into(),
                "-an".into(),
            ],
        )
    }

    /// 64x64 PNG still via lavfi `color` + image2. No encoder needed.
    fn make_png(tag: &str) -> Option<Vec<u8>> {
        let dir = TempDir::new(tag).ok()?;
        ffmpeg(
            &dir,
            "logo.png",
            &[
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                "color=red:size=64x64".into(),
                "-frames:v".into(),
                "1".into(),
            ],
        )
    }

    #[test]
    fn overlay_image_produces_video() {
        if !tools_available() {
            return;
        }
        let (Some(clip), Some(png)) = (make_clip("ov-clip"), make_png("ov-png")) else {
            return;
        };
        let out = overlay_image(&clip, &png, 10, 10, Some(0.5), &EngineLimits::default())
            .expect("overlay_image failed");
        assert!(!out.is_empty());
        let info = probe(&out).expect("output not probeable");
        assert!(info.has_video);
        assert!(info.has_audio, "audio stream should be preserved");
    }

    #[test]
    fn overlay_image_validates_inputs() {
        if !tools_available() {
            return;
        }
        let (Some(clip), Some(png)) = (make_clip("ovv-clip"), make_png("ovv-png")) else {
            return;
        };
        // Opacity outside 0..=1 is rejected before ffmpeg runs.
        assert!(matches!(
            overlay_image(&clip, &png, 0, 0, Some(1.5), &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        assert!(matches!(
            overlay_image(&clip, &png, 0, 0, Some(f64::NAN), &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        // Non-image overlay bytes are rejected by magic-byte sniffing.
        assert!(matches!(
            overlay_image(&clip, b"not an image", 0, 0, None, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        // Garbage base input fails as InvalidInput via ffprobe.
        assert!(matches!(
            overlay_image(b"junk", &png, 0, 0, None, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn draw_text_produces_video() {
        if !tools_available() {
            return;
        }
        let Some(clip) = make_clip("dt-clip") else {
            return;
        };
        // Deliberately hostile text: `:` `'` `%` `\` `{` `}` — the
        // textfile path must make all of these safe.
        let out = draw_text(
            &clip,
            "HELLO: 100% 'quoted' \\ path {pts}",
            5,
            20,
            0, // 0 → default 48px
            Some("FF0000"),
            &EngineLimits::default(),
        )
        .expect("draw_text failed");
        assert!(out.len() > 100);
        let info = probe(&out).expect("output not probeable");
        assert!(info.has_video);
    }

    #[test]
    fn draw_text_validates() {
        if !tools_available() {
            return;
        }
        let Some(clip) = make_clip("dtv-clip") else {
            return;
        };
        assert!(matches!(
            draw_text(&clip, "", 0, 0, 24, None, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn chroma_key_produces_video() {
        if !tools_available() {
            return;
        }
        let (Some(fg), Some(bg)) = (make_color_clip("ck-fg", "green"), make_clip("ck-bg")) else {
            return;
        };
        let out = chroma_key(&fg, &bg, "green", 0.1, 0.1, &EngineLimits::default())
            .expect("chroma_key failed");
        assert!(out.len() > 100);
        let info = probe(&out).expect("output not probeable");
        assert!(info.has_video);
        // Background audio is the output audio track.
        assert!(info.has_audio);
    }

    #[test]
    fn chroma_key_validates_ranges() {
        if !tools_available() {
            return;
        }
        let (Some(fg), Some(bg)) = (make_color_clip("ckv-fg", "green"), make_clip("ckv-bg")) else {
            return;
        };
        assert!(matches!(
            chroma_key(&fg, &bg, "green", 1.5, 0.1, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        assert!(matches!(
            chroma_key(&fg, &bg, "green", 0.1, -0.1, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }
}
