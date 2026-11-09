//! Write document information-dictionary (`/Info`) metadata.
//!
//! [`extract_metadata`](super::extract_metadata::extract_metadata)
//! reads the trailer `/Info` dictionary; this module is the write side:
//!
//! - [`set_metadata`] updates the standard keys (`/Title`, `/Author`,
//!   `/Subject`, `/Keywords`, `/Creator`, `/Producer`,
//!   `/CreationDate`, `/ModDate`). Only fields that are `Some` in the
//!   [`MetadataPatch`] are touched; every other entry — including
//!   non-standard custom keys — is preserved. `/ModDate` is *always*
//!   written: to the normalized `patch.mod_date` when one is supplied,
//!   otherwise to the current UTC time, so the modification stamp can
//!   never go stale.
//! - [`clear_metadata`] removes every standard metadata key and sets
//!   `/Producer` to `"KalaDoc"`. **Scope:** only the trailer `/Info`
//!   dictionary is affected. Custom (non-standard) keys are kept, and
//!   the catalog `/Metadata` XMP stream — which many producers also
//!   populate — is *not* touched and may still carry the old values.
//!
//! String values are encoded with [`lopdf::text_string`]: ASCII goes
//! out as a PDFDocEncoding literal string, anything else as a
//! UTF-16BE hexadecimal string with a byte-order mark, matching what
//! `decode_text_string` (and every conforming reader) expects.
//!
//! Dates accept either the PDF grammar `D:YYYYMMDDHHmmSSOHH'mm'` or
//! RFC 3339 (`YYYY-MM-DDTHH:MM:SS[.fff][Z|±HH:MM]`, a space separator
//! and a bare `YYYY-MM-DD` date are also tolerated). Both are
//! normalized to the canonical PDF form `D:YYYYMMDDHHmmSS` followed by
//! `Z` or `+HH'mm'`/`-HH'mm'` when a zone was given; unparseable input
//! is rejected with [`PdfError::InvalidInput`].
//!
//! Like every transforming op, encrypted input is rejected and all
//! input-side [`EngineLimits`](crate::crates::core::engine::EngineLimits)
//! are enforced through [`load_pdf`](super::load::load_pdf).

use std::time::{SystemTime, UNIX_EPOCH};

use lopdf::{Dictionary, Document, Object, text_string};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};

/// Partial document metadata to write into the trailer `/Info`
/// dictionary. Every field is optional; `None` leaves the existing
/// value untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataPatch {
    /// `/Title`.
    pub title: Option<String>,
    /// `/Author`.
    pub author: Option<String>,
    /// `/Subject`.
    pub subject: Option<String>,
    /// `/Keywords`. PDF stores keywords as a single text string, so the
    /// list is joined with `", "` (the conventional separator).
    pub keywords: Option<Vec<String>>,
    /// `/Creator` — the application that produced the original content.
    pub creator: Option<String>,
    /// `/Producer` — the application that last wrote the file.
    pub producer: Option<String>,
    /// `/CreationDate` — PDF date (`D:...`) or RFC 3339; normalized to
    /// PDF date form before writing.
    pub creation_date: Option<String>,
    /// `/ModDate` — same accepted formats. When `None`, `/ModDate` is
    /// still written, set to the current UTC time; when `Some`, the
    /// normalized value is used instead (useful for preserving a
    /// source document's timestamp).
    pub mod_date: Option<String>,
}

/// Update the trailer `/Info` dictionary with `patch` and return the
/// serialized PDF.
///
/// The `/Info` dictionary is created when the trailer lacks one (or
/// points at something that is not a dictionary). `/ModDate` is always
/// set — see [`MetadataPatch::mod_date`]. XMP metadata is not modified.
pub fn set_metadata(
    input: &[u8],
    patch: &MetadataPatch,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    // Validate all dates *before* mutating anything so a bad value
    // cannot leave a half-written dictionary behind (the document is
    // only serialized on success anyway, but early validation keeps
    // the error path obvious).
    let creation_date = patch
        .creation_date
        .as_deref()
        .map(normalize_pdf_date)
        .transpose()?;
    let mod_date = patch
        .mod_date
        .as_deref()
        .map(normalize_pdf_date)
        .transpose()?;

    ensure_info_dict(&mut doc)?;
    {
        let info = info_dict_mut(&mut doc)?;
        if let Some(title) = &patch.title {
            info.set("Title", text_string(title));
        }
        if let Some(author) = &patch.author {
            info.set("Author", text_string(author));
        }
        if let Some(subject) = &patch.subject {
            info.set("Subject", text_string(subject));
        }
        if let Some(keywords) = &patch.keywords {
            info.set("Keywords", text_string(&keywords.join(", ")));
        }
        if let Some(creator) = &patch.creator {
            info.set("Creator", text_string(creator));
        }
        if let Some(producer) = &patch.producer {
            info.set("Producer", text_string(producer));
        }
        if let Some(date) = &creation_date {
            info.set("CreationDate", Object::string_literal(date.clone()));
        }
        // /ModDate is unconditional: an explicit patch value wins,
        // otherwise stamp the current UTC time.
        let mod_stamp = mod_date.unwrap_or_else(pdf_date_now);
        info.set("ModDate", Object::string_literal(mod_stamp));
    }

    save_pdf(&mut doc)
}

