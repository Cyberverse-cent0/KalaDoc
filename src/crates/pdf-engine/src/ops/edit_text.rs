//! Find-and-replace inside PDF page content streams.
//!
//! For every selected page, each `/Contents` stream is decompressed,
//! decoded into [`lopdf::content::Content`] operations, and every
//! text-showing operator — `Tj`, `TJ`, `'` (single quote) and `"`
//! (double quote) — has its string operands scanned for the search
//! term. Matching strings are rewritten **in place**: the operator,
//! its position in the stream, and every non-string operand (including
//! the numeric kerning entries of `TJ` arrays) are preserved exactly.
//!
//! # Matching model — read this before relying on the result
//!
//! - Matching is **byte-level** against the raw bytes stored in the
//!   content stream's string operands. Those bytes are in whatever
//!   encoding the page's font uses (usually WinAnsi/Latin-1 for simple
//!   fonts, arbitrary CMap codes for composite fonts). This op does
//!   *not* consult `/ToUnicode` CMaps, font encodings, or glyph IDs:
//!   text drawn with a custom-encoded or subset font may not match even
//!   when it is visually identical, and text extracted by
//!   [`extract_text`](super::text::extract_text) is *not* guaranteed to
//!   match what this op sees.
//! - `find` and `replace` must be encodable in **Latin-1** (every char
//!   ≤ U+00FF); each char becomes exactly one byte. Characters above
//!   U+00FF are rejected with [`PdfError::InvalidInput`] because no
//!   single-byte representation exists. There is no way to express
//!   "search for this Unicode text" — only byte patterns.
//! - Case-insensitive matching (`case_sensitive: false`) folds ASCII
//!   `A-Z` and the Latin-1 letters `À-Ö`, `Ø-Þ` to lowercase. It does
//!   not understand locale-specific or multi-char case folding.
//! - Replacement happens **inside** the string operand. If the
//!   replacement is wider or narrower than the search term, the text
//!   simply overflows or leaves a gap — **no reflow, re-spacing, or
//!   re-wrapping is attempted**, and line breaks (`Td`/`TD`/`T*`) are
//!   untouched.
//!
//! # Honesty notes
//!
//! - Only the page's own content streams are edited. Text inside Form
//!   XObjects (`Do`), Type3 glyph descriptions, tiling patterns,
//!   annotation appearance streams, and optional-content layers is
//!   **not** reached.
//! - A stream that fails to decompress or decode is left **untouched**
//!   and reported in [`EditReport::warnings`]; its text will not be
//!   edited.
//! - Rewritten streams are stored **uncompressed**: `/Filter` and
//!   `/DecodeParms` are dropped, matching `redact`'s convention.
//!   Output may be larger than the input.
//! - Zero matches is **not** an error: the (re-serialized) PDF is
//!   returned with `report.replacements == 0`.

use std::collections::{HashMap, HashSet};

use lopdf::Object;
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::watermark::selected_pages;

/// Operators whose operands contain text to be shown (and therefore
/// text to be searched). For `'` and `"` the string is the last
/// operand; for `TJ` it is nested inside the array operand.
const TEXT_SHOWING_OPS: [&str; 4] = ["Tj", "TJ", "'", "\""];

/// Decompression bound applied per content stream — same 64 MiB guard
/// `watermark` uses when merging page contents.
const MAX_CONTENT_STREAM: usize = 64 * 1024 * 1024;

/// Parameters for [`edit_text`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEdit {
    /// Byte pattern to find, encoded Latin-1 (one byte per char, all
    /// chars ≤ U+00FF). Must not be empty.
    pub find: String,
    /// Replacement text, same Latin-1 restriction. May be empty, which
    /// deletes the matched text.
    pub replace: String,
    /// Optional 1-based page range spec (`"1-3,8"`, `"all"`). `None`
    /// edits every page.
    pub pages: Option<String>,
    /// `true` (default): match exactly. `false`: ASCII + Latin-1
    /// case-insensitive matching.
    #[serde(default = "default_case_sensitive")]
    pub case_sensitive: bool,
}

fn default_case_sensitive() -> bool {
    true
}

impl Default for TextEdit {
    fn default() -> Self {
        Self {
            find: String::new(),
            replace: String::new(),
            pages: None,
            case_sensitive: true,
        }
    }
}

/// What [`edit_text`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditReport {
    /// Total number of individual string occurrences replaced across
    /// all edited streams (a `TJ` array with three matching strings
    /// counts three).
    pub replacements: u32,
    /// 1-based page numbers on which at least one replacement happened,
    /// sorted ascending.
    pub pages_affected: Vec<u32>,
    /// Human-readable notes: streams skipped because they failed to
    /// decode, shared-stream dedup notes, etc.
    pub warnings: Vec<String>,
}

