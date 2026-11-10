//! Cross-domain conversions: audio → video (with waveform
//! visualization), GIF → video, and video → audio-with-artwork.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, checked_ext, encoder_available,
    ext_for_format, find_tool, h264_aac_args, h264_encoder, read_output, run_tool, tool_failure,
    write_input,
};

/// Visualization style for [`audio_to_video`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualStyle {
    /// Oscilloscope waveform (`showwaves`).
    Waveform,
    /// Frequency spectrum (`showspectrum`).
    Spectrum,
    /// Combined waveform+spectrum split (`showwaves` + `showspectrum`
    /// stacked via `vstack`).
    Both,
}

/// `-c:v`/`-c:a` arguments plus output extension for the audio→video
/// encode: prefer H.264+AAC in mp4; fall back to VP9+Opus in webm only
/// when no H.264 encoder exists. Honest error when neither chain is
/// available.
fn visual_encode_args() -> VideoResult<(String, Vec<String>)> {
    if h264_encoder().is_some() {
        // h264_aac_args re-selects the same encoder internally.
        return Ok(("mp4".to_string(), h264_aac_args()?));
    }
    if encoder_available("libvpx-vp9") && encoder_available("libopus") {
        return Ok((
            "webm".to_string(),
            vec![
                "-c:v".to_string(),
                "libvpx-vp9".to_string(),
                "-deadline".to_string(),
                "good".to_string(),
                "-cpu-used".to_string(),
                "8".to_string(),
                "-c:a".to_string(),
                "libopus".to_string(),
            ],
        ));
    }
    Err(VideoError::ExternalTool {
        tool: FFMPEG.to_string(),
        detail: "audio_to_video requires an H.264 encoder (libx264/libopenh264) \
                 or libvpx-vp9+libopus for webm; this ffmpeg build provides neither"
            .to_string(),
    })
}

/// Turn an audio file into a video with a generated visualization.
/// `style` selects the visual; output is `mp4` (or `webm` when no
/// H.264 encoder exists and VP9/Opus do).
pub(crate) fn audio_to_video(
    input: &[u8],
    style: VisualStyle,
    width: u32,
    height: u32,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    if width == 0 || height == 0 {
        return Err(VideoError::InvalidInput(format!(
            "width and height must be positive, got {width}x{height}"
        )));
    }
    let program = find_tool(FFMPEG)?;

    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;
    if !info.has_audio {
        return Err(VideoError::InvalidInput(
            "audio_to_video input has no audio stream".to_string(),
        ));
    }

    let dir = TempDir::new("avvis")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let (ext, codec_args) = visual_encode_args()?;
    let out_path = dir.file(&format!("visual.{ext}"));

    // Visualization filtergraph. `showwaves`/`showspectrum` turn the
    // audio stream into a video stream; `asplit` keeps a copy of the
    // audio for the output soundtrack.
    let (graph, audio_label) = match style {
        VisualStyle::Waveform => (
            format!(
                "[0:a]asplit=2[a1][a2];\
                 [a1]showwaves=s={width}x{height}:mode=p2p:rate=25,format=yuv420p[v]"
            ),
            "a2",
        ),
        VisualStyle::Spectrum => (
            format!(
                "[0:a]asplit=2[a1][a2];\
                 [a1]showspectrum=s={width}x{height}:slide=scroll,format=yuv420p[v]"
            ),
            "a2",
        ),
        VisualStyle::Both => {
            // Each half gets half the requested height (kept even for
            // yuv420p); vstack reassembles them.
            let half = (height / 2).max(2) & !1;
            (
                format!(
                    "[0:a]asplit=3[a1][a2][a3];\
                     [a1]showwaves=s={width}x{half}:mode=p2p[w];\
                     [a2]showspectrum=s={width}x{half}:slide=scroll[sp];\
                     [w][sp]vstack=inputs=2,format=yuv420p[v]"
                ),
                "a3",
            )
        }
    };

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
        format!("[{audio_label}]"),
    ];
    args.extend(codec_args);
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

