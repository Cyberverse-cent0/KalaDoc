//! Subtitle cue retiming (pure-Rust text processing).
//!
//! [`retime`] shifts every cue timestamp in a sidecar subtitle file
//! (SRT, WebVTT or ASS/SSA) by a fixed millisecond offset — the
//! subtitle-file equivalent of `ffmpeg -itsoffset`/`-avoid_negative_ts`,
//! but done without spawning a tool: subtitle files are text, so the
//! op parses timestamps, adds the offset and re-emits the same format.
//!
//! # Formats
//!
//! | Format | Timestamp syntax | Cue line |
//! |--------|------------------|----------|
//! | SRT    | `HH:MM:SS,mmm`   | any line containing `-->` |
//! | VTT    | `HH:MM:SS.mmm` or `MM:SS.mmm` | same |
//! | ASS    | `h:mm:ss.cc` (centiseconds) | `Dialogue: Layer,Start,End,...` |
//!
//! Format selection: `options.format` (`"srt"`/`"vtt"`/`"ass"`/`"ssa"`,
//! case-insensitive) wins; otherwise the content is sniffed (`WEBVTT`
//! header, `[Script Info]`/`Dialogue:` lines, `-->` separator style),
//! with `options.input_name`'s extension as a tiebreaker.
//!
//! # Negative results
//!
//! Cues whose *start* time would move below zero are handled per
//! `options.clamp`:
//!
//! - `clamp: false` (**default**) — the whole cue is **dropped**
//!   (for SRT/VTT the entire blank-line-delimited block, for ASS just
//!   the `Dialogue:` line).
//! - `clamp: true` — negative timestamps are clamped to `0` (a cue
//!   can become zero-length).
//!
//! # Caveats
//!
//! - Input is decoded as UTF-8 (lossy) and re-emitted as UTF-8;
//!   non-UTF-8 legacy encodings (e.g. Latin-1 SRT) are transcoded.
//! - A UTF-8 BOM and CRLF line endings are preserved.
//! - Unparseable `-->` lines (e.g. literal `-->` inside cue text) are
//!   passed through untouched.

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;

/// Largest accepted `|offset_ms|` — one day. Subtitle shifts beyond
/// this are almost certainly a units bug (seconds vs ms), so reject
/// rather than produce nonsense.
const MAX_OFFSET_MS: i64 = 24 * 60 * 60 * 1000;

/// Options for [`retime`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RetimeOptions {
    /// Shift applied to every cue timestamp, in milliseconds.
    /// Positive delays cues, negative advances them. `0` is a no-op
    /// (still validates and re-emits the file).
    pub offset_ms: i64,
    /// Input format: `"srt"`, `"vtt"`, `"ass"` or `"ssa"`
    /// (case-insensitive). `None` auto-detects from content, falling
    /// back to `input_name`'s extension.
    pub format: Option<String>,
    /// How cues pushed below `t=0` are handled. `false` (default)
    /// **drops** them; `true` clamps their timestamps to `0`.
    pub clamp: bool,
    /// Optional file-name hint (e.g. `"movie.srt"`) used only for
    /// format auto-detection — the bytes themselves carry no name.
    pub input_name: Option<String>,
}

/// Detected sidecar format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubFormat {
    Srt,
    Vtt,
    Ass,
}

impl SubFormat {
    fn extension(self) -> &'static str {
        match self {
            Self::Srt => "srt",
            Self::Ass => "ass",
            Self::Vtt => "vtt",
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Self::Srt => "application/x-subrip",
            Self::Ass => "text/x-ssa",
            Self::Vtt => "text/vtt",
        }
    }

    /// Timestamp formatter for this format.
    fn format_ts(self, ms: i64) -> String {
        let (h, rem) = (ms / 3_600_000, ms % 3_600_000);
        let (m, rem) = (rem / 60_000, rem % 60_000);
        let (s, milli) = (rem / 1_000, rem % 1_000);
        match self {
            Self::Srt => format!("{h:02}:{m:02}:{s:02},{milli:03}"),
            Self::Vtt => format!("{h:02}:{m:02}:{s:02}.{milli:03}"),
            // ASS/SSA use centiseconds and unpadded hours.
            Self::Ass => format!("{h}:{m:02}:{s:02}.{:02}", milli / 10),
        }
    }
}

