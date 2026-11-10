//! Audio merge / concatenation.
//!
//! [`merge_audio`] concatenates two or more audio inputs into a single
//! file in a target container/codec. Rather than the concat *demuxer*
//! (which requires identical codec parameters across inputs), every
//! input is first decoded to a canonical intermediate — 48 kHz
//! stereo signed-16-bit PCM WAV — then the WAVs are joined with the
//! `concat` audio filter and encoded once to the requested output
//! format. This is robust across arbitrary input containers
//! (mp3/wav/flac/ogg/m4a/...) and lets us optionally loudness-normalize
//! each input before merging.
//!
//! An optional silence gap ([`AudioMergeOptions::gap_ms`]) is
//! synthesized once as a PCM WAV via the `anullsrc` source and
//! inserted between consecutive inputs in the concat graph.
//!
//! The canonical intermediate parameters (48 kHz, stereo, s16) are
//! fixed so every segment has identical stream parameters — a hard
//! requirement of the `concat` filter. Inputs are resampled to match;
//! this is lossy only for sources that differ from 48 kHz stereo.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaFile, MediaInput, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::{process, tempdir::TempDir};

/// Canonical sample rate for the PCM intermediates and the final
/// `concat` graph. 48 kHz covers essentially all modern audio.
const INTERMEDIATE_SAMPLE_RATE: u32 = 48_000;
/// Canonical channel count (stereo).
const INTERMEDIATE_CHANNELS: u32 = 2;

/// Options for [`merge_audio`].
#[derive(Debug, Clone)]
pub struct AudioMergeOptions {
    /// Target container/format: `"mp3"`, `"wav"`, `"flac"`, `"ogg"`,
    /// `"aac"` or `"opus"` (case-insensitive).
    pub output_format: String,
    /// Explicit audio codec override (e.g. `"libopus"`). When `None`,
    /// a sensible default encoder for `output_format` is chosen from
    /// the runtime capability registry.
    pub codec: Option<String>,
    /// Target bit rate for lossy encoders (e.g. `"192k"`). Ignored by
    /// PCM encoders (`wav`).
    pub bitrate: Option<String>,
    /// Silence inserted *between* consecutive inputs, in
    /// milliseconds. `0` (default) merges back-to-back.
    pub gap_ms: u32,
    /// When `true`, each input is passed through a single-pass
    /// `loudnorm` filter during the PCM decode stage so merged
    /// segments have consistent loudness.
    pub normalize_inputs: bool,
}

impl Default for AudioMergeOptions {
    fn default() -> Self {
        Self {
            output_format: "mp3".to_string(),
            codec: None,
            bitrate: None,
            gap_ms: 0,
            normalize_inputs: false,
        }
    }
}

/// Resolved output target: container extension, muxer name, default
/// codec and a best-effort MIME type.
struct OutputTarget {
    /// File extension without dot (`"mp3"`, ...).
    ext: &'static str,
    /// ffmpeg muxer name (`"mp3"`, `"wav"`, `"flac"`, `"ogg"`, `"adts"`).
    muxer: &'static str,
    /// Default encoder name when `options.codec` is `None`.
    default_codec: &'static str,
    /// Whether `bitrate` is meaningful for this target.
    lossy: bool,
    /// Suggested MIME type for the produced [`MediaFile`].
    content_type: &'static str,
}

/// Resolve `output_format` (lowercased) to a target spec, or fail with
/// [`MediaError::InvalidInput`] for unknown formats.
fn resolve_format(format: &str) -> MediaResult<OutputTarget> {
    match format.trim().to_ascii_lowercase().as_str() {
        "mp3" => Ok(OutputTarget {
            ext: "mp3",
            muxer: "mp3",
            default_codec: "libmp3lame",
            lossy: true,
            content_type: "audio/mpeg",
        }),
        "wav" => Ok(OutputTarget {
            ext: "wav",
            muxer: "wav",
            default_codec: "pcm_s16le",
            lossy: false,
            content_type: "audio/wav",
        }),
        "flac" => Ok(OutputTarget {
            ext: "flac",
            muxer: "flac",
            default_codec: "flac",
            lossy: false,
            content_type: "audio/flac",
        }),
        "ogg" => Ok(OutputTarget {
            ext: "ogg",
            muxer: "ogg",
            default_codec: "libvorbis",
            lossy: true,
            content_type: "audio/ogg",
        }),
        "aac" => Ok(OutputTarget {
            ext: "aac",
            muxer: "adts",
            default_codec: "aac",
            lossy: true,
            content_type: "audio/aac",
        }),
        "opus" => Ok(OutputTarget {
            ext: "opus",
            muxer: "ogg",
            default_codec: "libopus",
            lossy: true,
            content_type: "audio/opus",
        }),
        other => Err(MediaError::InvalidInput(format!(
            "unsupported output_format {other:?}; expected one of \
             mp3, wav, flac, ogg, aac, opus"
        ))),
    }
}

