//! Advanced audio effects: EQ, denoise, compression, pitch shift,
//! and spectrogram image generation.
//!
//! Filter availability was verified against ffmpeg 7.1.5
//! (`ffmpeg -filters`): `firequalizer`, `equalizer`, `afftdn`,
//! `anlmdn`, `acompressor`, `asetrate`, `aresample`, `atempo` and
//! `showspectrumpic` are all present. Each op probes `ffmpeg -filters`
//! anyway and degrades honestly when a filter is missing
//! ([`filter_available`]).
//!
//! Every op accepts audio-only *or* A/V input. A/V input keeps its
//! video via `-c:v copy`; the output container follows the probed
//! `format_name` via [`ext_for_format`], falling back to `wav` +
//! `pcm_s16le` when the container cannot be named or mapped — the same
//! convention as `audio_time.rs`.

use std::path::Path;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::VideoInfo;
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, PROBE_TIMEOUT, TempDir, check_input, check_limits, encoder_available,
    ext_for_format, find_tool, read_output, run_tool, tool_failure, write_input,
};

/// Upper bound for spectrogram dimensions — `showspectrumpic` renders
/// one frame, but an absurd size is a decompression-bomb vector.
const MAX_SPECTROGRAM_DIM: u32 = 8192;

/// Probe + limit-check the input and require an audio stream.
fn checked_input(input: &[u8], limits: &EngineLimits) -> VideoResult<VideoInfo> {
    let len = check_input(input)?;
    let info = probe(input)?;
    check_limits(&info, len, limits, false)?;
    if !info.has_audio {
        return Err(VideoError::InvalidInput(
            "input has no audio stream".to_string(),
        ));
    }
    Ok(info)
}

/// Whether `ffmpeg -filters` lists `name`. Cached for the process
/// lifetime; a failed listing yields `false` so callers surface an
/// honest error instead of a mid-run filter parse failure.
fn filter_available(name: &str) -> bool {
    use std::sync::OnceLock;
    static FILTERS: OnceLock<Vec<String>> = OnceLock::new();
    let list = FILTERS.get_or_init(|| {
        let Ok(program) = find_tool(FFMPEG) else {
            return Vec::new();
        };
        let args = vec!["-hide_banner".to_string(), "-filters".to_string()];
        match run_tool(FFMPEG, &program, &args, PROBE_TIMEOUT) {
            Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.split_whitespace().nth(1))
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        }
    });
    list.iter().any(|f| f == name)
}

/// Codec args + required encoder for *container-preserving* outputs.
/// `ext` is an [`ext_for_format`] result; unmapped extensions yield
/// `None` so the caller falls back to `wav`.
fn container_codec(ext: &str) -> Option<(&'static str, Option<&'static str>)> {
    // (codec name, required encoder) — encoder `None` means the codec
    // is native to ffmpeg and assumed present.
    Some(match ext {
        "mp4" | "mkv" | "ts" => ("aac", None),
        "webm" => ("libopus", Some("libopus")),
        "mpg" | "wmv" => ("libmp3lame", Some("libmp3lame")),
        "ogg" => ("libvorbis", Some("libvorbis")),
        "mp3" => ("libmp3lame", Some("libmp3lame")),
        "wav" => ("pcm_s16le", None),
        _ => return None,
    })
}

/// Whether `ext` names a pure audio container (video must be dropped
/// with `-vn`). A/V-capable containers get `-c:v copy` instead.
fn is_audio_only_ext(ext: &str) -> bool {
    matches!(ext, "mp3" | "wav" | "ogg" | "flac" | "opus" | "aac" | "m4a")
}

/// Output-side args shared by every op: drop video for audio-only
/// containers, stream-copy it otherwise, then apply the audio codec.
fn output_args(ext: &str, codec: &str) -> Vec<String> {
    let mut args = Vec::with_capacity(6);
    if is_audio_only_ext(ext) {
        args.push("-vn".to_string());
    } else {
        args.extend(["-c:v".to_string(), "copy".to_string()]);
    }
    args.extend(["-c:a".to_string(), codec.to_string()]);
    args
}