/// Parse a subtitle timestamp into milliseconds.
///
/// Accepts `H:MM:SS.frac`, `HH:MM:SS,mmm`, `MM:SS.mmm` — i.e. two or
/// three colon-separated fields with an optional `,`/`.` fraction.
/// Fraction digits are right-padded to milliseconds (`.5` = 500 ms).
/// Returns `None` on any malformed input.
fn parse_timestamp(tok: &str) -> Option<i64> {
    let tok = tok.trim();
    if tok.is_empty() {
        return None;
    }
    // Split off the fraction at the first ',' or '.'.
    let (hms, frac) = match tok.find([',', '.']) {
        Some(i) => (&tok[..i], &tok[i + 1..]),
        None => (tok, ""),
    };
    let mut ms_frac: i64 = 0;
    if !frac.is_empty() {
        if !frac.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let digits = frac.as_bytes();
        // Right-pad/truncate to exactly 3 digits.
        for i in 0..3 {
            let d = if i < digits.len() {
                (digits[i] - b'0') as i64
            } else {
                0
            };
            ms_frac = ms_frac * 10 + d;
        }
    }
    let parts: Vec<&str> = hms.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let mut fields = [0i64; 3]; // hours, minutes, seconds
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        // Two-part form is MM:SS — shift into m/s slots.
        fields[if parts.len() == 2 { i + 1 } else { i }] = p.parse().ok()?;
    }
    let (h, m, s) = (fields[0], fields[1], fields[2]);
    Some(h * 3_600_000 + m * 60_000 + s * 1_000 + ms_frac)
}

/// Apply the offset. Returns `None` when the cue must be dropped.
fn shift_ms(t: i64, offset: i64, clamp: bool) -> Option<i64> {
    let shifted = t.checked_add(offset)?;
    if shifted < 0 {
        if clamp { Some(0) } else { None }
    } else {
        Some(shifted)
    }
}

/// Outcome of inspecting one `-->` timing line.
enum TimingLine {
    /// Not a parseable timestamp line — pass through unchanged.
    NotTiming,
    /// Both timestamps shifted; carries the rewritten line.
    Shifted(String),
    /// Start moved below zero and `clamp` is off — drop the cue.
    Drop,
}

/// Try to retime a `HH:MM:SS,mmm --> HH:MM:SS,mmm` style line.
/// Preserves any VTT cue-settings tail after the end timestamp.
fn retime_arrow_line(line: &str, offset: i64, clamp: bool, fmt: SubFormat) -> TimingLine {
    let Some(pos) = line.find("-->") else {
        return TimingLine::NotTiming;
    };
    let start_tok = line[..pos].trim();
    let Some(start_ms) = parse_timestamp(start_tok) else {
        return TimingLine::NotTiming;
    };
    let right = line[pos + 3..].trim_start();
    let end_tok_len = right
        .char_indices()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(right.len());
    let end_tok = &right[..end_tok_len];
    let Some(end_ms) = parse_timestamp(end_tok) else {
        return TimingLine::NotTiming;
    };
    // Tail after the end token (VTT settings like `line:0 align:start`).
    let tail = right[end_tok_len..].trim_start();
    let tail = if tail.is_empty() {
        String::new()
    } else {
        format!(" {tail}")
    };

    let new_start = shift_ms(start_ms, offset, clamp);
    if new_start.is_none() {
        return TimingLine::Drop;
    }
    // End < start would be malformed input; still shift honestly.
    let new_end = shift_ms(end_ms, offset, clamp).unwrap_or(0);
    TimingLine::Shifted(format!(
        "{} --> {}{}",
        fmt.format_ts(new_start.unwrap()),
        fmt.format_ts(new_end),
        tail
    ))
}

