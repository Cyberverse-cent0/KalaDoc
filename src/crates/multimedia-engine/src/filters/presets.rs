//! Named built-in filter presets — the friendly layer over
//! [`graph::apply_filters`].
//!
//! [`apply_preset`] maps a preset name to a fixed video (`-vf`) and/or
//! audio (`-af`) filter chain, validates every filter name against the
//! runtime capability set, probes the input's stream types with
//! ffprobe, then runs ffmpeg:
//!
//! - The stream type the preset touches is **re-encoded** through the
//!   chain (filter graphs cannot run on copied data).
//! - The other stream type is passed through with `-c:v copy` /
//!   `-c:a copy`.
//! - If the preset touches a stream type the input does not have, that
//!   chain is skipped with a [`MediaOutput::warnings`] entry.
//! - If neither chain applies (preset `"none"`, or no matching stream),
//!   everything is copied (`-c copy`) — a plain remux.
//!
//! Caveat: `-c copy` on the untouched stream type assumes its codec is
//! already legal in `output_format` (e.g. AAC audio in MP4). A
//! mismatched pair fails with [`MediaError::ToolFailed`], not a silent
//! re-encode — consistent with the engine's "never fall back" rule.
//!
//! # Intensity
//!
//! `PresetOptions::intensity` is `0.0..=1.0` (1.0 = full effect,
//! validated). Only parameters where scaling is meaningful are scaled
//! — documented per preset below. Presets with no scalable parameter
//! (e.g. `"grayscale"`) ignore it.

