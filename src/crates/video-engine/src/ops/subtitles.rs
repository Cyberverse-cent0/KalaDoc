//! Subtitle operations: single-track extraction, format conversion,
//! timing shifts, and burn-in (hard subtitles).
//!
//! Text-format allowlist: `srt` (codec `subrip`), `ass`/`ssa` (codec
//! `ass`), `vtt`/`webvtt` (codec `webvtt`). `mov_text` is deliberately
//! *not* a text target — it is container-bound to MP4/MOV and cannot
//! be extracted to a standalone text file; callers should request
//! `srt` instead (ffmpeg transcodes `mov_text` → `subrip` on the fly).
//!
//! [`shift_subtitles`] is implemented in pure Rust (no ffmpeg): it
//! rewrites cue timestamps for SRT, WebVTT, and ASS `Dialogue:`
//! events. Limitations: only the standard timestamp positions are
//! touched — SRT/VTT timing lines (`HH:MM:SS,mmm --> HH:MM:SS,mmm`,
//! `MM:SS.mmm` tolerated) and ASS `Dialogue:` start/end fields
//! (`H:MM:SS.cc`). Non-standard events, karaoke override tags that
//! embed `\k`-style timings, and exotic timestamp spellings pass
//! through unchanged. Negative results clamp to `00:00:00,000`.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::SubtitleStyle;
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, find_tool, h264_aac_args,
    read_output, run_tool, tool_failure, write_input,
};

/// Text subtitle formats this module produces/consumes.
pub(crate) const SUBTITLE_FORMATS: &[&str] = &["srt", "ass", "ssa", "vtt", "webvtt"];

/// ffmpeg subtitle codec + file extension for a caller-facing format
/// name. Aliases collapse (`ssa`→`ass`, `webvtt`→`vtt`).
struct SubFormat {
    codec: &'static str,
    ext: &'static str,
}

/// Validate `format` against [`SUBTITLE_FORMATS`] and return its
/// codec/extension pair. `mov_text` and unknown names are rejected.
fn sub_format(format: &str) -> VideoResult<SubFormat> {
    let norm = format.trim().trim_start_matches('.').to_ascii_lowercase();
    match norm.as_str() {
        "srt" => Ok(SubFormat {
            codec: "subrip",
            ext: "srt",
        }),
        "ass" | "ssa" => Ok(SubFormat {
            codec: "ass",
            ext: "ass",
        }),
        "vtt" | "webvtt" => Ok(SubFormat {
            codec: "webvtt",
            ext: "vtt",
        }),
        "mov_text" | "movtext" | "tx3g" => Err(VideoError::InvalidInput(
            "mov_text is container-bound (MP4/MOV only) and cannot be extracted \
             as a text file; use srt, ass, or vtt instead"
                .to_string(),
        )),
        other => Err(VideoError::InvalidInput(format!(
            "unsupported subtitle format '{other}'; allowed: {}",
            SUBTITLE_FORMATS.join(", ")
        ))),
    }
}

/// Sniff a subtitle text buffer into `"srt"`, `"ass"`, or `"vtt"`.
/// Used when the caller does not name the source format (burn-in,
/// convert auto-detect). Defaults to SRT — the most common case and
/// the one ffmpeg's demuxer probing also falls back to.
fn sniff_subtitle_format(bytes: &[u8]) -> &'static str {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
    let trimmed = head.trim_start();
    if trimmed.starts_with("WEBVTT") {
        "vtt"
    } else if trimmed.starts_with("[Script Info]")
        || trimmed.starts_with("[V4+ Styles]")
        || trimmed.starts_with("[V4 Styles]")
    {
        "ass"
    } else {
        "srt"
    }
}