/// Retype one ASS `Dialogue:` line's start/end fields.
/// Fields: `Dialogue: Layer,Start,End,Style,...,Text` — only commas
/// *before* the text matter, and Text may itself contain commas, so we
/// locate the third comma and splice only fields 1 and 2.
fn retime_ass_line(line: &str, offset: i64, clamp: bool, fmt: SubFormat) -> TimingLine {
    let rest = match line.strip_prefix("Dialogue:") {
        Some(r) => r,
        None => return TimingLine::NotTiming,
    };
    // Find the first three commas: after Layer, after Start, after End.
    let mut commas = [None::<usize>; 3];
    let mut count = 0;
    for (i, b) in rest.bytes().enumerate() {
        if b == b',' {
            commas[count] = Some(i);
            count += 1;
            if count == 3 {
                break;
            }
        }
    }
    let (Some(c1), Some(c2), Some(c3)) = (commas[0], commas[1], commas[2]) else {
        return TimingLine::NotTiming;
    };
    // c1 is end of Layer; field layout relative to `rest`:
    //   [0..c1)        = Layer
    //   [c1+1..c2)     = Start
    //   [c2+1..c3)     = End
    //   [c3..]         = ",Style,...,Text" (untouched)
    let start_ms = parse_timestamp(&rest[c1 + 1..c2]);
    let end_ms = parse_timestamp(&rest[c2 + 1..c3]);
    let (Some(start_ms), Some(end_ms)) = (start_ms, end_ms) else {
        return TimingLine::NotTiming;
    };
    if shift_ms(start_ms, offset, clamp).is_none() {
        return TimingLine::Drop;
    }
    let new_start = shift_ms(start_ms, offset, clamp).unwrap();
    let new_end = shift_ms(end_ms, offset, clamp).unwrap_or(0);
    TimingLine::Shifted(format!(
        "Dialogue:{}{},{}{}",
        &rest[..c1 + 1], // includes "Layer,"
        fmt.format_ts(new_start),
        fmt.format_ts(new_end),
        &rest[c3..] // starts at the comma before Style
    ))
}

/// Resolve the input format: explicit option → content sniff →
/// filename extension → error.
fn detect_format(text: &str, options: &RetimeOptions) -> MediaResult<SubFormat> {
    if let Some(f) = options.format.as_deref() {
        return match f.trim().to_ascii_lowercase().as_str() {
            "srt" => Ok(SubFormat::Srt),
            "vtt" => Ok(SubFormat::Vtt),
            "ass" | "ssa" => Ok(SubFormat::Ass),
            other => Err(MediaError::InvalidInput(format!(
                "unknown subtitle format {other:?} (expected srt, vtt, ass or ssa)"
            ))),
        };
    }

    let body = text.trim_start_matches('\u{feff}').trim_start();
    // Strong content signals first.
    if body.starts_with("WEBVTT") {
        return Ok(SubFormat::Vtt);
    }
    if body.contains("[Script Info]") || body.lines().any(|l| l.starts_with("Dialogue:")) {
        return Ok(SubFormat::Ass);
    }
    // `-->` lines: SRT uses a comma fraction, VTT a dot — or bare
    // `MM:SS` with neither. Prefer the separator actually present.
    for line in body.lines() {
        if let Some(pos) = line.find("-->") {
            let before = &line[..pos];
            let after = &line[pos + 3..];
            let has_comma = before.contains(',')
                || after
                    .split_whitespace()
                    .next()
                    .is_some_and(|t| t.contains(','));
            let has_dot = before.contains('.')
                || after
                    .split_whitespace()
                    .next()
                    .is_some_and(|t| t.contains('.'));
            if has_comma && !has_dot {
                return Ok(SubFormat::Srt);
            }
            if has_dot && !has_comma {
                return Ok(SubFormat::Vtt);
            }
            // Ambiguous (or both/neither) — fall through to extension/default.
            break;
        }
    }
    // Extension hint.
    if let Some(name) = options.input_name.as_deref() {
        let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        match ext.as_str() {
            "srt" => return Ok(SubFormat::Srt),
            "vtt" => return Ok(SubFormat::Vtt),
            "ass" | "ssa" => return Ok(SubFormat::Ass),
            _ => {}
        }
    }
    // Last resort: any `-->` content is almost certainly SRT/VTT; pick
    // SRT (the most common sidecar) rather than failing on odd files.
    if body.contains("-->") {
        return Ok(SubFormat::Srt);
    }
    Err(MediaError::InvalidInput(
        "unrecognized subtitle format: no WEBVTT header, ASS sections, or --> cue timings found"
            .to_string(),
    ))
}