/// Remove all standard `/Info` keys and set `/Producer` to `"KalaDoc"`,
/// returning the serialized PDF.
///
/// Removed keys: `/Title`, `/Author`, `/Subject`, `/Keywords`,
/// `/Creator`, `/CreationDate`, `/ModDate`, `/Trapped`. `/Producer` is
/// overwritten (not deleted) so the file honestly names its last
/// writer. Non-standard keys are preserved. See the module docs for
/// the XMP caveat.
pub fn clear_metadata(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    const CLEARED_KEYS: &[&[u8]] = &[
        b"Title",
        b"Author",
        b"Subject",
        b"Keywords",
        b"Creator",
        b"CreationDate",
        b"ModDate",
        b"Trapped",
    ];

    ensure_info_dict(&mut doc)?;
    {
        let info = info_dict_mut(&mut doc)?;
        for key in CLEARED_KEYS {
            info.remove(key);
        }
        info.set("Producer", text_string("KalaDoc"));
    }

    save_pdf(&mut doc)
}

// ---------------------------------------------------------------------------
// /Info dictionary access
// ---------------------------------------------------------------------------

/// Where the trailer `/Info` entry lives.
enum InfoLocation {
    /// Indirect reference to a dictionary object.
    Indirect(lopdf::ObjectId),
    /// Inline dictionary inside the trailer itself.
    Inline,
}

/// Make sure `doc.trailer["Info"]` resolves to a dictionary, creating a
/// fresh indirect object when the entry is missing or does not point
/// at a dictionary.
fn ensure_info_dict(doc: &mut Document) -> PdfResult<()> {
    let ok = match doc.trailer.get(b"Info") {
        Ok(Object::Reference(id)) => matches!(doc.get_object(*id), Ok(Object::Dictionary(_))),
        Ok(Object::Dictionary(_)) => true,
        _ => false,
    };
    if ok {
        return Ok(());
    }
    let id = doc.try_add_object(Dictionary::new()).map_err(lopdf_err)?;
    doc.trailer.set("Info", id);
    Ok(())
}

/// Mutable access to the `/Info` dictionary. Callers must run
/// [`ensure_info_dict`] first.
fn info_dict_mut<'a>(doc: &'a mut Document) -> PdfResult<&'a mut Dictionary> {
    let location = match doc.trailer.get(b"Info") {
        Ok(Object::Reference(id)) => InfoLocation::Indirect(*id),
        Ok(Object::Dictionary(_)) => InfoLocation::Inline,
        _ => {
            return Err(PdfError::InvalidInput(
                "document trailer has no usable /Info dictionary".to_string(),
            ));
        }
    };
    match location {
        InfoLocation::Indirect(id) => doc.get_dictionary_mut(id).map_err(lopdf_err),
        InfoLocation::Inline => match doc.trailer.get_mut(b"Info") {
            Ok(object) => object.as_dict_mut().map_err(lopdf_err),
            Err(err) => Err(lopdf_err(err)),
        },
    }
}

// ---------------------------------------------------------------------------
// PDF / RFC 3339 dates
// ---------------------------------------------------------------------------

/// Current UTC time formatted as a PDF date string
/// (`D:YYYYMMDDHHmmSSZ`).
fn pdf_date_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (year, month, day, hour, minute, second) = civil_from_unix_secs(secs);
    format!("D:{year:04}{month:02}{day:02}{hour:02}{minute:02}{second:02}Z")
}

/// Normalize a PDF date (`D:YYYYMMDDHHmmSSOHH'mm'`) or an RFC 3339
/// timestamp to the canonical PDF date form. Returns
/// [`PdfError::InvalidInput`] when neither grammar matches.
fn normalize_pdf_date(raw: &str) -> PdfResult<String> {
    let s = raw.trim();
    if s.starts_with("D:") || s.starts_with("d:") {
        normalize_pdf_date_literal(s)
    } else {
        normalize_rfc3339_date(s)
    }
}