/// Extract one subtitle track. `track` is the 0-based subtitle-stream
/// ordinal (`None` → first subtitle stream). `format` is `srt`,
/// `ass`/`ssa`, or `vtt`/`webvtt`. Returns the subtitle text bytes.
pub(crate) fn extract_subtitles(
    input: &[u8],
    track: Option<u32>,
    format: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    // Validate the format before touching PATH so an allowlist
    // violation is InvalidInput even on tool-less machines.
    let fmt = sub_format(format)?;
    let program = find_tool(FFMPEG)?;

    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;

    let sub_count = info
        .streams
        .iter()
        .filter(|s| s.codec_type == "subtitle")
        .count();
    if sub_count == 0 {
        return Err(VideoError::InvalidInput(
            "input has no subtitle streams to extract".to_string(),
        ));
    }
    let ordinal = track.unwrap_or(0) as usize;
    if ordinal >= sub_count {
        return Err(VideoError::InvalidInput(format!(
            "subtitle track {ordinal} out of range: input has {sub_count} subtitle stream(s)"
        )));
    }

    let dir = TempDir::new("subs-extract")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file(&format!("out.{}", fmt.ext));

    // `0:s:{ordinal}` addresses the ordinal-th subtitle stream
    // regardless of its absolute index in the container.
    let args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-map".to_string(),
        format!("0:s:{ordinal}"),
        "-c:s".to_string(),
        fmt.codec.to_string(),
        out_path.to_string_lossy().into_owned(),
    ];

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

/// Convert subtitle text between `srt`, `ass`/`ssa`, and `vtt`.
/// `from_format` may be `None` to auto-detect from content.
/// Parsing is best-effort: unrecognized cue payloads are preserved
/// verbatim where the target format allows.
///
/// The bytes are treated as a standalone subtitle file (the common
/// case: text produced by [`extract_subtitles`] or supplied by the
/// caller). ffmpeg's subtitle demuxers+encoders do the conversion —
/// e.g. SRT → ASS yields `Dialogue:` events under `[Script Info]`.
pub(crate) fn convert_subtitles(
    subtitle_bytes: &[u8],
    from_format: Option<&str>,
    to_format: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(subtitle_bytes)?;
    let to = sub_format(to_format)?;
    // Resolve the source extension: explicit format wins (validated
    // against the same allowlist), otherwise sniff the content.
    let from_ext = match from_format {
        Some(f) => sub_format(f)?.ext.to_string(),
        None => sniff_subtitle_format(subtitle_bytes).to_string(),
    };
    let program = find_tool(FFMPEG)?;

    let dir = TempDir::new("subs-convert")?;
    let input_path = write_input(&dir, &format!("in.{from_ext}"), subtitle_bytes)?;
    let out_path = dir.file(&format!("out.{}", to.ext));

    // Subtitle "files" have no duration/size metadata worth probing;
    // enforce the byte cap directly.
    if let Some(max) = limits.max_input_bytes {
        if input_len > max {
            return Err(VideoError::LimitExceeded(format!(
                "subtitle input is {input_len} bytes, exceeding max_input_bytes {max}"
            )));
        }
    }

    let args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-c:s".to_string(),
        to.codec.to_string(),
        out_path.to_string_lossy().into_owned(),
    ];

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

/// Shift every cue by `offset_secs` (negative = earlier). `format`
/// selects the parser (`srt`/`vtt`/`ass`).
///
/// Implemented in Rust — see the module docs for the exact timestamp
/// coverage and limitations. `format` accepts the same aliases as
/// [`sub_format`] plus raw `webvtt`.
pub(crate) fn shift_subtitles(
    subtitle_bytes: &[u8],
    offset_secs: f64,
    format: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    check_input(subtitle_bytes)?;
    if !offset_secs.is_finite() {
        return Err(VideoError::InvalidInput(format!(
            "offset_secs must be finite, got {offset_secs}"
        )));
    }
    if let Some(max) = limits.max_input_bytes {
        let len = subtitle_bytes.len() as u64;
        if len > max {
            return Err(VideoError::LimitExceeded(format!(
                "subtitle input is {len} bytes, exceeding max_input_bytes {max}"
            )));
        }
    }
    let norm = format.trim().trim_start_matches('.').to_ascii_lowercase();
    let offset_ms = (offset_secs * 1000.0).round() as i64;
    if offset_ms == 0 {
        return Ok(subtitle_bytes.to_vec());
    }

    let text = String::from_utf8(subtitle_bytes.to_vec()).map_err(|_| {
        VideoError::InvalidInput("subtitle input is not valid UTF-8 text".to_string())
    })?;
    let out = match norm.as_str() {
        "srt" => text
            .lines()
            .map(|l| shift_arrow_line(l, offset_ms, ','))
            .collect::<Vec<_>>()
            .join("\n"),
        "vtt" | "webvtt" => text
            .lines()
            .map(|l| shift_arrow_line(l, offset_ms, '.'))
            .collect::<Vec<_>>()
            .join("\n"),
        "ass" | "ssa" => text
            .lines()
            .map(|l| shift_ass_dialogue_line(l, offset_ms))
            .collect::<Vec<_>>()
            .join("\n"),
        other => {
            return Err(VideoError::InvalidInput(format!(
                "unsupported subtitle format '{other}' for shifting; allowed: srt, vtt, ass"
            )));
        }
    };
    // Preserve a trailing newline when the input had one.
    let mut bytes = out.into_bytes();
    if subtitle_bytes.last() == Some(&b'\n') && bytes.last() != Some(&b'\n') {
        bytes.push(b'\n');
    }
    Ok(bytes)
}