use std::collections::BTreeSet;
use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::streams::probe_streams;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Characters rejected in caller-supplied tokens (mirrors
/// `graph::FORBIDDEN_CHARS`; `;` would smuggle an unvalidated
/// filter-chain separator past the name check).
const FORBIDDEN_CHARS: &[char] = &[';', '|', '&', '$', '\n', '\r'];

/// One built-in preset: optional video and audio filter-chain
/// renderers. Each renderer takes `intensity` (`0.0..=1.0`) and
/// returns the complete `-vf`/`-af` chain string.
type ChainFn = fn(f64) -> String;

struct Preset {
    /// Lookup name (lowercase).
    name: &'static str,
    /// `-vf` chain builder, if the preset touches video.
    video: Option<ChainFn>,
    /// `-af` chain builder, if the preset touches audio.
    audio: Option<ChainFn>,
    /// Human note on which parameters `intensity` scales.
    doc: &'static str,
}

/// All built-in presets, in `list_presets` order. Video presets apply
/// to `-vf`, audio presets to `-af`; `"none"` is an explicit no-op
/// passthrough.
static PRESETS: &[Preset] = &[
    Preset {
        name: "none",
        video: None,
        audio: None,
        doc: "passthrough — remuxes without filtering",
    },
    // ---- video presets -------------------------------------------------
    Preset {
        name: "grayscale",
        video: Some(|_| "hue=s=0".to_string()),
        audio: None,
        doc: "desaturate (intensity ignored)",
    },
    Preset {
        name: "sharpen",
        video: Some(|i| format!("unsharp=5:5:{:.3}:5:5:0", 2.0 * i)),
        audio: None,
        doc: "unsharp mask; scales luma/chroma amount 0..2",
    },
    Preset {
        name: "soften",
        video: Some(|i| {
            let r = (1.0 + 3.0 * i).round().clamp(1.0, 8.0) as u32;
            format!("boxblur=luma_radius={r}:luma_power=1:chroma_radius={r}:chroma_power=1")
        }),
        audio: None,
        doc: "box blur; scales radius 1..4 px",
    },
    Preset {
        name: "vignette",
        video: Some(|i| {
            // Clamp away from exactly 0 so intensity=0 stays a legal
            // (effectively invisible) vignette rather than risking a
            // range error from the filter.
            let a = (std::f64::consts::PI / 4.0 * i).max(0.001);
            format!("vignette=angle={a:.4}")
        }),
        audio: None,
        doc: "vignette; scales angle 0..PI/4",
    },
    Preset {
        name: "noir",
        video: Some(|i| {
            format!(
                "hue=s=0,curves=master='0/0 0.3/0.12 0.7/0.85 1/1',eq=contrast={:.3}:brightness={:.3}",
                1.0 + 0.5 * i,
                -0.08 * i
            )
        }),
        audio: None,
        doc: "high-contrast B&W; scales eq contrast 1.0..1.5, brightness 0..-0.08",
    },
    Preset {
        name: "warm",
        video: Some(|i| {
            format!(
                "colorbalance=rs={:.3}:gs={:.3}:bs={:.3}",
                0.18 * i,
                0.04 * i,
                -0.18 * i
            )
        }),
        audio: None,
        doc: "warm color grade; scales red/blue balance ±0.18, green +0.04",
    },
    Preset {
        name: "vhs",
        video: Some(|i| {
            format!(
                "noise=alls={}:allf=t+u,curves=master='0/0.05 0.5/0.5 1/0.95',eq=saturation=0.9",
                (20.0 * i).round() as u32
            )
        }),
        audio: None,
        doc: "analog static + contrast crush; scales noise alls 0..20 (curves/eq fixed)",
    },
    Preset {
        name: "vintage",
        video: Some(|i| {
            format!(
                "curves=master='0/0.12 0.5/0.45 1/0.88',colorbalance=rs={:.3}:gs={:.3}:bs={:.3}",
                0.10 * i,
                0.02 * i,
                -0.10 * i
            )
        }),
        audio: None,
        doc: "faded film look; scales colorbalance ±0.10 red/blue, +0.02 green",
    },
    // ---- audio presets -------------------------------------------------
    Preset {
        name: "bass",
        video: None,
        audio: Some(|i| format!("equalizer=f=100:g={:.2}", 6.0 * i)),
        doc: "low-shelf boost @100 Hz; scales gain 0..+6 dB",
    },
    Preset {
        name: "treble",
        video: None,
        audio: Some(|i| {
            format!(
                "equalizer=f=3000:g={:.2},equalizer=f=8000:g={:.2}",
                6.0 * i,
                4.0 * i
            )
        }),
        doc: "high-shelf boost; scales 3 kHz gain 0..+6 dB, 8 kHz 0..+4 dB",
    },
    Preset {
        name: "telephone",
        video: None,
        audio: Some(|i| {
            format!(
                "highpass=f={:.0},lowpass=f={:.0}",
                100.0 + 200.0 * i,
                4200.0 - 800.0 * i
            )
        }),
        doc: "band-limit to voice range; scales cutoff 100..300 Hz / 4200..3400 Hz",
    },
    Preset {
        name: "echo",
        video: None,
        audio: Some(|i| format!("aecho=0.8:0.9:{:.0}:{:.2}", 60.0 + 120.0 * i, 0.3 + 0.4 * i)),
        doc: "single slap echo; scales delay 60..180 ms, decay 0.3..0.7",
    },
];

/// Options for [`apply_preset`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetOptions {
    /// Preset name (case-insensitive). See [`list_presets`] /
    /// [`preset_names`]. `"none"` is a valid no-op preset.
    pub preset: String,
    /// Effect strength `0.0..=1.0`; `1.0` (the default) is full
    /// strength. Out-of-range or non-finite values are
    /// [`MediaError::InvalidInput`]. Only parameters documented per
    /// preset are scaled.
    #[serde(default = "default_intensity")]
    pub intensity: f64,
    /// Output container extension without a leading dot
    /// (`"mp4"`, `"mkv"`, ...). `None`/empty means `"mp4"`.
    #[serde(default)]
    pub output_format: Option<String>,
}

fn default_intensity() -> f64 {
    1.0
}

/// All known preset names, in table order. For error messages and
/// callers building UIs.
pub fn preset_names() -> Vec<&'static str> {
    PRESETS.iter().map(|p| p.name).collect()
}

