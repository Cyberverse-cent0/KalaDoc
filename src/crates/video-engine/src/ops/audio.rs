//! Basic audio operations on media containers: volume, loudness
//! normalization, fades, channel layout, merging/concatenating audio
//! streams, and audio removal.
//!
//! Two input shapes are handled uniformly:
//!
//! - **A/V containers** (`info.has_video`): the video stream is
//!   stream-copied (`-c:v copy`), only video+audio streams are mapped
//!   (subtitles/data/attachments are dropped), and the audio is
//!   re-encoded into the *same container family* — the output
//!   extension comes from `ext_for_format`, and container-family
//!   outputs always carry AAC.
//! - **Audio-only inputs**: the output goes to a dedicated audio
//!   container picked from the probed `format_name` tokens
//!   (`flac`, `m4a`, `ogg`, `wav`, `mp3`, `aac`, `opus`) with
//!   `ext_for_format` as fallback; the codec/muxer pair comes from
//!   [`audio_out_args`].

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::VideoInfo;
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, checked_ext, encoder_available,
    ext_for_format, find_tool, read_output, run_tool, tool_failure, write_input,
};

/// Dedicated audio containers `audio_concat` accepts as a target.
pub(crate) const AUDIO_CONCAT_TARGETS: &[&str] =
    &["mp3", "wav", "flac", "ogg", "opus", "m4a", "aac"];

/// Audio container/codec targets this module can emit.
const AUDIO_EXTS: &[&str] = &["mp3", "wav", "flac", "ogg", "opus", "m4a", "aac"];

/// Map an output extension to `(audio codec, explicit ffmpeg muxer
/// name)`. `None` muxer means "let ffmpeg infer it from the output
/// path" — used for container-family outputs (mp4/mkv/ts/...) where
/// the extension already selects the muxer and AAC is the safe audio
/// codec. `webm` is special-cased to Opus because the WebM muxer
/// rejects AAC.
fn audio_out_args(ext: &str) -> (&'static str, Option<&'static str>) {
    match ext {
        "mp3" => ("libmp3lame", Some("mp3")),
        "wav" => ("pcm_s16le", Some("wav")),
        "flac" => ("flac", Some("flac")),
        "ogg" => ("libvorbis", Some("ogg")),
        "opus" => ("libopus", Some("ogg")),
        "m4a" => ("aac", Some("ipod")),
        "aac" => ("aac", Some("adts")),
        "webm" => ("libopus", None),
        _ => ("aac", None),
    }
}

/// Fail early with an honest [`VideoError::ExternalTool`] when the
/// ffmpeg build lacks the encoder `audio_out_args` selected.
fn require_encoder(codec: &str) -> VideoResult<()> {
    if encoder_available(codec) {
        Ok(())
    } else {
        Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: format!(
                "audio output requires the {codec} encoder, which this ffmpeg build does not provide"
            ),
        })
    }
}

/// `-c:a codec` (+ `-f muxer` when the codec needs an explicit
/// container) argument tail for `ext`.
fn codec_args(ext: &str) -> VideoResult<Vec<String>> {
    let (codec, muxer) = audio_out_args(ext);
    require_encoder(codec)?;
    let mut args = vec!["-c:a".to_string(), codec.to_string()];
    if let Some(muxer) = muxer {
        args.push("-f".to_string());
        args.push(muxer.to_string());
    }
    Ok(args)
}

/// Pick a sniffable extension for a *temp input* file so the right
/// demuxer is selected: dedicated audio exts named inside
/// `format_name` win, then `ext_for_format`, then `"bin"`.
fn input_file_ext(info: &VideoInfo) -> String {
    if let Some(name) = info.format_name.as_deref() {
        for token in name.to_ascii_lowercase().split(',') {
            let token = token.trim();
            if AUDIO_EXTS.contains(&token) {
                return token.to_string();
            }
        }
    }
    info.format_name
        .as_deref()
        .and_then(ext_for_format)
        .unwrap_or("bin")
        .to_string()
}