/// Burn `subtitle_bytes` into `input` as hard subtitles via the
/// `subtitles` filter. `style` tunes font/size/colors; `None` uses
/// ffmpeg defaults. Re-encodes video.
pub(crate) fn burn_subtitles(
    input: &[u8],
    subtitle_bytes: &[u8],
    style: Option<&SubtitleStyle>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    check_input(subtitle_bytes)?;
    let program = find_tool(FFMPEG)?;

    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;
    if !info.has_video {
        return Err(VideoError::InvalidInput(
            "burn_subtitles requires a video stream; input has none (audio-only?)".to_string(),
        ));
    }

    // The subtitles filter rasterizes through libass; give the file a
    // real subtitle extension so its internal demuxer probe lands on
    // the right parser.
    let sub_ext = sniff_subtitle_format(subtitle_bytes);
    let dir = TempDir::new("subs-burn")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let sub_path = write_input(&dir, &format!("subs.{sub_ext}"), subtitle_bytes)?;
    let out_ext = info
        .format_name
        .as_deref()
        .and_then(crate::crates::video_engine::ops::ext_for_format)
        .unwrap_or("mp4");
    let out_path = dir.file(&format!("out.{out_ext}"));

    let vf = subtitles_filter_arg(&sub_path, style)?;

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-vf".to_string(),
        vf,
    ];
    args.extend(h264_aac_args()?);
    // Keep any soft subtitle streams untouched alongside the burn-in.
    args.extend(["-c:s".to_string(), "copy".to_string()]);
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

// ------------------------------------------------------------------
// Timestamp shifting (pure Rust)
// ------------------------------------------------------------------

/// Parse a subtitle timestamp into milliseconds. Accepts
/// `HH:MM:SS,mmm`, `HH:MM:SS.mmm`, `H:MM:SS.cc` (ASS centiseconds),
/// and `MM:SS.mmm`/`MM:SS,mmm` (hour-less VTT/SRT). Fraction digits
/// are right-padded to milliseconds, so 2-digit ASS centiseconds and
/// 1-digit tenths both land correctly. Returns `None` on malformed
/// input — the caller leaves the line untouched.
fn parse_ts(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (hms, frac) = match raw.find([',', '.']) {
        Some(i) => (&raw[..i], &raw[i + 1..]),
        None => (raw, ""),
    };
    let fields: Vec<&str> = hms.split(':').collect();
    let (hours, minutes, seconds) = match fields.as_slice() {
        [h, m, s] => (
            h.parse::<i64>().ok()?,
            m.parse::<i64>().ok()?,
            s.parse::<i64>().ok()?,
        ),
        [m, s] => (0, m.parse::<i64>().ok()?, s.parse::<i64>().ok()?),
        _ => return None,
    };
    if minutes > 59 || seconds > 59 || hours < 0 || minutes < 0 || seconds < 0 {
        return None;
    }
    // Right-pad the fraction to millisecond precision: ",5" → 500 ms,
    // ".05" → 50 ms, ".005" → 5 ms. More than 3 digits truncates.
    let mut frac_ms: i64 = 0;
    for (i, ch) in frac.chars().take(3).enumerate() {
        let d = ch.to_digit(10)? as i64;
        frac_ms += d * [100, 10, 1][i];
    }
    if frac.chars().any(|c| !c.is_ascii_digit()) && !frac.is_empty() {
        return None;
    }
    let total = ((hours * 60 + minutes) * 60 + seconds) * 1000 + frac_ms;
    (total >= 0).then_some(total)
}