/// Resolve the output container for the container-preserving ops:
/// `(ext, codec)` where `codec` is the ffmpeg `-c:a` codec name.
/// Falls back to `wav`/`pcm_s16le` when the input container cannot be
/// named or mapped to an audio codec.
fn container_target(info: &VideoInfo) -> (String, String) {
    let ext = info
        .format_name
        .as_deref()
        .and_then(ext_for_format)
        .unwrap_or("wav");
    match container_codec(ext) {
        Some((codec, required)) => {
            if let Some(enc) = required {
                if !encoder_available(enc) {
                    // Codec exists but encoder missing — degrade to wav
                    // rather than fail, since this is a convenience path.
                    return ("wav".to_string(), "pcm_s16le".to_string());
                }
            }
            (ext.to_string(), codec.to_string())
        }
        None => ("wav".to_string(), "pcm_s16le".to_string()),
    }
}

/// Run ffmpeg and map a non-zero exit to [`VideoError::ExternalTool`].
fn run_ffmpeg(program: &Path, args: &[String]) -> VideoResult<()> {
    let output = run_tool(FFMPEG, program, args, COMMAND_TIMEOUT)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(tool_failure(FFMPEG, &output))
    }
}

/// Shared prologue: validate input (audio required), resolve ffmpeg,
/// pick the output container/codec.
fn prepare(
    input: &[u8],
    limits: &EngineLimits,
) -> VideoResult<(VideoInfo, std::path::PathBuf, String, String)> {
    let info = checked_input(input, limits)?;
    let program = find_tool(FFMPEG)?;
    let (ext, codec) = container_target(&info);
    Ok((info, program, ext, codec))
}

/// Assemble and run the common container-preserving command:
/// `-i input -af <filter> <output args> out.ext`. Video streams are
/// stream-copied for A/V-capable containers and dropped (`-vn`) for
/// audio-only ones.
fn run_preserving(
    program: &Path,
    dir: &TempDir,
    input: &[u8],
    ext: &str,
    codec: &str,
    filter: &str,
) -> VideoResult<Vec<u8>> {
    let input_path = write_input(dir, "input.bin", input)?;
    let out_path = dir.file(&format!("out.{ext}"));
    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-af".to_string(),
        filter.to_string(),
    ];
    args.extend(output_args(ext, codec));
    args.push(out_path.to_string_lossy().into_owned());
    run_ffmpeg(program, &args)?;
    read_output(&out_path)
}

/// Clamp a dB gain into the sane audible range shared by both EQ
/// paths (`equalizer` accepts ±900 dB, but anything beyond ±60 is a
/// brick-wall either way).
fn clamp_gain_db(db: f64) -> f64 {
    db.clamp(-60.0, 60.0)
}

/// Build the `firequalizer` gain expression for a 3-band EQ.
/// Piecewise-linear in log-ish anchors: bass below 100 Hz, mid
/// centred at 1 kHz, treble above 8 kHz, linearly interpolated
/// between anchors. `mid` defaults to 0 dB when `None` (neutral
/// shoulder between the outer bands).
///
/// Commas inside the `if()` expression must be escaped (`\,`) so the
/// filtergraph parser does not split them into separate filters.
fn firequalizer_gain(bass_db: f64, mid_db: f64, treble_db: f64) -> String {
    format!(
        "firequalizer=gain='if(lt(f\\,100)\\,{bass}\\,\
         if(lt(f\\,1000)\\,{bass_interp}\\,\
         if(lt(f\\,8000)\\,{treble_interp}\\,{treble})))'",
        bass = clamp_gain_db(bass_db),
        treble = clamp_gain_db(treble_db),
        // bass → mid ramp over 100–1000 Hz:
        //   B + (M - B) * (f - 100) / 900
        bass_interp = format!(
            "{b}+({m}-{b})*(f-100)/900",
            b = clamp_gain_db(bass_db),
            m = clamp_gain_db(mid_db)
        ),
        // mid → treble ramp over 1000–8000 Hz:
        //   M + (T - M) * (f - 1000) / 7000
        treble_interp = format!(
            "{m}+({t}-{m})*(f-1000)/7000",
            m = clamp_gain_db(mid_db),
            t = clamp_gain_db(treble_db)
        ),
    )
}