/// `(name, description)` pairs for every preset — `description`
/// records which parameters `intensity` scales.
pub fn list_presets() -> Vec<(&'static str, &'static str)> {
    PRESETS.iter().map(|p| (p.name, p.doc)).collect()
}

/// Apply a named built-in filter preset to `input`.
///
/// Runs `ffmpeg -hide_banner -loglevel error -nostdin -y -i <in>`
/// with the preset's `-vf`/`-af` chains (only for stream types
/// actually present), `-c <type> copy` for untouched stream types,
/// then reads `output.<format>` back under `limits`.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty/oversized input, unknown
///   preset name, non-finite or out-of-range `intensity`, empty
///   `output_format`, forbidden characters.
/// - [`MediaError::Unsupported`] — a filter used by the preset chain
///   is not in this FFmpeg build's capability set (all missing names
///   are listed).
/// - [`MediaError::ToolFailed`] / [`MediaError::Timeout`] — ffmpeg or
///   the ffprobe stream-type probe failed.
pub fn apply_preset(
    input: &[u8],
    options: &PresetOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if input.is_empty() {
        return Err(MediaError::InvalidInput("empty input buffer".to_string()));
    }
    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }
    if !options.intensity.is_finite() || !(0.0..=1.0).contains(&options.intensity) {
        return Err(MediaError::InvalidInput(format!(
            "intensity must be a finite value in 0.0..=1.0, got {}",
            options.intensity
        )));
    }

    let wanted = options.preset.trim().to_ascii_lowercase();
    let preset = PRESETS.iter().find(|p| p.name == wanted).ok_or_else(|| {
        MediaError::InvalidInput(format!(
            "unknown preset {wanted:?}; valid presets: {}",
            preset_names().join(", ")
        ))
    })?;

    let output_ext = options
        .output_format
        .as_deref()
        .map(str::trim)
        .map(|s| s.trim_start_matches('.'))
        .filter(|s| !s.is_empty())
        .unwrap_or("mp4")
        .to_string();
    reject_forbidden("output_format", &output_ext)?;

    // Render the chains, then validate every filter name against the
    // runtime capability set before spawning anything.
    let video_chain = preset.video.map(|f| f(options.intensity));
    let audio_chain = preset.audio.map(|f| f(options.intensity));

    let caps = rt.capabilities()?;
    let mut missing: BTreeSet<String> = BTreeSet::new();
    for (kind, chain) in [
        ("video", video_chain.as_deref()),
        ("audio", audio_chain.as_deref()),
    ] {
        if let Some(chain) = chain {
            for name in chain_filter_names(chain, kind)? {
                if !caps.has_filter(&name) {
                    missing.insert(name);
                }
            }
        }
    }
    if !missing.is_empty() {
        return Err(MediaError::Unsupported(format!(
            "preset {:?} uses filter(s) not available in this FFmpeg build: {}",
            preset.name,
            missing.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }

    // Discover which stream types the input actually has so we only
    // attach -vf/-af where they can apply and copy the rest.
    let report = probe_streams(input, limits)?;
    let has_video = report
        .streams
        .iter()
        .any(|s| s.codec_type.as_deref() == Some("video"));
    let has_audio = report
        .streams
        .iter()
        .any(|s| s.codec_type.as_deref() == Some("audio"));

    let mut warnings = Vec::new();
    let apply_video = video_chain.is_some() && has_video;
    let apply_audio = audio_chain.is_some() && has_audio;
    if video_chain.is_some() && !has_video {
        warnings.push(format!(
            "preset {:?} has a video chain but the input has no video stream; video filter skipped",
            preset.name
        ));
    }
    if audio_chain.is_some() && !has_audio {
        warnings.push(format!(
            "preset {:?} has an audio chain but the input has no audio stream; audio filter skipped",
            preset.name
        ));
    }

    let dir = TempDir::new("preset")?;
    let input_name = "input.bin";
    dir.write(input_name, input)?;

    let output_name = format!("output.{output_ext}");
    let output_path = dir.file(&output_name);

    let mut argv: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
        OsString::from("-i"),
        dir.file(input_name).into_os_string(),
    ];

    if apply_video && let Some(chain) = video_chain.as_deref() {
        argv.push(OsString::from("-vf"));
        argv.push(OsString::from(chain));
    }
    if apply_audio && let Some(chain) = audio_chain.as_deref() {
        argv.push(OsString::from("-af"));
        argv.push(OsString::from(chain));
    }

    // Untouched stream types are copied, never re-encoded. When
    // nothing was filtered at all this degrades to a plain remux.
    if apply_video && has_audio && !apply_audio {
        argv.push(OsString::from("-c:a"));
        argv.push(OsString::from("copy"));
    }
    if apply_audio && has_video && !apply_video {
        argv.push(OsString::from("-c:v"));
        argv.push(OsString::from("copy"));
    }
    if !apply_video && !apply_audio {
        argv.push(OsString::from("-c"));
        argv.push(OsString::from("copy"));
    }

    argv.push(output_path.into_os_string());

    process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let bytes = dir.read_limited(&output_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: format!("ffmpeg produced an empty {output_name}"),
        });
    }

    let mut out = MediaOutput::single(output_name, bytes);
    if let Some(file) = out.primary.as_mut() {
        file.content_type = content_type_for(&output_ext);
    }
    out.warnings = warnings;
    Ok(out)
}