/// Merge `inputs` (≥2) into a single audio file.
///
/// Each input is decoded to a 48 kHz stereo s16 PCM WAV intermediate
/// (with optional `loudnorm`), optional silence segments are
/// synthesized for `gap_ms`, and all segments are concatenated with
/// the `concat` filter and encoded once to `options.output_format`.
///
/// Errors:
/// - [`MediaError::InvalidInput`] when fewer than 2 inputs are given
///   or `output_format` is unknown.
/// - [`MediaError::LimitExceeded`] when any input exceeds
///   [`MediaLimits::max_input_bytes`].
/// - [`MediaError::Unsupported`] when the requested codec/filter is
///   absent from this FFmpeg build.
/// - [`MediaError::ToolFailed`] when ffmpeg cannot decode an input or
///   produce the output.
pub fn merge_audio(
    inputs: &[MediaInput],
    options: &AudioMergeOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if inputs.len() < 2 {
        return Err(MediaError::InvalidInput(format!(
            "merge_audio requires at least 2 inputs, got {}",
            inputs.len()
        )));
    }

    let target = resolve_format(&options.output_format)?;
    let caps = rt.capabilities()?;

    // --- resolve encoder -------------------------------------------------
    let codec: String = if let Some(requested) = options.codec.as_deref() {
        if !caps.has_encoder(requested) {
            return Err(MediaError::Unsupported(format!(
                "audio encoder {requested:?} is not available in this FFmpeg build"
            )));
        }
        requested.to_string()
    } else {
        // Prefer the lib* encoder, fall back to the built-in variant.
        let preferred = target.default_codec;
        if caps.has_encoder(preferred) {
            preferred.to_string()
        } else {
            let fallback = preferred.strip_prefix("lib").unwrap_or(preferred);
            if caps.has_encoder(fallback) {
                fallback.to_string()
            } else {
                return Err(MediaError::Unsupported(format!(
                    "no usable audio encoder for format {:?} in this FFmpeg build",
                    options.output_format
                )));
            }
        }
    };

    // Muxer must exist (ogg muxer backs both ogg and opus targets).
    if !caps.has_muxer(target.muxer) {
        return Err(MediaError::Unsupported(format!(
            "muxer {:?} is not available in this FFmpeg build",
            target.muxer
        )));
    }

    // Filters/sources we rely on.
    if !caps.has_filter("concat") {
        return Err(MediaError::Unsupported(
            "the concat audio filter is not available in this FFmpeg build".to_string(),
        ));
    }
    if options.normalize_inputs && !caps.has_filter("loudnorm") {
        return Err(MediaError::Unsupported(
            "the loudnorm filter is not available in this FFmpeg build".to_string(),
        ));
    }
    if options.gap_ms > 0 && !caps.has_filter("anullsrc") {
        return Err(MediaError::Unsupported(
            "the anullsrc source is not available in this FFmpeg build".to_string(),
        ));
    }

    // --- stage inputs ----------------------------------------------------
    let dir = TempDir::new("audio-merge")?;
    let mut input_paths: Vec<PathBuf> = Vec::with_capacity(inputs.len());
    for (i, input) in inputs.iter().enumerate() {
        if let Some(max) = limits.max_input_bytes
            && input.data.len() as u64 > max
        {
            return Err(MediaError::LimitExceeded(format!(
                "input {i} ({}) is {} bytes, exceeding max_input_bytes {max}",
                input.name,
                input.data.len()
            )));
        }
        let ext = extension_of(&input.name);
        let path = dir.write(&format!("in{i:04}{ext}"), &input.data)?;
        input_paths.push(path);
    }

    // --- decode each input to canonical PCM wav --------------------------
    let mut segment_paths: Vec<PathBuf> = Vec::with_capacity(inputs.len());
    for (i, input_path) in input_paths.iter().enumerate() {
        let out_path = dir.file(&format!("seg{i:04}.wav"));
        let mut args: Vec<OsString> = vec![
            OsString::from("-y"),
            OsString::from("-v"),
            OsString::from("error"),
            OsString::from("-i"),
            input_path.clone().into_os_string(),
        ];
        if options.normalize_inputs {
            args.push(OsString::from("-af"));
            args.push(OsString::from("loudnorm"));
        }
        args.extend([
            OsString::from("-map"),
            OsString::from("0:a:0"),
            OsString::from("-vn"),
            OsString::from("-acodec"),
            OsString::from("pcm_s16le"),
            OsString::from("-ar"),
            OsString::from(INTERMEDIATE_SAMPLE_RATE.to_string()),
            OsString::from("-ac"),
            OsString::from(INTERMEDIATE_CHANNELS.to_string()),
            out_path.clone().into_os_string(),
        ]);
        let out = process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, limits).map_err(|e| {
            // A decode failure on a specific input is an input problem,
            // not a generic tool failure — remap for caller clarity.
            match e {
                MediaError::ToolFailed { detail, .. } => MediaError::InvalidInput(format!(
                    "input {} could not be decoded as audio: {detail}",
                    inputs[i].name
                )),
                other => other,
            }
        })?;
        let _ = out;
        segment_paths.push(out_path);
    }

    // --- optional silence segments ----------------------------------------
    // Build the ordered list of segments: input, [silence, input]*.
    let mut ordered: Vec<PathBuf> = Vec::new();
    let mut silence_path: Option<PathBuf> = None;
    if options.gap_ms > 0 {
        let sil = dir.file("silence.wav");
        let secs = options.gap_ms as f64 / 1000.0;
        let args: Vec<OsString> = [
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=48000:cl=stereo",
            "-t",
            &secs.to_string(),
            "-acodec",
            "pcm_s16le",
            "-ar",
            "48000",
            "-ac",
            "2",
            sil.to_str().unwrap_or("silence.wav"),
        ]
        .iter()
        .map(OsString::from)
        .collect();
        process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, limits)?;
        silence_path = Some(sil);
    }

    for (i, seg) in segment_paths.iter().enumerate() {
        if i > 0
            && let Some(sil) = &silence_path
        {
            ordered.push(sil.clone());
        }
        ordered.push(seg.clone());
    }

    // --- concat filter + single encode ------------------------------------
    let n = ordered.len();
    let mut filter = String::new();
    for i in 0..n {
        filter.push_str(&format!("[{i}:a]"));
    }
    filter.push_str(&format!("concat=n={n}:v=0:a=1[aout]"));

    let out_name = format!("output.{}", target.ext);
    let out_path = dir.file(&out_name);

    let mut args: Vec<OsString> = vec![
        OsString::from("-y"),
        OsString::from("-v"),
        OsString::from("error"),
    ];
    for path in &ordered {
        args.push(OsString::from("-i"));
        args.push(path.clone().into_os_string());
    }
    args.extend([
        OsString::from("-filter_complex"),
        OsString::from(filter),
        OsString::from("-map"),
        OsString::from("[aout]"),
        OsString::from("-f"),
        OsString::from(target.muxer),
        OsString::from("-c:a"),
        OsString::from(codec.clone()),
    ]);
    if target.lossy
        && let Some(bitrate) = options.bitrate.as_deref()
    {
        args.push(OsString::from("-b:a"));
        args.push(OsString::from(bitrate));
    }
    args.push(out_path.clone().into_os_string());

    process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, limits)?;

    let bytes = dir.read_limited(&out_name, limits.max_output_bytes)?;

    Ok(MediaOutput {
        primary: Some(MediaFile {
            name: out_name,
            bytes,
            content_type: Some(target.content_type.to_string()),
        }),
        extra: Vec::new(),
        warnings: Vec::new(),
    })
}

