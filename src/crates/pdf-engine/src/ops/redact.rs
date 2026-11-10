//! Best-effort text redaction.
//!
//! For each selected page, every content stream is decoded into
//! [`lopdf::content::Content`] operations. Text-showing operators —
//! `Tj`, `TJ`, `'` and `"` — whose string operands (literal *or* hex
//! strings, including strings nested in `TJ` arrays) decode to text
//! containing any of `terms` (case-insensitive substring match) are
//! removed entirely, and the page's content stream is rewritten.
//!
//! # Honesty notes
//!
//! - This removes the *text-showing operations*, not a guaranteed visual
//!   region. The same glyphs could still be produced by means this pass
//!   does not inspect: Type3 fonts, tiling patterns, XObjects with their
//!   own content streams, or inline images. Redaction here is
//!   best-effort and must not be relied on for security-sensitive
//!   document sanitization.
//! - Document metadata (`/Info` strings, XMP metadata streams, embedded
//!   files, annotations) is **not** scrubbed — out of scope.
//! - Rewritten content streams are stored **uncompressed**: the `/Filter`
//!   and `/DecodeParms` entries are dropped and the raw re-encoded bytes
//!   are written directly. Output may be larger than the input.
//! - A content stream that fails to decode is left untouched rather than
//!   corrupted; terms it contains will not be redacted.

use lopdf::content::{Content, Operation};
use lopdf::{Object, decode_text_string};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::pages::parse_page_range;

/// Operators that show text on the page.
const TEXT_SHOWING_OPS: [&str; 4] = ["Tj", "TJ", "'", "\""];

/// Redact every occurrence of `terms` from the text layer of `input`.
///
/// `terms` is a list of case-insensitive substrings; an empty list (or a
/// list containing only empty/whitespace terms) is rejected. `pages` is
/// an optional 1-based range spec (`"1-3,8"`, `"all"`); `None` redacts
/// every page. Encrypted documents are rejected.
pub fn redact(
    input: &[u8],
    terms: &[String],
    pages: Option<&str>,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    if terms.is_empty() {
        return Err(PdfError::InvalidInput(
            "redact requires at least one search term".to_string(),
        ));
    }
    let needles: Vec<String> = terms.iter().map(|t| t.trim().to_lowercase()).collect();
    if needles.iter().any(|n| n.is_empty()) {
        return Err(PdfError::InvalidInput(
            "redact terms must not be empty".to_string(),
        ));
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let selected = match pages {
        Some(spec) => parse_page_range(spec, page_count)?,
        None => (1..=page_count).collect(),
    };

    let pages_map = doc.get_pages();
    for page_number in selected {
        let Some(&page_id) = pages_map.get(&page_number) else {
            continue;
        };
        for stream_id in doc.get_page_contents(page_id) {
            redact_stream(&mut doc, stream_id, &needles)?;
        }
    }

    save_pdf(&mut doc)
}

/// Decode, filter and rewrite a single content stream in place.
fn redact_stream(
    doc: &mut lopdf::Document,
    stream_id: lopdf::ObjectId,
    needles: &[String],
) -> PdfResult<()> {
    let Ok(object) = doc.get_object_mut(stream_id) else {
        return Ok(());
    };
    let Ok(stream) = object.as_stream_mut() else {
        return Ok(());
    };

    // Prefer the decompressed bytes; if filter decoding fails, try the
    // raw content (it may already be uncompressed).
    let raw = match stream.decompressed_content() {
        Ok(data) => data,
        Err(_) => stream.content.clone(),
    };
    let Ok(content) = Content::decode(&raw) else {
        // Cannot safely parse this stream — leave it untouched.
        return Ok(());
    };

    let original_len = content.operations.len();
    let kept: Vec<Operation> = content
        .operations
        .into_iter()
        .filter(|op| !op_shows_redacted_text(op, needles))
        .collect();
    if kept.len() == original_len {
        return Ok(());
    }

    let encoded = Content { operations: kept }.encode().map_err(lopdf_err)?;
    stream.content = encoded;
    // The new bytes are uncompressed: drop the filter entries so readers
    // do not try to decode them. /Length is rewritten on save.
    stream.dict.remove(b"Filter");
    stream.dict.remove(b"DecodeParms");
    Ok(())
}

/// Whether `op` is a text-showing operator with a string operand that
/// decodes to text containing any needle.
fn op_shows_redacted_text(op: &Operation, needles: &[String]) -> bool {
    if !TEXT_SHOWING_OPS.contains(&op.operator.as_str()) {
        return false;
    }
    operands_contain_term(&op.operands, needles)
}

/// Recursively scan operands (strings may be nested inside `TJ` arrays).
fn operands_contain_term(objects: &[Object], needles: &[String]) -> bool {
    objects.iter().any(|object| match object {
        Object::String(..) => decode_text_string(object)
            .map(|text| {
                let lowered = text.to_lowercase();
                needles.iter().any(|n| lowered.contains(n))
            })
            .unwrap_or(false),
        Object::Array(items) => operands_contain_term(items, needles),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::extract_text;
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};

    fn unlimited() -> EngineLimits {
        EngineLimits {
            max_input_bytes: None,
            max_pages: None,
            max_pixels: None,
            max_outputs: None,
            max_duration_secs: None,
        }
    }

    #[test]
    fn removes_matching_text_op() {
        let input = pdf_with_pages(&["Hello World", "Nothing to hide"]);
        let out = redact(&input, &["World".to_string()], None, &unlimited()).unwrap();
        assert_eq!(page_count(&out), 2);
        let texts = extract_text(&out, None, &unlimited()).unwrap();
        assert!(!texts[0].text.contains("World"));
        assert!(texts[1].text.contains("Nothing to hide"));
    }

    #[test]
    fn respects_page_range() {
        let input = pdf_with_pages(&["Secret one", "Secret two"]);
        let out = redact(&input, &["secret".to_string()], Some("2"), &unlimited()).unwrap();
        let texts = extract_text(&out, None, &unlimited()).unwrap();
        assert!(texts[0].text.contains("Secret one"));
        assert!(!texts[1].text.contains("Secret two"));
    }

    #[test]
    fn rejects_empty_terms() {
        let input = pdf_with_pages(&["Hello"]);
        assert!(redact(&input, &[], None, &unlimited()).is_err());
        assert!(redact(&input, &["  ".to_string()], None, &unlimited()).is_err());
    }
}