/// Format milliseconds as `HH:MM:SS{sep}mmm` (sep `,` for SRT, `.`
/// for VTT). Hours are not width-capped — long films exceed 99 h
/// legitimately and `{:02}` still prints them fully.
fn format_ts(ms: i64, sep: char) -> String {
    let ms = ms.max(0);
    let (h, rem) = (ms / 3_600_000, ms % 3_600_000);
    let (m, rem) = (rem / 60_000, rem % 60_000);
    let (s, f) = (rem / 1000, rem % 1000);
    format!("{h:02}:{m:02}:{s:02}{sep}{f:03}")
}

/// Format milliseconds as ASS `H:MM:SS.cc` (centiseconds).
fn format_ts_ass(ms: i64) -> String {
    let ms = ms.max(0);
    let (h, rem) = (ms / 3_600_000, ms % 3_600_000);
    let (m, rem) = (rem / 60_000, rem % 60_000);
    let (s, cs) = (rem / 1000, (rem % 1000) / 10);
    format!("{h}:{m:02}:{s:02}.{cs:02}")
}

/// Shift the two timestamps on an SRT/VTT cue-timing line
/// (`start --> end [settings]`). Lines without `-->` or with
/// unparseable timestamps pass through verbatim.
fn shift_arrow_line(line: &str, offset_ms: i64, sep: char) -> String {
    let Some(pos) = line.find("-->") else {
        return line.to_string();
    };
    let left = &line[..pos];
    let right = &line[pos + 3..];

    // Start timestamp = last whitespace-separated token on the left.
    let lt = left.trim_end();
    let boundary = lt.rfind(char::is_whitespace);
    let (lpre, lts) = match boundary {
        Some(i) => (&lt[..=i], &lt[i + 1..]),
        None => ("", lt),
    };
    // End timestamp = first token on the right; trailing cue settings
    // (e.g. `align:start position:0%`) are preserved.
    let rt = right.trim_start();
    let end_boundary = rt.find(char::is_whitespace);
    let (rts, rpost) = match end_boundary {
        Some(i) => (&rt[..i], &rt[i..]),
        None => (rt, ""),
    };

    let (Some(start), Some(end)) = (parse_ts(lts), parse_ts(rts)) else {
        return line.to_string();
    };
    format!(
        "{lpre}{} --> {}{rpost}",
        format_ts(start + offset_ms, sep),
        format_ts(end + offset_ms, sep)
    )
}

/// Shift the start/end fields of an ASS `Dialogue:` line:
/// `Dialogue: Layer,Start,End,Style,...,Text`. All other lines —
/// `[Script Info]` headers, `Format:` rows, `Comment:` events —
/// pass through untouched.
fn shift_ass_dialogue_line(line: &str, offset_ms: i64) -> String {
    let lead = line.len() - line.trim_start().len();
    let body = &line[lead..];
    let Some(rest) = body.strip_prefix("Dialogue:") else {
        return line.to_string();
    };
    // Fields after the prefix: layer,start,end,...,text. splitn(4)
    // keeps the comma-rich tail (style/name/effect/text) intact.
    let mut fields = rest.splitn(4, ',');
    let (Some(layer), Some(start_raw), Some(end_raw), Some(tail)) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return line.to_string();
    };
    let (Some(start), Some(end)) = (parse_ts(start_raw), parse_ts(end_raw)) else {
        return line.to_string();
    };
    format!(
        "{}Dialogue:{},{},{},{tail}",
        &line[..lead],
        layer,
        format_ts_ass(start + offset_ms),
        format_ts_ass(end + offset_ms),
    )
}

// ------------------------------------------------------------------
// Burn-in filter construction
// ------------------------------------------------------------------