/// Pick the output container for an **audio-only** input: prefer a
/// dedicated audio extension named by the probed `format_name` tokens
/// (covers `flac`, `aac`, `m4a`-family demuxers that `ext_for_format`
/// does not map), then fall back to `ext_for_format` — a container
/// family like `mp4`/`mkv` still works because `audio_out_args`
/// encodes AAC without an explicit `-f`.
fn audio_only_ext(info: &VideoInfo, op: &str) -> VideoResult<String> {
    if let Some(name) = info.format_name.as_deref() {
        for token in name.to_ascii_lowercase().split(',') {
            let token = token.trim();
            if AUDIO_EXTS.contains(&token) {
                return Ok(token.to_string());
            }
        }
    }
    info.format_name
        .as_deref()
        .and_then(ext_for_format)
        .map(str::to_string)
        .ok_or_else(|| {
            VideoError::InvalidInput(format!(
                "{op}: cannot determine an audio output container from format_name {:?}",
                info.format_name
            ))
        })
}

/// Shared runner for the `-af`/`-ac` style ops (volume, normalize,
/// fade, channel_convert). Probes once, enforces limits, requires an
/// audio stream, then either stream-copies video (A/V input) or emits
/// an audio-only container.
fn run_audio_op(
    input: &[u8],
    limits: &EngineLimits,
    op: &str,
    af: Option<String>,
    extra_audio_opts: &[String],
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    let program = find_tool(FFMPEG)?;

    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;
    if !info.has_audio {
        return Err(VideoError::InvalidInput(format!(
            "{op} requires an input with at least one audio stream"
        )));
    }

    let dir = TempDir::new(op)?;
    let input_path = write_input(&dir, "input.bin", input)?;

    // Output extension + codec args differ by input shape.
    let (ext, mut codec) = if info.has_video {
        let ext = info
            .format_name
            .as_deref()
            .and_then(ext_for_format)
            .ok_or_else(|| {
                VideoError::InvalidInput(format!(
                    "{op}: cannot determine output container from probed format_name"
                ))
            })?
            .to_string();
        (ext.clone(), codec_args(&ext)?)
    } else {
        let ext = audio_only_ext(&info, op)?;
        (ext.clone(), codec_args(&ext)?)
    };
    let _ = ext;

    let out_path = dir.file(&format!("out.{ext}"));

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
    ];
    if info.has_video {
        // Keep video (and attached pictures) as a lossless copy; map
        // only video+audio so subtitles/data streams are dropped.
        args.extend(["-map".to_string(), "0:v".to_string()]);
        args.extend(["-map".to_string(), "0:a".to_string()]);
        args.extend(["-c:v".to_string(), "copy".to_string()]);
    } else {
        args.extend(["-map".to_string(), "0:a".to_string()]);
        args.push("-vn".to_string());
    }
    args.append(&mut codec);
    args.extend(extra_audio_opts.iter().cloned());
    if let Some(af) = af {
        args.extend(["-af".to_string(), af]);
    }
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

/// Apply a gain change in dB to all audio streams (`volume` filter).
/// Negative values attenuate; A/V inputs keep their container and
/// stream-copy the video, audio-only inputs stay in their audio
/// container family.
pub(crate) fn audio_volume(
    input: &[u8],
    gain_db: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    if !gain_db.is_finite() {
        return Err(VideoError::InvalidInput(format!(
            "gain_db must be finite, got {gain_db}"
        )));
    }
    run_audio_op(
        input,
        limits,
        "audio_volume",
        Some(format!("volume={gain_db}dB")),
        &[],
    )
}