fn check_size(input: &[u8], limits: &MediaLimits) -> MediaResult<()> {
    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }
    Ok(())
}

/// Shift every cue timestamp in `input` by `options.offset_ms`.
///
/// `input` is a complete sidecar subtitle file (SRT, WebVTT or
/// ASS/SSA). The output keeps the detected/input format — this op
/// retimes, it does not convert (see [`super::convert::convert_subtitles`]).
///
/// `rt` is accepted for signature consistency with the other ops but
/// is unused — retiming is pure text processing and never invokes
/// ffmpeg/ffprobe.
///
/// # Errors
///
/// - [`MediaError::LimitExceeded`] when `input` exceeds
///   `limits.max_input_bytes`, or the result exceeds
///   `limits.max_output_bytes`.
/// - [`MediaError::InvalidInput`] on empty input, an unknown
///   `options.format`, an `offset_ms` beyond ±24 h, or content that
///   matches no known subtitle format.
pub fn retime(
    input: &[u8],
    options: &RetimeOptions,
    limits: &MediaLimits,
    _rt: &Runtime,
) -> MediaResult<MediaOutput> {
    check_size(input, limits)?;
    if input.is_empty() {
        return Err(MediaError::InvalidInput("empty subtitle input".to_string()));
    }
    if options.offset_ms.unsigned_abs() > MAX_OFFSET_MS as u64 {
        return Err(MediaError::InvalidInput(format!(
            "offset_ms {} exceeds sane bound ±{MAX_OFFSET_MS} (24h)",
            options.offset_ms
        )));
    }
    let offset = options.offset_ms;

    // Lossy UTF-8 decode; BOM and CRLF are preserved on re-emit.
    let text = String::from_utf8_lossy(input);
    let had_bom = text.starts_with('\u{feff}');
    // `\u{feff}` is 3 bytes in UTF-8; strip it before parsing.
    let body: &str = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let eol = if body.contains("\r\n") { "\r\n" } else { "\n" };

    let fmt = detect_format(body, options)?;

    let mut out_lines: Vec<String> = Vec::new();
    let mut cues_shifted = 0usize;
    let mut cues_dropped = 0usize;

    if fmt == SubFormat::Ass {
        // ASS cues are single self-contained `Dialogue:` lines — no
        // block semantics needed.
        for line in body.lines() {
            match retime_ass_line(line, offset, options.clamp, fmt) {
                TimingLine::Shifted(s) => {
                    cues_shifted += 1;
                    out_lines.push(s);
                }
                TimingLine::Drop => cues_dropped += 1,
                TimingLine::NotTiming => out_lines.push(line.to_string()),
            }
        }
    } else {
        // SRT/VTT: cues are blank-line-delimited blocks. The timing
        // line sits inside the block (after an optional index or
        // cue-identifier line); dropping a cue means dropping the
        // whole block, so buffer until the blank line.
        let mut block: Vec<String> = Vec::new();
        let flush = |block: &mut Vec<String>,
                     out: &mut Vec<String>,
                     shifted: &mut usize,
                     dropped: &mut usize| {
            if block.is_empty() {
                return;
            }
            // Find the timing line within the block.
            let mut timing_idx = None;
            let mut timing_result = TimingLine::NotTiming;
            for (i, l) in block.iter().enumerate() {
                let r = retime_arrow_line(l, offset, options.clamp, fmt);
                if !matches!(r, TimingLine::NotTiming) {
                    timing_idx = Some(i);
                    timing_result = r;
                    break;
                }
            }
            match timing_result {
                TimingLine::Shifted(new_line) => {
                    block[timing_idx.unwrap()] = new_line;
                    *shifted += 1;
                    out.extend(block.drain(..));
                }
                TimingLine::Drop => {
                    *dropped += 1;
                    block.clear();
                }
                TimingLine::NotTiming => out.extend(block.drain(..)),
            }
        };

        for line in body.lines() {
            if line.trim().is_empty() {
                flush(
                    &mut block,
                    &mut out_lines,
                    &mut cues_shifted,
                    &mut cues_dropped,
                );
                out_lines.push(String::new()); // preserve the blank separator
            } else {
                block.push(line.to_string());
            }
        }
        flush(
            &mut block,
            &mut out_lines,
            &mut cues_shifted,
            &mut cues_dropped,
        );
    }

    let mut result_text = String::new();
    if had_bom {
        result_text.push('\u{feff}');
    }
    // `lines()` drops the final terminator; re-add exactly one.
    for (i, l) in out_lines.iter().enumerate() {
        if i > 0 {
            result_text.push_str(eol);
        }
        result_text.push_str(l);
    }
    if !out_lines.is_empty() {
        result_text.push_str(eol);
    }

    let bytes = result_text.into_bytes();
    if let Some(max) = limits.max_output_bytes
        && (bytes.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "retimed output is {} bytes, exceeding max_output_bytes {max}",
            bytes.len()
        )));
    }

    let out_name = format!("output.{}", fmt.extension());
    let mut result = MediaOutput::single(out_name, bytes);
    if let Some(primary) = result.primary.as_mut() {
        primary.content_type = Some(fmt.content_type().to_string());
    }
    if cues_dropped > 0 {
        result.warnings.push(format!(
            "dropped {cues_dropped} cue(s) pushed below t=0 (clamp=false)"
        ));
    }
    if cues_shifted == 0 {
        result
            .warnings
            .push("no cue timestamps found; output is unchanged".to_string());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRT: &str = "1\n00:00:01,000 --> 00:00:02,500\nHello\n\n\
                        2\n00:00:03,000 --> 00:00:04,000\nWorld\n";

    const VTT: &str = "WEBVTT\n\n\
                       NOTE a comment\n\n\
                       00:01.000 --> 00:02.500 align:start\nHi there\n\n\
                       00:00:03.000 --> 00:00:04.000\nSecond\n";

    const ASS: &str = "[Script Info]\nTitle: test\n\n\
                       [V4+ Styles]\nFormat: Name, Fontname\n\n\
                       [Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
                       Dialogue: 0,0:00:01.00,0:00:02.50,Default,,0,0,0,,Hello\n\
                       Dialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,World\n";

    fn runtime() -> Option<Runtime> {
        // retime never touches the runtime, but the signature requires
        // one — skip gracefully when ffmpeg isn't installed.
        Runtime::discover().ok()
    }

    fn opts(offset_ms: i64) -> RetimeOptions {
        RetimeOptions {
            offset_ms,
            format: Some("srt".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn srt_shift_plus_one_second() {
        let Some(rt) = runtime() else { return };
        let out = retime(SRT.as_bytes(), &opts(1000), &MediaLimits::default(), &rt)
            .expect("retime failed");
        let primary = out.primary.unwrap();
        let text = String::from_utf8_lossy(&primary.bytes);
        assert!(text.contains("00:00:02,000 --> 00:00:03,500"), "{text}");
        assert!(text.contains("00:00:04,000 --> 00:00:05,000"), "{text}");
        assert!(text.contains("Hello"), "{text}");
    }

    #[test]
    fn srt_negative_offset_drops_and_clamps() {
        let Some(rt) = runtime() else { return };
        let srt = "1\n00:00:00,500 --> 00:00:01,000\nEarly\n\n\
                   2\n00:00:02,000 --> 00:00:03,000\nLate\n";
        // -750ms: cue 1 starts at -250 → dropped; cue 2 → 1.25–2.25s.
        let out = retime(srt.as_bytes(), &opts(-750), &MediaLimits::default(), &rt)
            .expect("retime failed");
        let text = String::from_utf8_lossy(&out.primary.as_ref().unwrap().bytes);
        assert!(!text.contains("Early"), "dropped cue leaked:\n{text}");
        assert!(text.contains("00:00:01,250 --> 00:00:02,250"), "{text}");
        assert!(!out.warnings.is_empty());

        // clamp=true: cue 1 clamps to 0 instead of dropping.
        let clamp_opts = RetimeOptions {
            clamp: true,
            ..opts(-750)
        };
        let out = retime(srt.as_bytes(), &clamp_opts, &MediaLimits::default(), &rt)
            .expect("retime failed");
        let primary = out.primary.unwrap();
        let text = String::from_utf8_lossy(&primary.bytes);
        assert!(text.contains("00:00:00,000 --> 00:00:00,250"), "{text}");
        assert!(text.contains("Early"), "{text}");
    }

    #[test]
    fn vtt_autodetect_and_shift() {
        let Some(rt) = runtime() else { return };
        let opts = RetimeOptions {
            offset_ms: 500,
            format: None,
            ..Default::default()
        };
        let out =
            retime(VTT.as_bytes(), &opts, &MediaLimits::default(), &rt).expect("retime failed");
        let primary = out.primary.unwrap();
        assert_eq!(primary.name, "output.vtt");
        assert_eq!(primary.content_type.as_deref(), Some("text/vtt"));
        let text = String::from_utf8_lossy(&primary.bytes);
        assert!(text.starts_with("WEBVTT"), "{text}");
        assert!(text.contains("NOTE a comment"), "{text}");
        // MM:SS.mmm cue +500ms → 00:01.500; settings tail preserved.
        assert!(
            text.contains("00:01.500 --> 00:03.000 align:start"),
            "{text}"
        );
        assert!(text.contains("00:00:03.500 --> 00:00:04.500"), "{text}");
    }

    #[test]
    fn ass_autodetect_and_shift() {
        let Some(rt) = runtime() else { return };
        let opts = RetimeOptions {
            offset_ms: -500,
            format: Some("ass".to_string()),
            ..Default::default()
        };
        let out =
            retime(ASS.as_bytes(), &opts, &MediaLimits::default(), &rt).expect("retime failed");
        let primary = out.primary.unwrap();
        let text = String::from_utf8_lossy(&primary.bytes);
        // Centiseconds: 0:00:01.00 - 0.5s → 0:00:00.50
        assert!(text.contains("0:00:00.50,0:00:02.00"), "{text}");
        assert!(text.contains("0:00:02.50,0:00:03.50"), "{text}");
        assert!(text.contains("[Script Info]"), "{text}");
    }

    #[test]
    fn rejects_empty_and_bad_format() {
        let Some(rt) = runtime() else { return };
        let err = retime(b"", &opts(0), &MediaLimits::default(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)));

        let bad = RetimeOptions {
            format: Some("mkv".to_string()),
            ..Default::default()
        };
        let err =
            retime(SRT.as_bytes(), &bad, &MediaLimits::default(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)));

        let huge = RetimeOptions {
            offset_ms: i64::MAX,
            ..Default::default()
        };
        let err =
            retime(SRT.as_bytes(), &huge, &MediaLimits::default(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)));
    }
}