/// Escape a filesystem path for use inside a ffmpeg filtergraph
/// single-quoted string. TempDir paths never contain quotes today,
/// but escape anyway so a hostile TMPDIR cannot break the filter.
fn escape_filter_path(path: &std::path::Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
}

/// Convert `#RRGGBB`, `0xRRGGBB`, or bare `RRGGBB` to ASS
/// `&H00BBGGRR&` (alpha=00 opaque, blue/green/red swapped per ASS
/// convention). Returns `None` on malformed input so the caller can
/// reject the style rather than emit a broken override.
fn ass_color(raw: &str) -> Option<String> {
    let hex = raw
        .trim()
        .trim_start_matches('#')
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(format!("&H00{b:02X}{g:02X}{r:02X}&"))
}

/// Build the `-vf` argument: `subtitles='path'[:force_style='...']`.
/// `force_style` is only emitted when at least one style field is
/// set; an all-`None` style yields the bare filter.
fn subtitles_filter_arg(
    sub_path: &std::path::Path,
    style: Option<&SubtitleStyle>,
) -> VideoResult<String> {
    let escaped = escape_filter_path(sub_path);
    let mut vf = format!("subtitles='{escaped}'");

    let Some(style) = style else {
        return Ok(vf);
    };
    let mut overrides: Vec<String> = Vec::new();
    if let Some(v) = &style.font_name {
        overrides.push(format!("FontName={v}"));
    }
    if let Some(v) = style.font_size {
        overrides.push(format!("FontSize={v}"));
    }
    if let Some(v) = &style.primary_color {
        let c = ass_color(v).ok_or_else(|| {
            VideoError::InvalidInput(format!("primary_color '{v}' is not #RRGGBB"))
        })?;
        overrides.push(format!("PrimaryColour={c}"));
    }
    if let Some(v) = &style.outline_color {
        let c = ass_color(v).ok_or_else(|| {
            VideoError::InvalidInput(format!("outline_color '{v}' is not #RRGGBB"))
        })?;
        overrides.push(format!("OutlineColour={c}"));
    }
    if let Some(v) = style.outline_width {
        overrides.push(format!("Outline={v}"));
    }
    if let Some(v) = style.shadow_offset {
        overrides.push(format!("Shadow={v}"));
    }
    if let Some(v) = style.margin_v {
        overrides.push(format!("MarginV={v}"));
    }
    if let Some(v) = style.alignment {
        if !(1..=11).contains(&v) {
            return Err(VideoError::InvalidInput(format!(
                "alignment must be 1-11, got {v}"
            )));
        }
        overrides.push(format!("Alignment={v}"));
    }

    if !overrides.is_empty() {
        vf.push_str(&format!(":force_style='{}'", overrides.join(",")));
    }
    Ok(vf)
}