/// Convert an animated GIF to a real video (`mp4`/`webm`/`mkv`).
/// GIF frame delays are preserved; palettes are converted to
/// yuv420p for compatibility.
pub(crate) fn gif_to_video(
    input: &[u8],
    target_ext: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    let ext = checked_ext(target_ext, &["mp4", "webm", "mkv"], "gif_to_video")?;
    let program = find_tool(FFMPEG)?;

    let info = probe(input)?;
    check_limits(&info, input_len, limits, true)?;

    let dir = TempDir::new("gif2vid")?;
    let input_path = write_input(&dir, "input.gif", input)?;
    let out_path = dir.file(&format!("out.{ext}"));

    // GIF frame delays are honored by default; re-encode because gif's
    // pal8 stream cannot be stream-copied into yuv420p containers.
    let codec_args: Vec<String> = match ext.as_str() {
        "webm" => {
            if !encoder_available("libvpx-vp9") {
                return Err(VideoError::ExternalTool {
                    tool: FFMPEG.to_string(),
                    detail: "webm output requires the libvpx-vp9 encoder; \
                             this ffmpeg build does not provide it"
                        .to_string(),
                });
            }
            vec![
                "-c:v".to_string(),
                "libvpx-vp9".to_string(),
                "-deadline".to_string(),
                "good".to_string(),
                "-cpu-used".to_string(),
                "8".to_string(),
            ]
        }
        _ => {
            let enc = h264_encoder().ok_or_else(|| VideoError::ExternalTool {
                tool: FFMPEG.to_string(),
                detail: "mp4/mkv output requires an H.264 encoder \
                         (libx264 or libopenh264); this ffmpeg build provides neither"
                    .to_string(),
            })?;
            vec![
                "-c:v".to_string(),
                enc.to_string(),
                "-pix_fmt".to_string(),
                "yuv420p".to_string(),
            ]
        }
    };

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-map".to_string(),
        "0:v:0".to_string(),
    ];
    args.extend(codec_args);
    args.extend(["-an".to_string(), out_path.to_string_lossy().into_owned()]);

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