/// Reject a string containing any [`FORBIDDEN_CHARS`].
fn reject_forbidden(field: &str, value: &str) -> MediaResult<()> {
    if let Some(bad) = value.chars().find(|c| FORBIDDEN_CHARS.contains(c)) {
        return Err(MediaError::InvalidInput(format!(
            "{field} contains forbidden character {bad:?}"
        )));
    }
    Ok(())
}

/// Split `chain` on top-level commas and return each filter's name
/// (token before the first `=`, trimmed, `@label` suffix stripped).
///
/// Same contract as `graph::filter_names`, duplicated because that
/// helper is private: `()` nesting and single/double quotes (with
/// `\` escaping) are respected so commas inside e.g.
/// `curves=master='0/0,1/1'` do not split. Used for capability
/// validation only — ffmpeg receives the original chain verbatim.
fn chain_filter_names(chain: &str, field: &str) -> MediaResult<Vec<String>> {
    reject_forbidden(field, chain)?;

    let mut elements: Vec<&str> = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut start = 0usize;
    let mut chars = chain.char_indices().peekable();

    while let Some((i, c)) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' {
                    chars.next();
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth < 0 {
                        return Err(MediaError::InvalidInput(format!(
                            "{field} has unbalanced ')'"
                        )));
                    }
                }
                ',' if depth == 0 => {
                    elements.push(&chain[start..i]);
                    start = i + 1;
                }
                _ => {}
            },
        }
    }
    if quote.is_some() {
        return Err(MediaError::InvalidInput(format!(
            "{field} has an unterminated quote"
        )));
    }
    if depth != 0 {
        return Err(MediaError::InvalidInput(format!(
            "{field} has unbalanced parentheses"
        )));
    }
    elements.push(&chain[start..]);

    let mut names = Vec::with_capacity(elements.len());
    for element in elements {
        let element = element.trim();
        if element.is_empty() {
            return Err(MediaError::InvalidInput(format!(
                "{field} has an empty filter element"
            )));
        }
        let name = element
            .split('=')
            .next()
            .unwrap_or("")
            .trim()
            .trim_end_matches('@');
        if name.is_empty() {
            return Err(MediaError::InvalidInput(format!(
                "{field} element {element:?} has no filter name"
            )));
        }
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(MediaError::InvalidInput(format!(
                "{field} filter name {name:?} contains invalid characters"
            )));
        }
        names.push(name.to_string());
    }
    Ok(names)
}