/// Build the `equalizer` filter chain for a 3-band EQ — the fallback
/// when `firequalizer` is absent. Frequencies/widths per spec:
/// bass f=100 w=200 Hz, mid f=1000, treble f=8000 w=4000 Hz.
/// `t=h` makes `w` a width in Hz (`ffmpeg -h filter=equalizer`
/// verified: `f`, `t`, `w`, `g` options exist). A `None` mid skips
/// that band entirely.
fn equalizer_chain(bass_db: f64, mid_db: Option<f64>, treble_db: f64) -> Vec<String> {
    let mut bands = Vec::with_capacity(3);
    bands.push(format!(
        "equalizer=f=100:t=h:w=200:g={}",
        clamp_gain_db(bass_db)
    ));
    if let Some(mid) = mid_db {
        bands.push(format!(
            "equalizer=f=1000:t=h:w=2000:g={}",
            clamp_gain_db(mid)
        ));
    }
    bands.push(format!(
        "equalizer=f=8000:t=h:w=4000:g={}",
        clamp_gain_db(treble_db)
    ));
    bands
}

/// Apply a 3-band EQ (`bass_db`, `mid_db`, `treble_db`; `None` skips
/// a band). Prefers `firequalizer` (verified present in ffmpeg 7.1.5)
/// with a piecewise-linear gain curve; falls back to a chain of
/// `equalizer` peaking filters when `firequalizer` is missing.
pub(crate) fn audio_eq(
    input: &[u8],
    bass_db: f64,
    mid_db: Option<f64>,
    treble_db: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    for (name, v) in [("bass_db", bass_db), ("treble_db", treble_db)] {
        if !v.is_finite() {
            return Err(VideoError::InvalidInput(format!(
                "audio_eq {name} must be finite, got {v}"
            )));
        }
    }
    if let Some(m) = mid_db {
        if !m.is_finite() {
            return Err(VideoError::InvalidInput(format!(
                "audio_eq mid_db must be finite, got {m}"
            )));
        }
    }

    let filter = if filter_available("firequalizer") {
        // `None` mid → neutral 0 dB shoulder between the outer bands.
        firequalizer_gain(bass_db, mid_db.unwrap_or(0.0), treble_db)
    } else if filter_available("equalizer") {
        equalizer_chain(bass_db, mid_db, treble_db).join(",")
    } else {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "neither the firequalizer nor the equalizer audio filter \
                     is present in this ffmpeg build"
                .to_string(),
        });
    };

    let (_info, program, ext, codec) = prepare(input, limits)?;
    let dir = TempDir::new("audio-eq")?;
    run_preserving(&program, &dir, input, &ext, &codec, &filter)
}