/// Result of [`edit_text`]: the rewritten PDF plus its report.
#[derive(Debug, Clone)]
pub struct EditOutcome {
    /// The edited PDF, serialized.
    pub bytes: Vec<u8>,
    /// Counts and warnings for this run.
    pub report: EditReport,
}

/// Encode `text` as the single-byte pattern used for matching: each
/// char must be ≤ U+00FF and becomes one byte (a Latin-1/WinAnsi
/// approximation — glyph-level font encodings are not consulted).
fn latin1_bytes(text: &str) -> PdfResult<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len());
    for c in text.chars() {
        let cp = c as u32;
        if cp > 0xFF {
            return Err(PdfError::InvalidInput(format!(
                "edit_text only supports Latin-1 characters (<= U+00FF); \
                 found U+{cp:04X} in {text:?}. Text is matched at the byte \
                 level, so characters without a single-byte representation \
                 cannot be searched for or written."
            )));
        }
        out.push(cp as u8);
    }
    Ok(out)
}

/// Latin-1 lowercase fold for case-insensitive matching: ASCII
/// `A-Z` and `À-Ö`, `Ø-Þ` map to their lowercase counterparts. Other
/// bytes are unchanged (e.g. `ÿ`/`Ÿ` has no Latin-1 uppercase pair
/// handled here).
fn fold_byte(b: u8) -> u8 {
    match b {
        b'A'..=b'Z' => b + (b'a' - b'A'),
        0xC0..=0xD6 | 0xD8..=0xDE => b + 0x20,
        _ => b,
    }
}

#[inline]
fn bytes_match_at(haystack: &[u8], needle: &[u8], case_sensitive: bool) -> bool {
    if case_sensitive {
        haystack == needle
    } else {
        haystack
            .iter()
            .zip(needle.iter())
            .all(|(&h, &n)| fold_byte(h) == fold_byte(n))
    }
}