/// Best-effort MIME type for common output containers (mirrors
/// `graph::content_type_for`, also private there).
fn content_type_for(format: &str) -> Option<String> {
    let mime = match format.to_ascii_lowercase().as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "wav" => "audio/wav",
        "ogg" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "gif" => "image/gif",
        _ => return None,
    };
    Some(mime.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::discovery::find_tool;
    use crate::crates::multimedia_engine::runtime::process;
    use std::path::PathBuf;

    fn have_tools() -> bool {
        find_tool("ffmpeg").is_some() && find_tool("ffprobe").is_some()
    }

    /// Generate a 1-second testsrc+sine mp4 via lavfi (same recipe as
    /// `graph` tests).
    fn sample_input() -> Option<Vec<u8>> {
        let rt = Runtime::discover().ok()?;
        let dir = TempDir::new("preset-test-in").ok()?;
        let out_path: PathBuf = dir.file("sample.mp4");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=160x120:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:v",
            "mpeg4",
            "-c:a",
            "aac",
            "-shortest",
            out_path.to_str()?,
        ]);
        process::run("ffmpeg", &rt.ffmpeg, &argv, None, &MediaLimits::default()).ok()?;
        std::fs::read(&out_path).ok()
    }

    fn opts(preset: &str) -> PresetOptions {
        PresetOptions {
            preset: preset.to_string(),
            intensity: 1.0,
            output_format: Some("mp4".to_string()),
        }
    }

    #[test]
    fn apply_preset_grayscale_produces_output() {
        if !have_tools() {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        }
        let Some(input) = sample_input() else {
            eprintln!("skipping: could not build sample mp4");
            return;
        };
        let rt = Runtime::discover().expect("runtime");
        let out = apply_preset(&input, &opts("grayscale"), &MediaLimits::default(), &rt)
            .expect("grayscale preset");
        let file = out.primary.expect("primary output");
        assert_eq!(file.name, "output.mp4");
        assert!(!file.bytes.is_empty());
        assert_eq!(file.content_type.as_deref(), Some("video/mp4"));
    }

    #[test]
    fn apply_preset_sharpen_produces_output() {
        if !have_tools() {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        }
        let Some(input) = sample_input() else {
            eprintln!("skipping: could not build sample mp4");
            return;
        };
        let rt = Runtime::discover().expect("runtime");
        let out = apply_preset(&input, &opts("sharpen"), &MediaLimits::default(), &rt)
            .expect("sharpen preset");
        assert!(!out.primary.expect("primary").bytes.is_empty());
    }

    #[test]
    fn apply_preset_unknown_name_is_invalid_input() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => {
                eprintln!("skipping: ffmpeg/ffprobe not on PATH");
                return;
            }
        };
        let err = apply_preset(
            &[1, 2, 3],
            &opts("does-not-exist"),
            &MediaLimits::default(),
            &rt,
        )
        .expect_err("unknown preset must fail");
        match err {
            MediaError::InvalidInput(msg) => {
                assert!(msg.contains("does-not-exist"), "msg: {msg}");
                assert!(
                    msg.contains("grayscale"),
                    "msg should list valid names: {msg}"
                );
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn apply_preset_rejects_bad_intensity() {
        let rt = match Runtime::discover() {
            Ok(rt) => rt,
            Err(_) => {
                eprintln!("skipping: ffmpeg/ffprobe not on PATH");
                return;
            }
        };
        for bad in [1.5, -0.1, f64::NAN] {
            let mut o = opts("grayscale");
            o.intensity = bad;
            let err = apply_preset(&[1, 2, 3], &o, &MediaLimits::default(), &rt)
                .expect_err("bad intensity must fail");
            assert!(
                matches!(err, MediaError::InvalidInput(_)),
                "intensity {bad}: expected InvalidInput, got {err:?}"
            );
        }
    }

    #[test]
    fn preset_table_is_well_formed() {
        let names = preset_names();
        assert!(names.contains(&"none"));
        assert!(names.contains(&"grayscale"));
        assert!(names.contains(&"bass"));
        // No duplicate names.
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(names.len(), sorted.len(), "duplicate preset names");
        // list_presets covers every preset.
        assert_eq!(list_presets().len(), names.len());
    }
}