/// Spectral noise reduction via `afftdn` (verified in ffmpeg 7.1.5);
/// `strength` (0.0–1.0, clamped) maps to the `nr` noise-reduction
/// parameter (0.01–97 → `strength * 97`). Falls back to `anlmdn`
/// (non-local-means denoiser, `s` strength 0.00001–1) when `afftdn`
/// is absent.
pub(crate) fn audio_denoise(
    input: &[u8],
    strength: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    if !strength.is_finite() {
        return Err(VideoError::InvalidInput(format!(
            "audio_denoise strength must be finite, got {strength}"
        )));
    }
    let s = strength.clamp(0.0, 1.0);

    let filter = if filter_available("afftdn") {
        // nr range 0.01–97; strength 0 → floor at 0.01 (minimal),
        // strength 1 → 97 (maximum).
        let nr = (s * 97.0).max(0.01);
        format!("afftdn=nr={nr:.2}")
    } else if filter_available("anlmdn") {
        // anlmdn `s` is a 0.00001–1 strength; scale our 0–1 input into
        // the useful part of that range.
        let sn = (0.00001 + s * 0.99999).min(1.0);
        format!("anlmdn=s={sn:.5}")
    } else {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "neither the afftdn nor the anlmdn audio filter is present \
                     in this ffmpeg build"
                .to_string(),
        });
    };

    let (_info, program, ext, codec) = prepare(input, limits)?;
    let dir = TempDir::new("audio-denoise")?;
    run_preserving(&program, &dir, input, &ext, &codec, &filter)
}

/// Dynamic-range compression via `acompressor` (verified in ffmpeg
/// 7.1.5). `threshold_db` is passed with a `dB` suffix — ffmpeg's
/// option parser converts it to the filter's linear threshold;
/// `ratio` is clamped into the filter's 1–20 range. Fixed
/// attack=10 ms / release=200 ms and makeup=2 per spec.
pub(crate) fn audio_compress(
    input: &[u8],
    threshold_db: f64,
    ratio: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    if !filter_available("acompressor") {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "the acompressor audio filter is not present in this \
                     ffmpeg build"
                .to_string(),
        });
    }
    if !threshold_db.is_finite() || !ratio.is_finite() {
        return Err(VideoError::InvalidInput(format!(
            "audio_compress requires finite threshold_db and ratio, got \
             threshold_db={threshold_db}, ratio={ratio}"
        )));
    }
    // acompressor's linear threshold spans ~0.00098–1 ≈ -60..0 dB;
    // clamp rather than let ffmpeg reject the option.
    let threshold = threshold_db.clamp(-60.0, 0.0);
    let ratio = ratio.clamp(1.0, 20.0);
    let filter = format!(
        "acompressor=threshold={threshold:.2}dB:ratio={ratio:.2}:attack=10:release=200:makeup=2"
    );

    let (_info, program, ext, codec) = prepare(input, limits)?;
    let dir = TempDir::new("audio-compress")?;
    run_preserving(&program, &dir, input, &ext, &codec, &filter)
}

/// Shift pitch by `semitones` without changing duration via the
/// `asetrate` → `aresample` → `atempo` chain (all verified in ffmpeg
/// 7.1.5):
///
/// 1. `asetrate={rate}*r` resamples to `rate * r` — pitch *and*
///    duration scale by `r = 2^(semitones/12)`.
/// 2. `aresample={rate}` brings the sample rate back.
/// 3. `atempo=1/r` restores the original duration.
///
/// `rate` comes from the probed audio stream (`sample_rate`),
/// defaulting to 44100 Hz when unreported. `|semitones| > 12` is
/// rejected — beyond that the artifacts of this naive chain grow fast.
pub(crate) fn audio_pitch(
    input: &[u8],
    semitones: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    if !semitones.is_finite() || semitones.abs() > 12.0 {
        return Err(VideoError::InvalidInput(format!(
            "audio_pitch semitones must be finite and within ±12, got {semitones}"
        )));
    }
    for name in ["asetrate", "aresample", "atempo"] {
        if !filter_available(name) {
            return Err(VideoError::ExternalTool {
                tool: FFMPEG.to_string(),
                detail: format!("the {name} audio filter is not present in this ffmpeg build"),
            });
        }
    }

    let (info, program, ext, codec) = prepare(input, limits)?;
    let rate = info
        .streams
        .iter()
        .find(|s| s.codec_type == "audio")
        .and_then(|s| s.sample_rate)
        .filter(|r| *r > 0)
        .unwrap_or(44100);

    let r = 2f64.powf(semitones / 12.0);
    let tempo = 1.0 / r;
    // atempo's range is 0.5–100; semitones ±12 gives r ∈ [0.5, 2],
    // tempo ∈ [0.5, 2] — always in range.
    debug_assert!((0.5..=100.0).contains(&tempo));
    let filter = format!("asetrate={rate}*{r:.6},aresample={rate},atempo={tempo:.6}");

    let dir = TempDir::new("audio-pitch")?;
    run_preserving(&program, &dir, input, &ext, &codec, &filter)
}