/// Replace every non-overlapping occurrence of `needle` in `haystack`
/// with `replacement`. Returns the new bytes and the occurrence count.
fn replace_all(
    haystack: &[u8],
    needle: &[u8],
    replacement: &[u8],
    case_sensitive: bool,
) -> (Vec<u8>, u32) {
    debug_assert!(!needle.is_empty());
    let mut out = Vec::with_capacity(haystack.len());
    let mut count = 0u32;
    let mut i = 0usize;
    while i + needle.len() <= haystack.len() {
        if bytes_match_at(&haystack[i..i + needle.len()], needle, case_sensitive) {
            out.extend_from_slice(replacement);
            i += needle.len();
            count += 1;
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out.extend_from_slice(&haystack[i..]);
    (out, count)
}

/// Recursively rewrite string operands (including strings nested in
/// `TJ` arrays), adding the number of replaced occurrences to `count`.
fn replace_in_operands(
    operands: &mut [Object],
    needle: &[u8],
    replacement: &[u8],
    case_sensitive: bool,
    count: &mut u32,
) {
    for obj in operands {
        match obj {
            Object::String(bytes, _) => {
                let (new_bytes, n) = replace_all(bytes, needle, replacement, case_sensitive);
                if n > 0 {
                    *bytes = new_bytes;
                    *count += n;
                }
            }
            Object::Array(items) => {
                replace_in_operands(items, needle, replacement, case_sensitive, count);
            }
            _ => {}
        }
    }
}

/// Decode, edit and rewrite a single content stream in place.
///
/// Returns the number of replacements made in this stream. Streams
/// that cannot be decompressed/decoded are left untouched; a warning
/// describing the skip is pushed onto `warnings`.
fn edit_stream(
    doc: &mut lopdf::Document,
    stream_id: lopdf::ObjectId,
    needle: &[u8],
    replacement: &[u8],
    case_sensitive: bool,
    warnings: &mut Vec<String>,
) -> PdfResult<u32> {
    let object = match doc.get_object_mut(stream_id) {
        Ok(o) => o,
        Err(_) => return Ok(0),
    };
    let stream = match object.as_stream_mut() {
        Ok(s) => s,
        Err(_) => return Ok(0),
    };

    // Prefer bounded decompression; if the filters fail, the raw bytes
    // may still be directly parseable (uncompressed stream).
    let raw = match stream.decompressed_content_with_limit(MAX_CONTENT_STREAM) {
        Ok(data) => data,
        Err(_) => stream.content.clone(),
    };
    let Ok(mut content) = lopdf::content::Content::decode(&raw) else {
        warnings.push(format!(
            "content stream {stream_id:?} could not be decoded; left unchanged"
        ));
        return Ok(0);
    };

    let mut count = 0u32;
    for op in &mut content.operations {
        if !TEXT_SHOWING_OPS.contains(&op.operator.as_str()) {
            continue;
        }
        replace_in_operands(&mut op.operands, needle, replacement, case_sensitive, &mut count);
    }
    if count == 0 {
        return Ok(0);
    }

    let encoded = lopdf::content::Content {
        operations: content.operations,
    }
    .encode()
    .map_err(lopdf_err)?;
    stream.content = encoded;
    // New bytes are uncompressed: drop filter entries so readers do not
    // try to decode them. /Length is rewritten on save.
    stream.dict.remove(b"Filter");
    stream.dict.remove(b"DecodeParms");
    Ok(count)
}

/// Find-and-replace `edit.find` with `edit.replace` inside the text
/// layer of `input`, returning the rewritten PDF and an [`EditReport`].
///
/// See the module docs for the byte-level matching model and its
/// limitations. Validation (empty `find`, non-Latin-1 characters,
/// malformed page range) happens before the document is mutated.
/// Encrypted documents are rejected. A document with zero matches is
/// returned unchanged in content (still re-serialized) with
/// `replacements: 0` — not an error.
pub fn edit_text(
    input: &[u8],
    edit: &TextEdit,
    limits: &EngineLimits,
) -> PdfResult<EditOutcome> {
    if edit.find.is_empty() {
        return Err(PdfError::InvalidInput(
            "edit_text requires a non-empty find string".to_string(),
        ));
    }
    let needle = latin1_bytes(&edit.find)?;
    let replacement = latin1_bytes(&edit.replace)?;

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let selected = selected_pages(&doc, edit.pages.as_deref())?;
    let pages_map = doc.get_pages();

    // Collect each selected page's content streams. A stream shared by
    // several pages (or listed twice) is edited exactly once — the
    // edit is baked into the stream itself — and every page that
    // references it counts as affected.
    let mut warnings = Vec::new();
    let mut stream_counts: HashMap<lopdf::ObjectId, u32> = HashMap::new();
    let mut page_streams: Vec<(u32, Vec<lopdf::ObjectId>)> = Vec::with_capacity(selected.len());
    let mut seen_streams: HashSet<lopdf::ObjectId> = HashSet::new();

    for &page_number in &selected {
        let Some(&page_id) = pages_map.get(&page_number) else {
            warnings.push(format!("page {page_number} not found; skipped"));
            continue;
        };
        let stream_ids = doc.get_page_contents(page_id);
        for &stream_id in &stream_ids {
            if seen_streams.insert(stream_id) {
                let n = edit_stream(
                    &mut doc,
                    stream_id,
                    &needle,
                    &replacement,
                    edit.case_sensitive,
                    &mut warnings,
                )?;
                stream_counts.insert(stream_id, n);
            }
        }
        page_streams.push((page_number, stream_ids));
    }

    let mut pages_affected: Vec<u32> = page_streams
        .iter()
        .filter(|(_, ids)| ids.iter().any(|id| stream_counts.get(id).copied().unwrap_or(0) > 0))
        .map(|(n, _)| *n)
        .collect();
    pages_affected.sort_unstable();
    pages_affected.dedup();

    let replacements: u32 = stream_counts.values().sum();
    let report = EditReport {
        replacements,
        pages_affected,
        warnings,
    };

    // Recompress like `watermark` does, keeping output size sane.
    doc.compress();
    let bytes = save_pdf(&mut doc)?;
    Ok(EditOutcome { bytes, report })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::extract_text;
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};

    fn edit(find: &str, replace: &str) -> TextEdit {
        TextEdit {
            find: find.to_string(),
            replace: replace.to_string(),
            pages: None,
            case_sensitive: true,
        }
    }

    #[test]
    fn replaces_text_and_reports() {
        let input = pdf_with_pages(&["Hello World", "Nothing here"]);
        let out = edit_text(&input, &edit("World", "There"), &EngineLimits::default()).unwrap();
        assert_eq!(out.report.replacements, 1);
        assert_eq!(out.report.pages_affected, vec![1]);
        assert_eq!(page_count(&out.bytes), 2);
        let texts = extract_text(&out.bytes, None, &EngineLimits::default()).unwrap();
        assert!(texts[0].text.contains("Hello There"));
        assert!(!texts[0].text.contains("World"));
        assert!(texts[1].text.contains("Nothing here"));
    }

    #[test]
    fn replaces_all_occurrences_in_one_string() {
        let input = pdf_with_pages(&["aaa"]);
        let out = edit_text(&input, &edit("a", "bb"), &EngineLimits::default()).unwrap();
        assert_eq!(out.report.replacements, 3);
        let texts = extract_text(&out.bytes, None, &EngineLimits::default()).unwrap();
        assert!(texts[0].text.contains("bbbbbb"));
    }

    #[test]
    fn case_insensitive_matching() {
        let input = pdf_with_pages(&["Hello HELLO hello"]);
        let e = TextEdit {
            case_sensitive: false,
            ..edit("hello", "bye")
        };
        let out = edit_text(&input, &e, &EngineLimits::default()).unwrap();
        assert_eq!(out.report.replacements, 3);
        let texts = extract_text(&out.bytes, None, &EngineLimits::default()).unwrap();
        assert_eq!(texts[0].text.trim(), "bye bye bye");
    }

    #[test]
    fn case_sensitive_by_default() {
        let input = pdf_with_pages(&["Hello hello"]);
        let out = edit_text(&input, &edit("hello", "x"), &EngineLimits::default()).unwrap();
        assert_eq!(out.report.replacements, 1);
    }

    #[test]
    fn empty_replacement_deletes() {
        let input = pdf_with_pages(&["foo bar baz"]);
        let out = edit_text(&input, &edit(" bar", ""), &EngineLimits::default()).unwrap();
        assert_eq!(out.report.replacements, 1);
        let texts = extract_text(&out.bytes, None, &EngineLimits::default()).unwrap();
        assert!(texts[0].text.contains("foo baz"));
    }

    #[test]
    fn zero_matches_returns_valid_pdf() {
        let input = pdf_with_pages(&["unchanged"]);
        let out = edit_text(&input, &edit("zzz", "yyy"), &EngineLimits::default()).unwrap();
        assert_eq!(out.report.replacements, 0);
        assert!(out.report.pages_affected.is_empty());
        assert_eq!(page_count(&out.bytes), 1);
        let texts = extract_text(&out.bytes, None, &EngineLimits::default()).unwrap();
        assert!(texts[0].text.contains("unchanged"));
    }

    #[test]
    fn respects_page_range() {
        let input = pdf_with_pages(&["target one", "target two"]);
        let e = TextEdit {
            pages: Some("2".to_string()),
            ..edit("target", "hit")
        };
        let out = edit_text(&input, &e, &EngineLimits::default()).unwrap();
        assert_eq!(out.report.replacements, 1);
        assert_eq!(out.report.pages_affected, vec![2]);
        let texts = extract_text(&out.bytes, None, &EngineLimits::default()).unwrap();
        assert!(texts[0].text.contains("target one"));
        assert!(texts[1].text.contains("hit two"));
    }

    #[test]
    fn rejects_empty_find() {
        let input = pdf_with_pages(&["x"]);
        assert!(edit_text(&input, &edit("", "y"), &EngineLimits::default()).is_err());
    }

    #[test]
    fn rejects_non_latin1() {
        let input = pdf_with_pages(&["x"]);
        assert!(edit_text(&input, &edit("héllo→", "y"), &EngineLimits::default()).is_err());
        assert!(edit_text(&input, &edit("hi", "→"), &EngineLimits::default()).is_err());
        // é (U+00E9) is fine.
        let ok = edit_text(&input, &edit("q", "héllo"), &EngineLimits::default()).unwrap();
        assert_eq!(ok.report.replacements, 0);
    }

    #[test]
    fn latin1_case_fold() {
        // 'É' is U+00C9 → folds to 'é' U+00E9.
        assert_eq!(fold_byte(0xC9), 0xE9);
        assert_eq!(fold_byte(b'A'), b'a');
        assert_eq!(fold_byte(b'5'), b'5');
        let (out, n) = replace_all(b"\xC9t\xE9", b"\xE9", b"e", false);
        assert_eq!(n, 2);
        assert_eq!(out, b"ete");
    }

    #[test]
    fn replace_in_tj_array_strings() {
        // Simulate a TJ array operand containing strings and kern numbers.
        let mut operands = vec![Object::Array(vec![
            Object::string_literal("foo"),
            Object::Integer(120),
            Object::string_literal("bar"),
        ])];
        let mut count = 0u32;
        replace_in_operands(&mut operands, b"ba", b"XY", true, &mut count);
        assert_eq!(count, 1);
        let Object::Array(items) = &operands[0] else {
            panic!("expected array");
        };
        assert_eq!(items[0].as_str().unwrap(), b"foo");
        assert_eq!(items[1].as_i64().unwrap(), 120);
        assert_eq!(items[2].as_str().unwrap(), b"XYr");
    }

    #[test]
    fn non_overlapping_replacement() {
        // find "aa" in "aaa" → one replacement at offset 0, leftover "a".
        let (out, n) = replace_all(b"aaa", b"aa", b"b", true);
        assert_eq!(n, 1);
        assert_eq!(out, b"ba");
    }
}