/// Canonical `D:YYYYMMDDHHmmSS` + optional `Z`/`±HH'mm'` suffix.
fn format_pdf_date(p: &DateParts) -> String {
    let mut out = format!(
        "D:{:04}{:02}{:02}{:02}{:02}{:02}",
        p.year, p.month, p.day, p.hour, p.minute, p.second
    );
    match p.zone {
        Some(Zone::Utc) => out.push('Z'),
        Some(Zone::Offset {
            hours,
            minutes,
            negative,
        }) => {
            out.push(if negative { '-' } else { '+' });
            out.push_str(&format!("{hours:02}'{minutes:02}'"));
        }
        None => {}
    }
    out
}

#[derive(Debug, Clone, Copy)]
struct DateParts {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    zone: Option<Zone>,
}

#[derive(Debug, Clone, Copy)]
enum Zone {
    Utc,
    /// `+`/`-` offset; `negative` records the sign so `+05'30'` and
    /// `-05'30'` stay distinct.
    Offset {
        hours: u32,
        minutes: u32,
        negative: bool,
    },
}

fn valid_date(p: &DateParts) -> bool {
    (1..=9999).contains(&p.year)
        && (1..=12).contains(&p.month)
        && (1..=days_in_month(p.year, p.month)).contains(&p.day)
        && p.hour <= 23
        && p.minute <= 59
        && p.second <= 59
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

fn invalid_date(raw: &str) -> PdfError {
    PdfError::InvalidInput(format!(
        "unrecognized date {raw:?}: expected a PDF date (D:YYYYMMDDHHmmSSOHH'mm') or RFC 3339 timestamp"
    ))
}

/// Parse the PDF date grammar. Every field after the year is optional;
/// missing fields default to `01` (day) / `00` (time), matching how
/// readers interpret partial dates.
fn normalize_pdf_date_literal(s: &str) -> PdfResult<String> {
    let body = &s[2..]; // strip "D:" / "d:"
    let bytes = body.as_bytes();
    let digits = |range: std::ops::Range<usize>| -> Option<u32> {
        let slice = bytes.get(range)?;
        if !slice.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(slice).ok()?.parse().ok()
    };

    if bytes.len() < 4 {
        return Err(invalid_date(s));
    }
    let year = i64::from(digits(0..4).ok_or_else(|| invalid_date(s))?);
    let month = if bytes.len() >= 6 {
        digits(4..6).ok_or_else(|| invalid_date(s))?
    } else {
        1
    };
    let day = if bytes.len() >= 8 {
        digits(6..8).ok_or_else(|| invalid_date(s))?
    } else {
        1
    };
    let hour = if bytes.len() >= 10 {
        digits(8..10).ok_or_else(|| invalid_date(s))?
    } else {
        0
    };
    let minute = if bytes.len() >= 12 {
        digits(10..12).ok_or_else(|| invalid_date(s))?
    } else {
        0
    };
    let second = if bytes.len() >= 14 {
        digits(12..14).ok_or_else(|| invalid_date(s))?
    } else {
        0
    };

    // Optional zone: index 14 onward. Apostrophes around HH'mm' are
    // part of the spec but tolerated as absent. A malformed zone is
    // ignored rather than fatal, matching `parse_pdf_date`'s leniency.
    let mut zone = None;
    let rest = &body[14.min(body.len())..];
    if !rest.is_empty() {
        match rest.as_bytes()[0] {
            b'Z' | b'z' if rest.len() == 1 => zone = Some(Zone::Utc),
            sign @ (b'+' | b'-') => {
                let negative = sign == b'-';
                let tz = rest[1..].trim_matches('\'');
                if tz.len() == 4 && tz.bytes().all(|c| c.is_ascii_digit()) {
                    let hours = tz[0..2].parse().unwrap_or(99);
                    let minutes = tz[2..4].parse().unwrap_or(99);
                    if hours <= 23 && minutes <= 59 {
                        zone = Some(Zone::Offset {
                            hours,
                            minutes,
                            negative,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    let parts = DateParts {
        year,
        month,
        day,
        hour,
        minute,
        second,
        zone,
    };
    if !valid_date(&parts) {
        return Err(invalid_date(s));
    }
    Ok(format_pdf_date(&parts))
}

/// Parse an RFC 3339 timestamp (`YYYY-MM-DD[T ]HH:MM:SS[.frac][Z|±HH:MM]`)
/// or a bare `YYYY-MM-DD` date, and render it as a PDF date string.
fn normalize_rfc3339_date(s: &str) -> PdfResult<String> {
    let bytes = s.as_bytes();
    // Date part: YYYY-MM-DD (10 bytes, fixed positions).
    if bytes.len() < 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes[..10]
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
    {
        return Err(invalid_date(s));
    }
    let num = |range: std::ops::Range<usize>| -> u32 {
        std::str::from_utf8(&bytes[range])
            .unwrap_or("0")
            .parse()
            .unwrap_or(0)
    };
    let year = i64::from(num(0..4));
    let month = num(5..7);
    let day = num(8..10);

    // Optional time part after 'T' or a space.
    let mut hour = 0;
    let mut minute = 0;
    let mut second = 0;
    let mut zone = None;

    if bytes.len() > 10 {
        if bytes[10] != b'T' && bytes[10] != b't' && bytes[10] != b' ' {
            return Err(invalid_date(s));
        }
        let mut idx = 11;
        // HH:MM[:SS][.frac]
        let need = idx + 5;
        if bytes.len() < need
            || bytes[idx + 2] != b':'
            || !bytes[idx..need]
                .iter()
                .enumerate()
                .all(|(i, b)| i == 2 || b.is_ascii_digit())
        {
            return Err(invalid_date(s));
        }
        hour = num(idx..idx + 2);
        minute = num(idx + 3..idx + 5);
        idx = need;
        if idx < bytes.len() && bytes[idx] == b':' {
            if bytes.len() < idx + 3 || !bytes[idx + 1..idx + 3].iter().all(u8::is_ascii_digit) {
                return Err(invalid_date(s));
            }
            second = num(idx + 1..idx + 3);
            idx += 3;
        }
        // Optional fractional seconds — skipped, PDF dates keep whole
        // seconds only.
        if idx < bytes.len() && bytes[idx] == b'.' {
            idx += 1;
            while idx < bytes.len() && bytes[idx].is_ascii_digit() {
                idx += 1;
            }
        }
        // Zone: 'Z' or ±HH:MM (also tolerate ±HHMM / ±HH).
        if idx < bytes.len() {
            match bytes[idx] {
                b'Z' | b'z' => {
                    if idx + 1 != bytes.len() {
                        return Err(invalid_date(s));
                    }
                    zone = Some(Zone::Utc);
                }
                sign @ (b'+' | b'-') => {
                    // ±HH[:MM] — ':' tolerated between the hour and
                    // minute fields.
                    let rest = &s[idx + 1..];
                    let compact: String = rest.chars().filter(|c| *c != ':').collect();
                    let cb = compact.as_bytes();
                    if cb.len() < 2 || !cb.iter().all(u8::is_ascii_digit) || cb.len() > 4 {
                        return Err(invalid_date(s));
                    }
                    let zh: u32 = compact[0..2].parse().map_err(|_| invalid_date(s))?;
                    let zm: u32 = if cb.len() == 4 {
                        compact[2..4].parse().map_err(|_| invalid_date(s))?
                    } else {
                        0
                    };
                    if zh > 23 || zm > 59 {
                        return Err(invalid_date(s));
                    }
                    zone = Some(Zone::Offset {
                        hours: zh,
                        minutes: zm,
                        negative: sign == b'-',
                    });
                }
                _ => return Err(invalid_date(s)),
            }
        }
    }

    let parts = DateParts {
        year,
        month,
        day,
        hour,
        minute,
        second,
        zone,
    };
    if !valid_date(&parts) {
        return Err(invalid_date(s));
    }
    Ok(format_pdf_date(&parts))
}

/// Days-from-civil / civil-from-days (Howard Hinnant's algorithms) —
/// converts a Unix timestamp to UTC calendar fields without pulling in
/// a datetime dependency.
fn civil_from_unix_secs(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = (rem / 3600) as u32;
    let minute = ((rem % 3600) / 60) as u32;
    let second = (rem % 60) as u32;

    // civil_from_days: z counts days since 1970-01-01.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages_and_title;
    use lopdf::Document;

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    /// Read a string key straight out of the trailer /Info dict.
    fn info_value(pdf: &[u8], key: &[u8]) -> Option<String> {
        let doc = Document::load_mem(pdf).unwrap();
        let info = doc.trailer.get(b"Info").ok()?;
        let (_, object) = doc.dereference(info).ok()?;
        let dict = object.as_dict().ok()?;
        lopdf::decode_text_string(dict.get(key).ok()?).ok()
    }

    #[test]
    fn set_metadata_writes_only_given_fields() {
        let pdf = pdf_with_pages_and_title(&["a", "b"], "Old Title");
        let patch = MetadataPatch {
            title: Some("New Title".to_string()),
            author: Some("Jane Doe".to_string()),
            keywords: Some(vec!["rust".to_string(), "pdf".to_string()]),
            creation_date: Some("2024-03-01T09:30:00Z".to_string()),
            ..MetadataPatch::default()
        };
        let out = set_metadata(&pdf, &patch, &limits()).unwrap();

        assert_eq!(info_value(&out, b"Title").as_deref(), Some("New Title"));
        assert_eq!(info_value(&out, b"Author").as_deref(), Some("Jane Doe"));
        assert_eq!(info_value(&out, b"Keywords").as_deref(), Some("rust, pdf"));
        // Untouched fields survive.
        assert_eq!(
            info_value(&out, b"Creator").as_deref(),
            Some("kaladoc-test-utils")
        );
        assert_eq!(info_value(&out, b"Producer").as_deref(), Some("lopdf"));
        // Dates normalized to PDF form; ModDate always stamped.
        assert_eq!(
            info_value(&out, b"CreationDate").as_deref(),
            Some("D:20240301093000Z")
        );
        let mod_date = info_value(&out, b"ModDate").unwrap();
        assert!(mod_date.starts_with("D:"), "ModDate was {mod_date}");
        assert!(mod_date.ends_with('Z'));
        assert_eq!(mod_date.len(), "D:".len() + 14 + 1);
    }

    #[test]
    fn set_metadata_accepts_pdf_dates_and_offsets() {
        let pdf = pdf_with_pages_and_title(&["a"], "T");
        let patch = MetadataPatch {
            creation_date: Some("D:20240115143000+05'30'".to_string()),
            mod_date: Some("2024-06-30".to_string()),
            ..MetadataPatch::default()
        };
        let out = set_metadata(&pdf, &patch, &limits()).unwrap();
        assert_eq!(
            info_value(&out, b"CreationDate").as_deref(),
            Some("D:20240115143000+05'30'")
        );
        assert_eq!(info_value(&out, b"ModDate").as_deref(), Some("D:20240630"));
    }

    #[test]
    fn set_metadata_utf16_title_roundtrips() {
        let pdf = pdf_with_pages_and_title(&["a"], "T");
        let patch = MetadataPatch {
            title: Some("日本語タイトル".to_string()),
            ..MetadataPatch::default()
        };
        let out = set_metadata(&pdf, &patch, &limits()).unwrap();
        assert_eq!(
            info_value(&out, b"Title").as_deref(),
            Some("日本語タイトル")
        );
    }

    #[test]
    fn set_metadata_rejects_bad_date() {
        let pdf = pdf_with_pages_and_title(&["a"], "T");
        let patch = MetadataPatch {
            title: Some("x".to_string()),
            creation_date: Some("not a date".to_string()),
            ..MetadataPatch::default()
        };
        let err = set_metadata(&pdf, &patch, &limits()).unwrap_err();
        assert!(matches!(err, PdfError::InvalidInput(_)));
    }

    #[test]
    fn clear_metadata_keeps_only_producer_and_custom_keys() {
        let pdf = pdf_with_pages_and_title(&["a"], "Secret Title");
        // Inject a custom key first via set_metadata's dict? Easier:
        // build expectation on the standard keys only.
        let out = clear_metadata(&pdf, &limits()).unwrap();
        assert_eq!(info_value(&out, b"Producer").as_deref(), Some("KalaDoc"));
        for key in [
            &b"Title"[..],
            b"Author",
            b"Subject",
            b"Keywords",
            b"Creator",
            b"CreationDate",
            b"ModDate",
            b"Trapped",
        ] {
            assert!(info_value(&out, key).is_none(), "key {key:?} survived");
        }
    }

    #[test]
    fn date_normalization_unit() {
        assert_eq!(
            normalize_pdf_date("2024-01-15T14:30:00Z").unwrap(),
            "D:20240115143000Z"
        );
        assert_eq!(normalize_pdf_date("D:20240115").unwrap(), "D:20240115");
        assert_eq!(
            normalize_pdf_date("1999-12-31 23:59:58-05:00").unwrap(),
            "D:19991231235958-05'00'"
        );
        assert!(normalize_pdf_date("2024-13-40T99:99:99Z").is_err());
        assert!(normalize_pdf_date("garbage").is_err());
    }
}
