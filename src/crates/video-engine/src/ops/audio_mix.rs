//! Multi-track mixing: weighted amix, crossfade, delay/sync.
//!
//! [`audio_mix_video`] mixes one or more standalone audio files into a
//! video's existing audio track (`amix`, video stream copied);
//! [`audio_replace`] swaps a video's audio for a single file;
//! [`video_audio_mix`] replaces each of several videos' audio with a
//! paired file and concatenates the results (video via the concat
//! demuxer, audio via the `concat` filter so heterogeneous codecs are
//! resampled safely).

use std::path::PathBuf;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::VideoInfo;
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, ext_for_format, find_tool,
    read_output, run_tool, tool_failure, write_input,
};

/// Guess an ffmpeg-friendly extension for an audio buffer from its
/// magic bytes: MP3 (`ID3` tag or MPEG frame sync `0xFF 0xE*`), WAV
/// (`RIFF`), FLAC (`fLaC`), Ogg (`OggS`), MP4/M4A (`....ftyp` at
/// offset 4). Unknown payloads get `"bin"` — ffmpeg probes content
/// anyway; the extension is only a demuxer hint.
#[allow(dead_code)] // used only by the not-yet-wired mix ops below
fn audio_ext(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"ID3")
        || (bytes.len() > 1 && bytes[0] == 0xFF && (bytes[1] & 0xE0) == 0xE0)
    {
        "mp3"
    } else if bytes.starts_with(b"RIFF") {
        "wav"
    } else if bytes.starts_with(b"fLaC") {
        "flac"
    } else if bytes.starts_with(b"OggS") {
        "ogg"
    } else if bytes.len() > 8 && &bytes[4..8] == b"ftyp" {
        "m4a"
    } else {
        "bin"
    }
}

/// Output container extension for a probed video, or
/// [`VideoError::InvalidInput`] when the container cannot be named.
#[allow(dead_code)] // used only by the not-yet-wired mix ops below
fn output_ext(info: &VideoInfo) -> VideoResult<String> {
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

/// Probe `input`, enforce `limits`, and require the named stream kind.
#[allow(dead_code)] // used only by the not-yet-wired mix ops below
fn probed_input<'a>(
    input: &'a [u8],
    limits: &EngineLimits,
    need_video: bool,
    op: &str,
) -> VideoResult<VideoInfo> {
    let len = check_input(input)?;
    let info = probe(input)?;
    check_limits(&info, len, limits, false)?;
    if need_video && !info.has_video {
        return Err(VideoError::InvalidInput(format!(
            "{op} requires a video stream; the input has none"
        )));
    }
    Ok(info)
}

/// Mix `inputs_audio` into `input_video`'s existing audio track and
/// return the encoded bytes.
///
/// The video's audio is input `[0:a]` (first), followed by each audio
/// file `[1:a]`..`[N:a]`; they are summed with
/// `amix=inputs=N+1:duration=first:dropout_transition=2:normalize=1`.
/// `duration=first` anchors the mix to the *video's* audio length, so
/// the mixed track ends with the video. `normalize=1` scales inputs to
/// avoid clipping when full-level tracks overlap.
///
/// The video stream is stream-copied (`-c:v copy`); the mixed audio is
/// encoded AAC. The output container matches the probed input
/// container. Requires at least one audio input and a video that
/// already carries an audio stream — there is nothing to anchor
/// `duration=first` to otherwise.
#[allow(dead_code)] // implemented ahead of engine wiring
pub(crate) fn audio_mix_video(
    input_video: &[u8],
    inputs_audio: &[Vec<u8>],
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    if inputs_audio.is_empty() {
        return Err(VideoError::InvalidInput(
            "audio_mix_video requires at least one audio input".to_string(),
        ));
    }
    let program = find_tool(FFMPEG)?;
    let info = probed_input(input_video, limits, true, "audio_mix_video")?;
    if !info.has_audio {
        return Err(VideoError::InvalidInput(
            "audio_mix_video requires the video to have an audio stream to mix into".to_string(),
        ));
    }
    let ext = output_ext(&info)?;

    let dir = TempDir::new("audio-mix")?;
    let video_path = write_input(&dir, &format!("video.{ext}"), input_video)?;
    let mut audio_paths: Vec<PathBuf> = Vec::with_capacity(inputs_audio.len());
    for (i, audio) in inputs_audio.iter().enumerate() {
        audio_paths.push(write_input(
            &dir,
            &format!("audio{i}.{}", audio_ext(audio)),
            audio,
        )?);
    }
    let out_path = dir.file(&format!("out.{ext}"));

    let n = inputs_audio.len() + 1;
    let labels: Vec<String> = (0..n).map(|i| format!("[{i}:a]")).collect();
    let filter = format!(
        "{}amix=inputs={n}:duration=first:dropout_transition=2:normalize=1[mixed]",
        labels.concat()
    );

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        video_path.to_string_lossy().into_owned(),
    ];
    for path in &audio_paths {
        args.push("-i".to_string());
        args.push(path.to_string_lossy().into_owned());
    }
    args.extend([
        "-filter_complex".to_string(),
        filter,
        "-map".to_string(),
        "0:v".to_string(),
        "-map".to_string(),
        "[mixed]".to_string(),
        "-c:v".to_string(),
        "copy".to_string(),
        "-c:a".to_string(),
        "aac".to_string(),
        out_path.to_string_lossy().into_owned(),
    ]);

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