/// Normalize loudness toward the EBU R128 target (-23 LUFS
/// integrated by default, `TP=-1.5`, `LRA=11`) using single-pass
/// `loudnorm` linear normalization.
pub(crate) fn audio_normalize(
    input: &[u8],
    target_lufs: Option<f64>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let target = target_lufs.unwrap_or(-23.0);
    if !target.is_finite() {
        return Err(VideoError::InvalidInput(format!(
            "target_lufs must be finite, got {target}"
        )));
    }
    run_audio_op(
        input,
        limits,
        "audio_normalize",
        Some(format!("loudnorm=I={target:.2}:TP=-1.5:LRA=11")),
        &[],
    )
}

/// Mix `inputs` (audio files or containers with audio) into one WAV
/// via the `amix` filter (`duration=longest`, `normalize=0` so input
/// levels are preserved rather than auto-scaled). Requires at least
/// two inputs; every input is probed and limit-checked up front.
pub(crate) fn audio_merge(inputs: &[Vec<u8>], limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    if inputs.len() < 2 {
        return Err(VideoError::InvalidInput(
            "audio_merge requires at least two inputs".to_string(),
        ));
    }
    let program = find_tool(FFMPEG)?;

    // Probe every input up front: limits apply per input, `has_audio`
    // is required, and the probed format picks each temp file's
    // sniffable extension.
    let mut infos = Vec::with_capacity(inputs.len());
    for input in inputs {
        let len = check_input(input)?;
        let info = probe(input)?;
        check_limits(&info, len, limits, false)?;
        if !info.has_audio {
            return Err(VideoError::InvalidInput(
                "audio_merge requires every input to have an audio stream".to_string(),
            ));
        }
        infos.push(info);
    }

    let dir = TempDir::new("audio-merge")?;
    let mut args = vec!["-y".to_string(), "-v".to_string(), "error".to_string()];
    for (idx, input) in inputs.iter().enumerate() {
        let ext = input_file_ext(&infos[idx]);
        let path = write_input(&dir, &format!("in{idx}.{ext}"), input)?;
        args.extend(["-i".to_string(), path.to_string_lossy().into_owned()]);
    }

    // [0:a][1:a]...[N-1:a]amix=inputs=N:duration=longest:normalize=0[a]
    let mut filter = String::new();
    for i in 0..inputs.len() {
        filter.push_str(&format!("[{i}:a]"));
    }
    filter.push_str(&format!(
        "amix=inputs={}:duration=longest:normalize=0[a]",
        inputs.len()
    ));

    let out_path = dir.file("out.wav");
    args.extend([
        "-filter_complex".to_string(),
        filter,
        "-map".to_string(),
        "[a]".to_string(),
    ]);
    args.extend(codec_args("wav")?);
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

/// Concatenate `inputs` end-to-end into `target_ext` bytes. Uses the
/// concat demuxer: `-c copy` when every input reports the same
/// `format_name` and audio codec *and* the probed input container
/// already matches `target_ext`; otherwise audio is re-encoded to
/// the target codec. Video/subtitle/data streams are always dropped —
/// the output is an audio-only file.
pub(crate) fn audio_concat(
    inputs: &[Vec<u8>],
    target_ext: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    if inputs.is_empty() {
        return Err(VideoError::InvalidInput(
            "audio_concat requires at least one input".to_string(),
        ));
    }
    let ext = checked_ext(target_ext, AUDIO_CONCAT_TARGETS, "audio_concat")?;
    let program = find_tool(FFMPEG)?;

    // Probe every input up front: limits per input, plus the
    // container/codec comparison that decides copy vs re-encode.
    let mut infos: Vec<VideoInfo> = Vec::with_capacity(inputs.len());
    for input in inputs {
        let len = check_input(input)?;
        let info = probe(input)?;
        check_limits(&info, len, limits, false)?;
        infos.push(info);
    }
    let first = &infos[0];
    let same_format = infos.iter().all(|i| {
        i.format_name == first.format_name
            && i.acodec == first.acodec
            && i.has_video == first.has_video
    });
    // Stream copy is only honest when the target container is the
    // same audio family the inputs already are — otherwise the codec
    // would have to cross container families.
    let copy_ok = same_format
        && audio_only_ext(first, "audio_concat")
            .map(|e| e == ext)
            .unwrap_or(false);

    let dir = TempDir::new("audio-concat")?;

    // Write inputs with sniffable extensions so the concat demuxer
    // picks the right demuxer for each file (temp paths are generated
    // and never contain quotes — single-quoting is safe).
    let mut list = String::new();
    for (idx, input) in inputs.iter().enumerate() {
        let in_ext = input_file_ext(&infos[idx]);
        let path = write_input(&dir, &format!("in{idx}.{in_ext}"), input)?;
        list.push_str(&format!("file '{}'\n", path.display()));
    }
    let list_path = dir.file("list.txt");
    std::fs::write(&list_path, list)?;

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
        // Audio-only output: drop video/subtitle/data streams.
        "-vn".to_string(),
        "-sn".to_string(),
        "-dn".to_string(),
    ];
    if copy_ok {
        args.extend(["-c".to_string(), "copy".to_string()]);
    } else {
        args.extend(codec_args(&ext)?);
    }
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

/// Apply fade-in over `fade_in_secs` and fade-out over
/// `fade_out_secs` via the `afade` filter; `0` disables that side.
/// Fade-out start is `max(0, duration - fade_out)`; the probed
/// duration is required when a fade-out is requested.
pub(crate) fn audio_fade(
    input: &[u8],
    fade_in_secs: f64,
    fade_out_secs: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    for (name, value) in [
        ("fade_in_secs", fade_in_secs),
        ("fade_out_secs", fade_out_secs),
    ] {
        if !value.is_finite() || value < 0.0 {
            return Err(VideoError::InvalidInput(format!(
                "{name} must be a finite non-negative number, got {value}"
            )));
        }
    }
    if fade_in_secs == 0.0 && fade_out_secs == 0.0 {
        return Err(VideoError::InvalidInput(
            "audio_fade requires at least one non-zero fade duration".to_string(),
        ));
    }

    // Probe early for the fade-out start time (the shared runner
    // probes again — cheap relative to the encode).
    let input_len = check_input(input)?;
    let _ = input_len;
    let info = probe(input)?;
    let mut filters = Vec::new();
    if fade_in_secs > 0.0 {
        filters.push(format!("afade=t=in:st=0:d={fade_in_secs:.3}"));
    }
    if fade_out_secs > 0.0 {
        let duration = info.duration_secs.ok_or_else(|| {
            VideoError::InvalidInput(
                "audio_fade with fade_out_secs requires a probed duration".to_string(),
            )
        })?;
        let start = (duration - fade_out_secs).max(0.0);
        filters.push(format!("afade=t=out:st={start:.3}:d={fade_out_secs:.3}"));
    }

    run_audio_op(input, limits, "audio_fade", Some(filters.join(",")), &[])
}

/// Strip all audio streams, keeping video/subtitle streams; output
/// container follows the input. Audio-only inputs are rejected —
/// there is nothing to remove.
pub(crate) fn remove_audio(input: &[u8], limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    let program = find_tool(FFMPEG)?;

    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;
    if !info.has_video {
        return Err(VideoError::InvalidInput(
            "remove_audio requires an input with a video stream".to_string(),
        ));
    }
    let ext = info
        .format_name
        .as_deref()
        .and_then(ext_for_format)
        .ok_or_else(|| {
            VideoError::InvalidInput(
                "remove_audio: cannot determine output container from probed format_name"
                    .to_string(),
            )
        })?
        .to_string();

    let dir = TempDir::new("remove-audio")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file(&format!("out.{ext}"));

    let args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        // Map everything, then subtract the audio streams.
        "-map".to_string(),
        "0".to_string(),
        "-map".to_string(),
        "-0:a".to_string(),
        "-c".to_string(),
        "copy".to_string(),
        out_path.to_string_lossy().into_owned(),
    ];

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

/// Convert channel layout via `-ac {channels}` (ffmpeg picks the
/// matching channel layout for the count: 1=mono, 2=stereo, 6=5.1,
/// 8=7.1). `channels` must be in `1..=64`.
pub(crate) fn channel_convert(
    input: &[u8],
    channels: u32,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    if !(1..=64).contains(&channels) {
        return Err(VideoError::InvalidInput(format!(
            "channels must be in 1..=64, got {channels}"
        )));
    }
    run_audio_op(
        input,
        limits,
        "channel_convert",
        None,
        &["-ac".to_string(), channels.to_string()],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::probe::probe;
    use crate::crates::video_engine::ops::tool_available;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

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
                "kaladoc-audio-test-{tag}-{}-{nanos}-{seq}",
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

    fn ffmpeg_available() -> bool {
        tool_available(FFMPEG) && tool_available("ffprobe")
    }

    /// Run ffmpeg to produce a fixture file; returns its bytes.
    /// `extra` args go before the output path (e.g. `-c:a pcm_s16le`).
    fn run_fixture(dir: &TestDir, name: &str, args: Vec<String>) -> Vec<u8> {
        let out = dir.0.join(name);
        let status = Command::new(FFMPEG)
            .args(args)
            .arg(&out)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .status()
            .expect("failed to spawn ffmpeg");
        assert!(status.success(), "ffmpeg fixture generation failed");
        fs::read(&out).unwrap()
    }

    /// Audio-only WAV: sine → pcm_s16le. `stereo` upmixes to 2ch.
    fn make_wav(dir: &TestDir, name: &str, secs: f64, freq: u32, stereo: bool) -> Vec<u8> {
        let mut args = vec![
            "-y".into(),
            "-v".into(),
            "error".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            format!("sine=frequency={freq}:duration={secs}"),
            "-c:a".into(),
            "pcm_s16le".into(),
        ];
        if stereo {
            args.extend(["-ac".into(), "2".into()]);
        }
        run_fixture(dir, name, args)
    }

    /// Audio-only MP3: sine → libmp3lame.
    fn make_mp3(dir: &TestDir, name: &str, secs: f64, freq: u32) -> Vec<u8> {
        run_fixture(
            dir,
            name,
            vec![
                "-y".into(),
                "-v".into(),
                "error".into(),
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                format!("sine=frequency={freq}:duration={secs}"),
                "-c:a".into(),
                "libmp3lame".into(),
            ],
        )
    }

    /// A/V MP4: testsrc video + sine audio, H.264 (libopenh264) + AAC.
    fn make_av(dir: &TestDir, name: &str, secs: f64) -> Vec<u8> {
        run_fixture(
            dir,
            name,
            vec![
                "-y".into(),
                "-v".into(),
                "error".into(),
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                format!("testsrc=duration={secs}:size=160x120:rate=10"),
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                format!("sine=frequency=440:duration={secs}"),
                "-c:v".into(),
                "libopenh264".into(),
                "-pix_fmt".into(),
                "yuv420p".into(),
                "-c:a".into(),
                "aac".into(),
                "-shortest".into(),
            ],
        )
    }

    fn duration_of(bytes: &[u8]) -> f64 {
        probe(bytes)
            .expect("probe output")
            .duration_secs
            .expect("output duration missing")
    }

    fn audio_channels(bytes: &[u8]) -> Option<u32> {
        probe(bytes)
            .expect("probe output")
            .streams
            .iter()
            .find(|s| s.codec_type == "audio")
            .and_then(|s| s.channels)
    }

    #[test]
    fn volume_wav_keeps_duration() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TestDir::new("volume");
        let wav = make_wav(&dir, "in.wav", 1.0, 440, false);

        let out = audio_volume(&wav, -6.0, &EngineLimits::default()).unwrap();
        assert!(!out.is_empty());
        let dur = duration_of(&out);
        assert!((dur - 1.0).abs() < 0.2, "duration {dur}");
    }

    #[test]
    fn volume_av_mp4_copies_video() {
        if !ffmpeg_available() || !encoder_available("libopenh264") {
            return;
        }
        let dir = TestDir::new("volume-av");
        let av = make_av(&dir, "in.mp4", 1.0);

        let out = audio_volume(&av, 3.0, &EngineLimits::default()).unwrap();
        let info = probe(&out).unwrap();
        assert!(info.has_video, "video stream must be preserved");
        assert!(info.has_audio, "audio stream must be preserved");
        let dur = info.duration_secs.expect("duration missing");
        assert!((dur - 1.0).abs() < 0.3, "duration {dur}");
    }

    #[test]
    fn normalize_wav_produces_output() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TestDir::new("normalize");
        let wav = make_wav(&dir, "in.wav", 1.0, 440, false);

        let out = audio_normalize(&wav, None, &EngineLimits::default()).unwrap();
        assert!(!out.is_empty());
        let dur = duration_of(&out);
        assert!(dur > 0.5, "duration {dur}");
    }

    #[test]
    fn merge_two_wavs_uses_longest_duration() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TestDir::new("merge");
        // amix duration=longest → output spans the longer input,
        // not the sum (inputs play simultaneously).
        let w1 = make_wav(&dir, "a.wav", 0.4, 440, false);
        let w2 = make_wav(&dir, "b.wav", 0.6, 880, false);

        let out = audio_merge(&[w1, w2], &EngineLimits::default()).unwrap();
        assert!(!out.is_empty());
        let dur = duration_of(&out);
        assert!((dur - 0.6).abs() < 0.3, "duration {dur}, expected ~0.6s");
    }

    #[test]
    fn concat_two_mp3s_sums_durations() {
        if !ffmpeg_available() || !encoder_available("libmp3lame") {
            return;
        }
        let dir = TestDir::new("concat");
        let m1 = make_mp3(&dir, "a.mp3", 0.5, 440);
        let m2 = make_mp3(&dir, "b.mp3", 0.5, 880);

        let out = audio_concat(&[m1, m2], "mp3", &EngineLimits::default()).unwrap();
        assert!(!out.is_empty());
        let dur = duration_of(&out);
        assert!((dur - 1.0).abs() < 0.4, "duration {dur}, expected ~1.0s");
    }

    #[test]
    fn fade_wav_keeps_duration() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TestDir::new("fade");
        let wav = make_wav(&dir, "in.wav", 1.0, 440, false);

        let out = audio_fade(&wav, 0.2, 0.3, &EngineLimits::default()).unwrap();
        assert!(!out.is_empty());
        let dur = duration_of(&out);
        assert!((dur - 1.0).abs() < 0.2, "duration {dur}");
    }

    #[test]
    fn remove_audio_drops_audio_keeps_video() {
        if !ffmpeg_available() || !encoder_available("libopenh264") {
            return;
        }
        let dir = TestDir::new("remove-audio");
        let av = make_av(&dir, "in.mp4", 1.0);

        let out = remove_audio(&av, &EngineLimits::default()).unwrap();
        let info = probe(&out).unwrap();
        assert!(info.has_video, "video stream must remain");
        assert!(!info.has_audio, "audio stream must be removed");
    }

    #[test]
    fn channel_convert_stereo_to_mono() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TestDir::new("channels");
        let wav = make_wav(&dir, "stereo.wav", 1.0, 440, true);
        assert_eq!(audio_channels(&wav), Some(2));

        let out = channel_convert(&wav, 1, &EngineLimits::default()).unwrap();
        assert!(!out.is_empty());
        assert_eq!(audio_channels(&out), Some(1));
    }

    #[test]
    fn merge_rejects_single_input() {
        assert!(matches!(
            audio_merge(&[vec![1, 2, 3]], &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn remove_audio_rejects_audio_only() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TestDir::new("remove-audio-only");
        let wav = make_wav(&dir, "in.wav", 0.5, 440, false);
        assert!(matches!(
            remove_audio(&wav, &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }
}