// ------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::crates::video_engine::ops::{probe::probe, tool_available};

    const SRT: &str = "1\n00:00:00,000 --> 00:00:00,500\nHello world\n\n\
                      2\n00:00:00,500 --> 00:00:00,900\nSecond cue\n";

    fn ffmpeg_available() -> bool {
        tool_available("ffmpeg") && tool_available("ffprobe")
    }

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
                "kaladoc-subs-test-{tag}-{}-{nanos}-{seq}",
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

    /// Run ffmpeg with the given args; panic on failure.
    fn run_ffmpeg(args: &[String]) {
        let status = Command::new("ffmpeg")
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .status()
            .expect("failed to spawn ffmpeg");
        assert!(status.success(), "ffmpeg failed: {args:?}");
    }

    /// MKV fixture with video + audio + an embedded SRT subtitle
    /// track, generated entirely by ffmpeg.
    fn make_mkv_with_subs(dir: &TestDir) -> Vec<u8> {
        let srt_path = dir.0.join("subs.srt");
        fs::write(&srt_path, SRT).unwrap();
        let out = dir.0.join("fixture.mkv");
        run_ffmpeg(&[
            "-y".into(),
            "-v".into(),
            "error".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "testsrc=duration=1:size=320x240:rate=10".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "sine=frequency=440:duration=1".into(),
            "-i".into(),
            srt_path.to_string_lossy().into_owned(),
            "-map".into(),
            "0:v".into(),
            "-map".into(),
            "1:a".into(),
            "-map".into(),
            "2:s".into(),
            "-c:v".into(),
            "mpeg4".into(),
            "-c:a".into(),
            "aac".into(),
            "-c:s".into(),
            "subrip".into(),
            out.to_string_lossy().into_owned(),
        ]);
        fs::read(&out).unwrap()
    }

    /// Plain MP4 clip (no subtitles).
    fn make_mp4(dir: &TestDir) -> Vec<u8> {
        let out = dir.0.join("clip.mp4");
        run_ffmpeg(&[
            "-y".into(),
            "-v".into(),
            "error".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "testsrc=duration=1:size=320x240:rate=10".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "sine=frequency=440:duration=1".into(),
            "-pix_fmt".into(),
            "yuv420p".into(),
            "-shortest".into(),
            out.to_string_lossy().into_owned(),
        ]);
        fs::read(&out).unwrap()
    }

    fn unlimited() -> EngineLimits {
        EngineLimits::default()
    }

    #[test]
    fn extract_returns_srt_text() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TestDir::new("extract");
        let mkv = make_mkv_with_subs(&dir);
        // Sanity: the fixture really carries a subtitle stream.
        assert!(
            probe(&mkv)
                .unwrap()
                .streams
                .iter()
                .any(|s| s.codec_type == "subtitle")
        );

        let srt = extract_subtitles(&mkv, None, "srt", &unlimited()).unwrap();
        let text = String::from_utf8_lossy(&srt);
        assert!(text.contains("-->"), "extracted text: {text}");
        assert!(text.contains("Hello world"));

        // Explicit first-track ordinal works identically.
        let srt2 = extract_subtitles(&mkv, Some(0), "srt", &unlimited()).unwrap();
        assert_eq!(srt, srt2);

        // Out-of-range ordinal and missing-stream input fail cleanly.
        assert!(matches!(
            extract_subtitles(&mkv, Some(7), "srt", &unlimited()),
            Err(VideoError::InvalidInput(_))
        ));
        let mp4 = make_mp4(&dir);
        assert!(matches!(
            extract_subtitles(&mp4, None, "srt", &unlimited()),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn extract_rejects_bad_format() {
        // Format allowlist is enforced before ffmpeg/ffprobe run.
        let err = extract_subtitles(b"not media", None, "xyz", &unlimited()).unwrap_err();
        assert!(matches!(err, VideoError::InvalidInput(_)));
        let err = extract_subtitles(b"not media", None, "mov_text", &unlimited()).unwrap_err();
        assert!(matches!(err, VideoError::InvalidInput(_)));
    }

    #[test]
    fn convert_srt_to_ass() {
        if !ffmpeg_available() {
            return;
        }
        let ass = convert_subtitles(SRT.as_bytes(), Some("srt"), "ass", &unlimited()).unwrap();
        let text = String::from_utf8_lossy(&ass);
        assert!(
            text.contains("Dialogue:") || text.contains("[Script"),
            "ass output: {text}"
        );

        // Auto-detect source format and round-trip back to srt.
        let vtt = convert_subtitles(SRT.as_bytes(), None, "vtt", &unlimited()).unwrap();
        let text = String::from_utf8_lossy(&vtt);
        assert!(text.contains("WEBVTT"), "vtt output: {text}");
        let back = convert_subtitles(&vtt, None, "srt", &unlimited()).unwrap();
        let back_text = String::from_utf8_lossy(&back);
        assert!(back_text.contains("-->"));
    }

    #[test]
    fn burn_produces_video() {
        if !ffmpeg_available() {
            return;
        }
        let dir = TestDir::new("burn");
        let mkv = make_mkv_with_subs(&dir);
        let style = SubtitleStyle {
            font_name: None,
            font_size: Some(20),
            primary_color: Some("#FFFFFF".into()),
            outline_color: Some("0x000000".into()),
            outline_width: Some(2),
            shadow_offset: Some(1),
            margin_v: Some(30),
            alignment: Some(2),
        };
        let burned = burn_subtitles(&mkv, SRT.as_bytes(), Some(&style), &unlimited()).unwrap();
        assert!(!burned.is_empty());
        let info = probe(&burned).unwrap();
        assert!(info.has_video);

        // Audio-only input is rejected before ffmpeg runs.
        let srt_only = dir.0.join("only.srt");
        fs::write(&srt_only, SRT).unwrap();
        // Burn an audio-only file: extract audio to wav first.
        let wav = {
            let clip = make_mp4(&dir);
            // Reuse ffmpeg to strip video → wav bytes.
            let wav_path = dir.0.join("tone.wav");
            let input_path = dir.0.join("tone-in.mp4");
            fs::write(&input_path, &clip).unwrap();
            run_ffmpeg(&[
                "-y".into(),
                "-v".into(),
                "error".into(),
                "-i".into(),
                input_path.to_string_lossy().into_owned(),
                "-vn".into(),
                "-c:a".into(),
                "pcm_s16le".into(),
                wav_path.to_string_lossy().into_owned(),
            ]);
            fs::read(&wav_path).unwrap()
        };
        assert!(matches!(
            burn_subtitles(&wav, SRT.as_bytes(), None, &unlimited()),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn shift_moves_cue_times() {
        if !ffmpeg_available() {
            return;
        }
        let limits = unlimited();
        let shifted = shift_subtitles(SRT.as_bytes(), 0.5, "srt", &limits).unwrap();
        let text = String::from_utf8_lossy(&shifted);
        assert!(text.contains("00:00:01,000 --> 00:00:01,400"), "{text}");
        // Cue payload lines are untouched.
        assert!(text.contains("Hello world"));

        // Negative shift clamps at zero rather than wrapping.
        let back = shift_subtitles(SRT.as_bytes(), -5.0, "srt", &limits).unwrap();
        let text = String::from_utf8_lossy(&back);
        assert!(text.contains("00:00:00,000 --> 00:00:00,000"), "{text}");

        // ASS Dialogue lines shift too.
        let ass_in = b"[Script Info]\nTitle: t\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:02.50,Default,,0,0,0,,Hi there\n";
        let ass_out = shift_subtitles(ass_in, 1.0, "ass", &limits).unwrap();
        let text = String::from_utf8_lossy(&ass_out);
        assert!(text.contains("0:00:02.00,0:00:03.50"), "{text}");

        // WebVTT hour-less timestamps.
        let vtt_in = b"WEBVTT\n\n00:01.000 --> 00:02.000\ncue\n";
        let vtt_out = shift_subtitles(vtt_in, 0.5, "vtt", &limits).unwrap();
        let text = String::from_utf8_lossy(&vtt_out);
        assert!(text.contains("00:00:01.500 --> 00:00:02.500"), "{text}");

        // Malformed format is rejected without touching ffmpeg.
        assert!(matches!(
            shift_subtitles(SRT.as_bytes(), 1.0, "xyz", &limits),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn shift_unit_helpers() {
        // Timestamp parsing: comma/dot/centisecond fractions.
        assert_eq!(parse_ts("00:00:01,500"), Some(1500));
        assert_eq!(parse_ts("0:00:01.05"), Some(1050));
        assert_eq!(parse_ts("0:00:01.5"), Some(1500));
        assert_eq!(parse_ts("01:30"), Some(90_000));
        assert_eq!(parse_ts("not a ts"), None);
        assert_eq!(parse_ts("00:00:01,abc"), None);

        // ASS colour conversion: #RRGGBB → &H00BBGGRR&.
        assert_eq!(ass_color("#FF0000").as_deref(), Some("&H000000FF&"));
        assert_eq!(ass_color("0x00FF00").as_deref(), Some("&H0000FF00&"));
        assert_eq!(ass_color("0000FF").as_deref(), Some("&H00FF0000&"));
        assert_eq!(ass_color("red"), None);

        // Sniffing.
        assert_eq!(sniff_subtitle_format(b"WEBVTT\n\n"), "vtt");
        assert_eq!(sniff_subtitle_format(b"[Script Info]\n"), "ass");
        assert_eq!(sniff_subtitle_format(SRT.as_bytes()), "srt");
    }
}