/// Replace `input_video`'s audio track with `audio` and return the
/// encoded bytes.
///
/// Single ffmpeg invocation:
/// `ffmpeg -i video -i audio -map 0:v -map 1:a -c:v copy -c:a aac
/// -shortest out`. The video stream is copied untouched; the audio is
/// decoded from whatever container `audio` is and re-encoded AAC.
/// `-shortest` ends the output at the shorter of the video/audio
/// streams, so a long replacement track does not pad the result and a
/// short one trims it. Output container matches the probed input.
#[allow(dead_code)] // implemented ahead of engine wiring
pub(crate) fn audio_replace(
    input_video: &[u8],
    audio: &[u8],
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let program = find_tool(FFMPEG)?;
    let info = probed_input(input_video, limits, true, "audio_replace")?;
    check_input(audio)?;
    let ext = output_ext(&info)?;

    let dir = TempDir::new("audio-replace")?;
    let video_path = write_input(&dir, &format!("video.{ext}"), input_video)?;
    let audio_path = write_input(&dir, &format!("audio.{}", audio_ext(audio)), audio)?;
    let out_path = dir.file(&format!("out.{ext}"));

    let args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        video_path.to_string_lossy().into_owned(),
        "-i".to_string(),
        audio_path.to_string_lossy().into_owned(),
        "-map".to_string(),
        "0:v".to_string(),
        "-map".to_string(),
        "1:a".to_string(),
        "-c:v".to_string(),
        "copy".to_string(),
        "-c:a".to_string(),
        "aac".to_string(),
        "-shortest".to_string(),
        out_path.to_string_lossy().into_owned(),
    ];

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