/// Mux `audio` as the soundtrack of `video` *without* re-encoding
/// video (stream copy) — the lightweight counterpart to
/// [`crate::crates::video_engine::ops::stream_ops::merge_av`].
/// Fails when the audio codec cannot be muxed into the video
/// container; caller should fall back to merge_av.
pub(crate) fn replace_audio_track(
    video: &[u8],
    audio: &[u8],
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let video_len = check_input(video)?;
    check_input(audio)?;
    let program = find_tool(FFMPEG)?;

    let video_info = probe(video)?;
    check_limits(&video_info, video_len, limits, false)?;
    if !video_info.has_video {
        return Err(VideoError::InvalidInput(
            "replace_audio_track video input has no video stream".to_string(),
        ));
    }
    let audio_info = probe(audio)?;
    if !audio_info.has_audio {
        return Err(VideoError::InvalidInput(
            "replace_audio_track audio input has no audio stream".to_string(),
        ));
    }

    // Keep the source container: derive the output extension from the
    // probed format_name, defaulting to mp4.
    let ext = video_info
        .format_name
        .as_deref()
        .and_then(ext_for_format)
        .unwrap_or("mp4");
    // Pick an audio encoder the container accepts: Opus for webm,
    // AAC elsewhere (mp4/mov/mkv/avi all take AAC).
    let audio_codec: Vec<String> = if ext == "webm" {
        if !encoder_available("libopus") {
            return Err(VideoError::ExternalTool {
                tool: FFMPEG.to_string(),
                detail: "webm output requires the libopus audio encoder; \
                         this ffmpeg build does not provide it"
                    .to_string(),
            });
        }
        vec!["-c:a".to_string(), "libopus".to_string()]
    } else {
        vec!["-c:a".to_string(), "aac".to_string()]
    };

    let dir = TempDir::new("replaudio")?;
    let video_path = write_input(&dir, "video.bin", video)?;
    let audio_path = write_input(&dir, "audio.bin", audio)?;
    let out_path = dir.file(&format!("out.{ext}"));

    // Stream-copy video (and subtitles), replace/insert the audio
    // track with a re-encode of input 1's first audio stream.
    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        video_path.to_string_lossy().into_owned(),
        "-i".to_string(),
        audio_path.to_string_lossy().into_owned(),
        "-map".to_string(),
        "0:v?".to_string(),
        "-map".to_string(),
        "0:s?".to_string(),
        "-map".to_string(),
        "1:a:0".to_string(),
        "-map_chapters".to_string(),
        "0".to_string(),
        "-c:v".to_string(),
        "copy".to_string(),
        "-c:s".to_string(),
        "copy".to_string(),
    ];
    args.extend(audio_codec);
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::slideshow;
    use crate::crates::video_engine::ops::{FFPROBE, find_tool};
    use std::fs;

    fn tools_available() -> bool {
        find_tool(FFMPEG).is_ok() && find_tool(FFPROBE).is_ok()
    }

    /// Run ffmpeg against lavfi sources; returns output file bytes.
    fn lavfi_to_bytes(args_tail: &[String], out_name: &str) -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let dir = TempDir::new("avvis-test").ok()?;
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

    /// Tiny H.264+AAC mp4 fixture (0.6s, 96x64, with audio).
    fn make_mp4(with_audio: bool) -> Option<Vec<u8>> {
        let enc = crate::crates::video_engine::ops::h264_encoder()?;
        let mut tail = vec![
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc=duration=0.6:size=96x64:rate=10".to_string(),
            "-c:v".to_string(),
            enc.to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
        ];
        if with_audio {
            tail.extend([
                "-f".to_string(),
                "lavfi".to_string(),
                "-i".to_string(),
                "sine=frequency=440:duration=0.6".to_string(),
                "-c:a".to_string(),
                "aac".to_string(),
                "-shortest".to_string(),
            ]);
        } else {
            tail.push("-an".to_string());
        }
        lavfi_to_bytes(&tail, "fixture.mp4")
    }

    fn make_wav() -> Option<Vec<u8>> {
        lavfi_to_bytes(
            &[
                "-f".to_string(),
                "lavfi".to_string(),
                "-i".to_string(),
                "sine=frequency=330:duration=0.5".to_string(),
            ],
            "tone.wav",
        )
    }

    #[test]
    fn audio_to_video_has_av_streams() {
        if !tools_available() {
            return;
        }
        let Some(wav) = make_wav() else { return };
        let limits = EngineLimits::default();
        for style in [
            VisualStyle::Waveform,
            VisualStyle::Spectrum,
            VisualStyle::Both,
        ] {
            let out = audio_to_video(&wav, style, 160, 120, &limits)
                .unwrap_or_else(|e| panic!("audio_to_video {style:?}: {e}"));
            let info = probe(&out).expect("probe audio_to_video output");
            assert!(info.has_video, "{style:?}: no video stream");
            assert!(info.has_audio, "{style:?}: no audio stream");
        }
    }

    #[test]
    fn gif_to_video_roundtrip() {
        if !tools_available() {
            return;
        }
        let Some(mp4) = make_mp4(true) else { return };
        let limits = EngineLimits::default();
        let gif = slideshow::video_to_gif(&mp4, Some(8.0), Some(64), &limits)
            .expect("video_to_gif fixture");
        let mp4_back = gif_to_video(&gif, "mp4", &limits).expect("gif_to_video mp4");
        let info = probe(&mp4_back).expect("probe gif_to_video output");
        assert!(info.has_video);
        // mkv path also exercised cheaply.
        let mkv = gif_to_video(&gif, "mkv", &limits).expect("gif_to_video mkv");
        assert!(!mkv.is_empty());
    }

    #[test]
    fn replace_audio_track_swaps_soundtrack() {
        if !tools_available() {
            return;
        }
        // Video without audio + wav → output gains an aac track.
        let Some(mp4) = make_mp4(false) else { return };
        let Some(wav) = make_wav() else { return };
        let limits = EngineLimits::default();
        let out = replace_audio_track(&mp4, &wav, &limits).expect("replace_audio_track");
        let info = probe(&out).expect("probe replace_audio_track output");
        assert!(info.has_video, "video stream lost");
        assert!(info.has_audio, "no audio track in output");
        assert_eq!(
            info.acodec.as_deref(),
            Some("aac"),
            "expected aac audio, got {:?}",
            info.acodec
        );
        // Video with existing aac + pcm wav → still aac (replaced).
        let Some(mp4a) = make_mp4(true) else { return };
        let out2 = replace_audio_track(&mp4a, &wav, &limits).expect("replace_audio_track 2");
        let info2 = probe(&out2).expect("probe replace_audio_track output 2");
        assert!(info2.has_audio);
        assert_eq!(info2.acodec.as_deref(), Some("aac"));
    }
}