/// Extract a filesystem-safe extension from an input display name.
/// Returns `".bin"` when the name has no usable extension — ffmpeg
/// probes content regardless, but a wrong-but-plausible extension can
/// confuse the demuxer, so only known-audio-ish or generic extensions
/// are kept as-is.
fn extension_of(name: &str) -> String {
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    if ext.is_empty() || ext.len() > 8 || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        ".bin".to_string()
    } else {
        format!(".{ext}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::probe::inspect::inspect;
    use crate::crates::multimedia_engine::runtime::discovery;

    /// Generate a mono/stereo sine WAV of `duration` seconds at
    /// `frequency` Hz via ffmpeg lavfi. Returns `None` when ffmpeg or
    /// the sine source is unavailable so tests skip gracefully.
    fn make_sine_wav(freq: u32, duration_secs: u32) -> Option<Vec<u8>> {
        let ffmpeg = discovery::find_tool("ffmpeg")?;
        let dir = TempDir::new("merge-test-gen").ok()?;
        let out_path = dir.file("tone.wav");
        let args: Vec<OsString> = [
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency={freq}:duration={duration_secs}"),
            "-acodec",
            "pcm_s16le",
            out_path.to_str()?,
        ]
        .iter()
        .map(OsString::from)
        .collect();
        let limits = MediaLimits::default();
        let out = process::run("ffmpeg", &ffmpeg, &args, None, &limits).ok()?;
        if !out.success {
            return None;
        }
        std::fs::read(&out_path).ok()
    }

    fn rt() -> Option<Runtime> {
        Runtime::discover().ok()
    }

    /// Duration of the primary output in seconds via ffprobe.
    fn out_duration(out: &MediaOutput) -> f64 {
        let primary = out.primary.as_ref().expect("primary output");
        let info = inspect(&primary.bytes, &MediaLimits::default()).expect("probe output");
        info.duration_secs.expect("output duration")
    }

    #[test]
    fn merge_two_wavs_to_mp3_duration_is_sum() {
        let (Some(a), Some(b), Some(rt)) = (make_sine_wav(440, 1), make_sine_wav(880, 1), rt())
        else {
            eprintln!("skipping: ffmpeg sine generation or runtime unavailable");
            return;
        };
        let inputs = vec![
            MediaInput {
                name: "a.wav".to_string(),
                data: a,
            },
            MediaInput {
                name: "b.wav".to_string(),
                data: b,
            },
        ];
        let options = AudioMergeOptions {
            output_format: "mp3".to_string(),
            bitrate: Some("128k".to_string()),
            ..Default::default()
        };
        let out =
            merge_audio(&inputs, &options, &MediaLimits::default(), &rt).expect("merge to mp3");
        let primary = out.primary.as_ref().expect("primary");
        assert!(primary.name.ends_with(".mp3"), "name: {}", primary.name);
        let dur = out_duration(&out);
        // MP3 encoder delay/padding adds a small tail; allow ~0.5s.
        assert!((dur - 2.0).abs() < 0.5, "duration {dur}");
    }

    #[test]
    fn merge_two_wavs_to_wav_with_gap() {
        let (Some(a), Some(b), Some(rt)) = (make_sine_wav(440, 1), make_sine_wav(660, 1), rt())
        else {
            eprintln!("skipping: ffmpeg sine generation or runtime unavailable");
            return;
        };
        let inputs = vec![
            MediaInput {
                name: "a.wav".to_string(),
                data: a,
            },
            MediaInput {
                name: "b.wav".to_string(),
                data: b,
            },
        ];
        let options = AudioMergeOptions {
            output_format: "wav".to_string(),
            gap_ms: 500,
            ..Default::default()
        };
        let out = merge_audio(&inputs, &options, &MediaLimits::default(), &rt)
            .expect("merge to wav with gap");
        let primary = out.primary.as_ref().expect("primary");
        assert!(primary.name.ends_with(".wav"));
        let dur = out_duration(&out);
        assert!((dur - 2.5).abs() < 0.2, "duration {dur}");
    }

    #[test]
    fn merge_rejects_single_input() {
        let Some(rt) = rt() else {
            eprintln!("skipping: ffmpeg unavailable");
            return;
        };
        let inputs = vec![MediaInput {
            name: "only.wav".to_string(),
            data: vec![0u8; 64],
        }];
        let err = merge_audio(
            &inputs,
            &AudioMergeOptions::default(),
            &MediaLimits::default(),
            &rt,
        )
        .expect_err("single input must fail");
        assert!(
            matches!(err, MediaError::InvalidInput(_)),
            "unexpected error: {err:?}"
        );
    }
}