/// Replace each pair's video audio with its paired audio file, then
/// concatenate all pairs into one file. Requires at least two pairs.
///
/// All videos must share codec, width, height and frame rate — the
/// concat demuxer copies packets, so mismatched parameters would
/// produce a corrupt stream; mismatches are rejected up front with
/// [`VideoError::InvalidInput`].
///
/// Implementation: the videos are fed to ffmpeg through the concat
/// demuxer (`-f concat -safe 0 -i vlist`, video stream copied); each
/// paired audio file is a separate input, resampled to a common
/// format (48 kHz stereo fltp) and joined with the `concat` filter —
/// this is used instead of a second concat demuxer because the demuxer
/// requires identical codec parameters across segments while paired
/// audio files may differ (wav + mp3 + ...). The result is
/// `-map 0:v -map "[acat]" -c:v copy -c:a aac`.
#[allow(dead_code)] // implemented ahead of engine wiring
pub(crate) fn video_audio_mix(
    inputs: &[(Vec<u8>, Vec<u8>)],
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    if inputs.len() < 2 {
        return Err(VideoError::InvalidInput(
            "video_audio_mix requires at least two (video, audio) pairs".to_string(),
        ));
    }
    let program = find_tool(FFMPEG)?;

    // Probe every video and enforce compatibility for packet-copy
    // concat: same codec, geometry and frame rate.
    let mut infos: Vec<VideoInfo> = Vec::with_capacity(inputs.len());
    for (video, _audio) in inputs {
        infos.push(probed_input(video, limits, true, "video_audio_mix")?);
    }
    let first = &infos[0];
    for (i, info) in infos.iter().enumerate().skip(1) {
        if info.vcodec != first.vcodec
            || info.width != first.width
            || info.height != first.height
            || info.fps != first.fps
        {
            return Err(VideoError::InvalidInput(format!(
                "video_audio_mix pair {i} is not concat-compatible with pair 0 \
                 (codec {:?} vs {:?}, {}x{} vs {}x{}, fps {:?} vs {:?})",
                info.vcodec,
                first.vcodec,
                info.width.unwrap_or(0),
                info.height.unwrap_or(0),
                first.width.unwrap_or(0),
                first.height.unwrap_or(0),
                info.fps,
                first.fps,
            )));
        }
    }
    let ext = output_ext(first)?;
    if ext == "webm" && !crate::crates::video_engine::ops::encoder_available("libopus") {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "webm output requires the libopus audio encoder, \
                     which this ffmpeg build does not provide"
                .to_string(),
        });
    }

    let dir = TempDir::new("video-audio-mix")?;

    // Concat-demuxer list for the video files.
    let mut vlist = String::new();
    let mut audio_paths: Vec<PathBuf> = Vec::with_capacity(inputs.len());
    for (i, (video, audio)) in inputs.iter().enumerate() {
        check_input(audio)?;
        let vext = infos[i]
            .format_name
            .as_deref()
            .and_then(ext_for_format)
            .unwrap_or("bin");
        let vpath = write_input(&dir, &format!("v{i}.{vext}"), video)?;
        vlist.push_str(&format!("file '{}'\n", vpath.display()));
        audio_paths.push(write_input(
            &dir,
            &format!("a{i}.{}", audio_ext(audio)),
            audio,
        )?);
    }
    let vlist_path = dir.file("vlist.txt");
    std::fs::write(&vlist_path, vlist)?;
    let out_path = dir.file(&format!("out.{ext}"));

    // Audio filtergraph: each paired file → common format → concat.
    // Audio inputs start at index 1 (input 0 is the video concat list).
    let n = inputs.len();
    let mut filter_parts: Vec<String> = Vec::with_capacity(n + 1);
    let mut labels: Vec<String> = Vec::with_capacity(n);
    for i in 0..n {
        let idx = i + 1;
        let label = format!("[a{i}]");
        labels.push(label.clone());
        filter_parts.push(format!(
            "[{idx}:a]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo{label}"
        ));
    }
    filter_parts.push(format!("{}concat=n={n}:v=0:a=1[acat]", labels.concat()));
    let filter = filter_parts.join(";");

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-f".to_string(),
        "concat".to_string(),
        "-safe".to_string(),
        "0".to_string(),
        "-i".to_string(),
        vlist_path.to_string_lossy().into_owned(),
    ];
    for path in &audio_paths {
        args.push("-i".to_string());
        args.push(path.to_string_lossy().into_owned());
    }
    args.extend([
        "-filter_complex".to_string(),
        filter,
        "-map".to_string(),
        "0:v".to_string(),
        "-map".to_string(),
        "[acat]".to_string(),
        "-c:v".to_string(),
        "copy".to_string(),
        "-c:a".to_string(),
        if ext == "webm" { "libopus" } else { "aac" }.to_string(),
        out_path.to_string_lossy().into_owned(),
    ]);

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