/// Render the audio as a spectrogram PNG via `showspectrumpic`
/// (verified in ffmpeg 7.1.5). Returns the PNG bytes — the caller
/// gets image data, not a media container. Requires an audio stream.
///
/// `legend=1` draws the frequency axis; the output frame may be
/// slightly taller than `height` because the legend is overlaid in
/// the frame (observed: `s=320x240` yields a 602x368-ish PNG —
/// ffmpeg pads to even dimensions and the legend consumes rows).
pub(crate) fn audio_spectrogram(
    input: &[u8],
    width: u32,
    height: u32,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    if width == 0 || height == 0 || width > MAX_SPECTROGRAM_DIM || height > MAX_SPECTROGRAM_DIM {
        return Err(VideoError::InvalidInput(format!(
            "audio_spectrogram dimensions must be within 1..={MAX_SPECTROGRAM_DIM}, \
             got {width}x{height}"
        )));
    }
    if !filter_available("showspectrumpic") {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "the showspectrumpic filter is not present in this ffmpeg \
                     build"
                .to_string(),
        });
    }
    checked_input(input, limits)?;
    let program = find_tool(FFMPEG)?;
    let dir = TempDir::new("audio-spectrogram")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file("out.png");

    // Single-frame image output: filter_complex produces a video
    // stream labeled [v]; -frames:v 1 grabs the one frame showspectrumpic
    // emits and the image2 muxer writes PNG.
    let args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-filter_complex".to_string(),
        format!("[0:a]showspectrumpic=s={width}x{height}:legend=1[v]"),
        "-map".to_string(),
        "[v]".to_string(),
        "-frames:v".to_string(),
        "1".to_string(),
        out_path.to_string_lossy().into_owned(),
    ];
    run_ffmpeg(&program, &args)?;
    read_output(&out_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, find_tool};
    use std::fs;

    fn tools_available() -> bool {
        find_tool(FFMPEG).is_ok() && find_tool(FFPROBE).is_ok()
    }

    /// 2s mono sine WAV fixture via lavfi.
    fn make_wav_fixture() -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let dir = TempDir::new("afx-test-wav").ok()?;
        let out = dir.file("tone.wav");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "sine=frequency=440:duration=2".to_string(),
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

    /// 2s A/V MP4 fixture: testsrc video + sine audio.
    fn make_av_fixture() -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        if !h264_aac_available() {
            return None;
        }
        let dir = TempDir::new("afx-test-av").ok()?;
        let out = dir.file("av.mp4");
        let args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc=duration=2:size=160x120:rate=10".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "sine=frequency=440:duration=2".to_string(),
            "-c:v".to_string(),
            "libx264".to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-c:a".to_string(),
            "aac".to_string(),
            "-shortest".to_string(),
            out.to_string_lossy().into_owned(),
        ];
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    /// Whether the fixture's encoders exist (libx264 + aac).
    fn h264_aac_available() -> bool {
        encoder_available("libx264") && encoder_available("aac")
    }

    fn duration_of(bytes: &[u8]) -> Option<f64> {
        probe(bytes).ok().and_then(|i| i.duration_secs)
    }

    fn assert_duration_near(actual: f64, expected: f64, what: &str) {
        assert!(
            (actual - expected).abs() <= 0.35,
            "{what}: expected ~{expected}s, got {actual}s"
        );
    }

    fn assert_nonempty(name: &str, bytes: &[u8]) {
        assert!(!bytes.is_empty(), "{name} produced empty output");
    }

    #[test]
    fn eq_produces_audio_and_preserves_duration() {
        if !tools_available() {
            return;
        }
        let wav = make_wav_fixture().expect("wav fixture");
        let out =
            audio_eq(&wav, 6.0, Some(-3.0), 4.0, &EngineLimits::default()).expect("audio_eq wav");
        assert_nonempty("audio_eq", &out);
        let d = duration_of(&out).expect("probe eq output");
        assert_duration_near(d, 2.0, "audio_eq duration");

        if let Some(av) = make_av_fixture() {
            let out =
                audio_eq(&av, -6.0, None, 3.0, &EngineLimits::default()).expect("audio_eq av");
            assert_nonempty("audio_eq av", &out);
            // A/V container: video must survive the round-trip.
            let info = probe(&out).expect("probe eq av output");
            assert!(info.has_video, "audio_eq dropped the video stream");
        }
    }

    #[test]
    fn denoise_produces_audio_and_preserves_duration() {
        if !tools_available() {
            return;
        }
        let wav = make_wav_fixture().expect("wav fixture");
        let out = audio_denoise(&wav, 0.5, &EngineLimits::default()).expect("audio_denoise");
        assert_nonempty("audio_denoise", &out);
        let d = duration_of(&out).expect("probe denoise output");
        assert_duration_near(d, 2.0, "audio_denoise duration");

        // Out-of-range strength is clamped, not an error.
        let _ = audio_denoise(&wav, 5.0, &EngineLimits::default()).expect("clamped strength");
    }

    #[test]
    fn compress_produces_audio_and_preserves_duration() {
        if !tools_available() {
            return;
        }
        let wav = make_wav_fixture().expect("wav fixture");
        let out =
            audio_compress(&wav, -20.0, 4.0, &EngineLimits::default()).expect("audio_compress");
        assert_nonempty("audio_compress", &out);
        let d = duration_of(&out).expect("probe compress output");
        assert_duration_near(d, 2.0, "audio_compress duration");
    }

    #[test]
    fn pitch_shifts_without_changing_duration() {
        if !tools_available() {
            return;
        }
        let wav = make_wav_fixture().expect("wav fixture");

        // ±12 semitones is the accepted boundary; beyond it errors.
        assert!(
            audio_pitch(&wav, 13.0, &EngineLimits::default()).is_err(),
            "audio_pitch accepted semitones > 12"
        );

        let up = audio_pitch(&wav, 12.0, &EngineLimits::default()).expect("pitch +12");
        let down = audio_pitch(&wav, -12.0, &EngineLimits::default()).expect("pitch -12");
        assert_nonempty("pitch up", &up);
        assert_nonempty("pitch down", &down);
        let d = duration_of(&up).expect("probe pitch up");
        assert_duration_near(d, 2.0, "pitch +12 duration");
        let d = duration_of(&down).expect("probe pitch down");
        assert_duration_near(d, 2.0, "pitch -12 duration");

        // The shifted waveforms must actually differ from each other
        // (different pitch → different PCM payloads).
        assert_ne!(up, down, "pitch +12 and -12 outputs are identical");
    }

    #[test]
    fn spectrogram_produces_png() {
        if !tools_available() {
            return;
        }
        let wav = make_wav_fixture().expect("wav fixture");
        let png =
            audio_spectrogram(&wav, 320, 240, &EngineLimits::default()).expect("audio_spectrogram");
        assert_nonempty("spectrogram", &png);
        // PNG magic: 89 50 4E 47 0D 0A 1A 0A.
        assert_eq!(
            &png[..8],
            b"\x89PNG\r\n\x1a\n",
            "spectrogram output is not a PNG"
        );

        // A/V input also works (audio stream is picked out).
        if let Some(av) = make_av_fixture() {
            let png =
                audio_spectrogram(&av, 200, 150, &EngineLimits::default()).expect("spectrogram av");
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        }
    }
}