/// Mix `inputs` with per-track `weights` (linear gain multipliers,
/// must match `inputs.len()`). `normalize` applies `amix`'s
/// auto-leveling (`normalize=1`).
pub(crate) fn audio_mix_weighted(
    inputs: &[Vec<u8>],
    weights: &[f64],
    normalize: bool,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (inputs, weights, normalize, limits);
    Err(
        crate::crates::video_engine::error::VideoError::InvalidInput(
            "audio_mix_weighted not implemented".to_string(),
        ),
    )
}

/// Crossfade between two inputs over `duration_secs` (`acrossfade`).
/// Output length ≈ sum of inputs minus the overlap.
pub(crate) fn audio_crossfade(
    inputs: &[Vec<u8>],
    duration_secs: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (inputs, duration_secs, limits);
    Err(
        crate::crates::video_engine::error::VideoError::InvalidInput(
            "audio_crossfade not implemented".to_string(),
        ),
    )
}

/// Shift the audio track by `offset_ms` (positive = later; uses
/// `adelay`/`atrim`+`asetpts` as needed, padding silence at the head).
pub(crate) fn audio_delay(
    input: &[u8],
    offset_ms: i64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, offset_ms, limits);
    Err(
        crate::crates::video_engine::error::VideoError::InvalidInput(
            "audio_delay not implemented".to_string(),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, encoder_available, find_tool};
    use std::fs;

    fn tools_available() -> bool {
        find_tool(FFMPEG).is_ok() && find_tool(FFPROBE).is_ok()
    }

    /// `dur`-second mono sine WAV (pcm_s16le).
    fn make_wav(freq: u32, dur: f64) -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let dir = TempDir::new("am-test-wav").ok()?;
        let out = dir.file("tone.wav");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            format!("sine=frequency={freq}:duration={dur}"),
            "-c:a".to_string(),
            "pcm_s16le".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    /// `dur`-second mono MP3 via lavfi; `None` when libmp3lame is absent.
    fn make_mp3(freq: u32, dur: f64) -> Option<Vec<u8>> {
        if !encoder_available("libmp3lame") {
            return None;
        }
        let program = find_tool(FFMPEG).ok()?;
        let dir = TempDir::new("am-test-mp3").ok()?;
        let out = dir.file("tone.mp3");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            format!("sine=frequency={freq}:duration={dur}"),
            "-c:a".to_string(),
            "libmp3lame".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    /// `dur`-second A/V clip: testsrc video + sine audio, H.264 + AAC.
    /// `rate`/`size`/`freq` vary per fixture for the incompatibility test.
    fn make_av(size: &str, rate: u32, dur: f64, freq: u32) -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let enc = crate::crates::video_engine::ops::h264_encoder()?;
        let dir = TempDir::new("am-test-av").ok()?;
        let out = dir.file("clip.mp4");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            format!("testsrc=size={size}:rate={rate}:duration={dur}"),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            format!("sine=frequency={freq}:duration={dur}"),
            "-c:v".to_string(),
            enc.to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-c:a".to_string(),
            "aac".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    fn duration_of(bytes: &[u8]) -> Option<f64> {
        probe(bytes).ok().and_then(|i| i.duration_secs)
    }

    fn has_audio(bytes: &[u8]) -> bool {
        probe(bytes).map(|i| i.has_audio).unwrap_or(false)
    }

    fn assert_duration_near(actual: f64, expected: f64, what: &str) {
        assert!(
            (actual - expected).abs() <= 0.3,
            "{what}: expected ~{expected}s, got {actual}s"
        );
    }

    #[test]
    fn mix_video_with_wav_and_mp3_keeps_video_duration() {
        if !tools_available() {
            return;
        }
        let Some(video) = make_av("160x120", 10, 1.0, 440) else {
            return;
        };
        let Some(wav) = make_wav(550, 1.5) else {
            return;
        };
        // MP3 is optional (libmp3lame); mix degrades gracefully to wav only.
        let mut audios = vec![wav];
        if let Some(mp3) = make_mp3(660, 1.5) {
            audios.push(mp3);
        }
        let out = audio_mix_video(&video, &audios, &EngineLimits::default())
            .expect("audio_mix_video failed");
        assert!(has_audio(&out), "mixed output must have an audio stream");
        let dur = duration_of(&out).expect("probe mixed output");
        assert_duration_near(dur, 1.0, "audio_mix_video");
    }

    #[test]
    fn mix_rejects_video_without_audio() {
        if !tools_available() {
            return;
        }
        // Video-only clip: testsrc, no audio input.
        let program = find_tool(FFMPEG).ok().unwrap();
        let dir = TempDir::new("am-test-vonly").ok().unwrap();
        let out = dir.file("vonly.mp4");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc=size=160x120:rate=10:duration=0.5".to_string(),
            "-c:v".to_string(),
            "libopenh264".to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-an".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)
            .ok()
            .unwrap();
        if !output.status.success() {
            return;
        }
        let video = fs::read(out).unwrap();
        let Some(wav) = make_wav(440, 0.5) else {
            return;
        };
        assert!(matches!(
            audio_mix_video(&video, &[wav], &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn replace_swaps_audio_and_trims_to_shorter() {
        if !tools_available() {
            return;
        }
        let Some(video) = make_av("160x120", 10, 1.0, 440) else {
            return;
        };
        // Replacement audio shorter than the video → output ≈ audio length.
        let Some(short_wav) = make_wav(550, 0.5) else {
            return;
        };
        let out = audio_replace(&video, &short_wav, &EngineLimits::default())
            .expect("audio_replace failed");
        let dur = duration_of(&out).expect("probe replaced output");
        assert_duration_near(dur, 0.5, "audio_replace(short audio)");

        // Replacement audio longer than the video → output ≈ video length.
        let Some(long_mp3) = make_mp3(660, 2.0) else {
            return;
        };
        let out = audio_replace(&video, &long_mp3, &EngineLimits::default())
            .expect("audio_replace mp3 failed");
        let dur = duration_of(&out).expect("probe replaced mp3 output");
        assert_duration_near(dur, 1.0, "audio_replace(long mp3)");
    }

    #[test]
    fn video_audio_mix_two_pairs_concatenates() {
        if !tools_available() {
            return;
        }
        // Two compatible clips (same codec/geometry/rate) with different
        // audio codecs to exercise the resampling concat filter.
        let Some(v1) = make_av("160x120", 10, 0.8, 440) else {
            return;
        };
        let Some(v2) = make_av("160x120", 10, 0.8, 550) else {
            return;
        };
        let Some(a1) = make_wav(330, 0.8) else {
            return;
        };
        let Some(a2) = make_mp3(770, 0.8) else {
            // Fall back to a second wav when libmp3lame is absent.
            let Some(a2) = make_wav(770, 0.8) else {
                return;
            };
            let pairs = vec![(v1, a1), (v2, a2)];
            let out =
                video_audio_mix(&pairs, &EngineLimits::default()).expect("video_audio_mix failed");
            let dur = duration_of(&out).expect("probe mixed output");
            assert_duration_near(dur, 1.6, "video_audio_mix");
            return;
        };
        let pairs = vec![(v1, a1), (v2, a2)];
        let out =
            video_audio_mix(&pairs, &EngineLimits::default()).expect("video_audio_mix failed");
        assert!(has_audio(&out));
        let dur = duration_of(&out).expect("probe mixed output");
        assert_duration_near(dur, 1.6, "video_audio_mix");
    }

    #[test]
    fn video_audio_mix_rejects_incompatible_videos() {
        if !tools_available() {
            return;
        }
        let Some(v1) = make_av("160x120", 10, 0.5, 440) else {
            return;
        };
        let Some(v2) = make_av("320x240", 10, 0.5, 550) else {
            return;
        };
        let Some(a1) = make_wav(330, 0.5) else {
            return;
        };
        let Some(a2) = make_wav(770, 0.5) else {
            return;
        };
        let pairs = vec![(v1, a1), (v2, a2)];
        assert!(matches!(
            video_audio_mix(&pairs, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn video_audio_mix_requires_two_pairs() {
        let pairs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        assert!(matches!(
            video_audio_mix(&pairs, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }
}
