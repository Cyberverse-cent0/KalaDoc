//! Read and write the document's `/CreationDate` and `/ModDate`.
//!
//! Thin, focused wrappers over the metadata machinery:
//!
//! - [`get_document_dates`] reads the trailer `/Info` dictionary's
//!   `/CreationDate` and `/ModDate` entries through
//!   [`extract_metadata`], returning each date as a string. The
//!   normalized `YYYY-MM-DDTHH:MM:SS[Z|±HH:MM]` form is preferred;
//!   when a stored value does not match the PDF date grammar, the
//!   raw string is returned unchanged so callers still see what the
//!   file carries.
//! - [`set_document_dates`] writes the two dates by delegating to
//!   [`set_metadata`] with a [`MetadataPatch`] limited to the date
//!   fields. Both inputs accept the PDF date grammar
//!   (`D:YYYYMMDDHHmmSSOHH'mm'`) or RFC 3339; values are normalized
//!   to canonical PDF date form before writing, and unparseable
//!   input is rejected with [`PdfError::InvalidInput`]. Like every
//!   transforming op, encrypted input is rejected.
//!
//! Note: [`set_metadata`] always refreshes `/ModDate` — when
//! `mod_date` is `None` it stamps the current UTC time, so calling
//! [`set_document_dates`] with only `creation` still updates the
//! modification stamp.

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::extract_metadata::extract_metadata;
use super::load::load_pdf;
use super::load::reject_encrypted;
use super::set_metadata::{MetadataPatch, set_metadata};

/// The document's `/CreationDate` and `/ModDate` as stored in the
/// trailer `/Info` dictionary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocDates {
    /// `/CreationDate` — normalized `YYYY-MM-DDTHH:MM:SS[Z|±HH:MM]`
    /// form when parseable, otherwise the raw stored string.
    pub creation_date: Option<String>,
    /// `/ModDate` — same representation rules as `creation_date`.
    pub mod_date: Option<String>,
}

/// Read the document's creation and modification dates.
///
/// Encrypted input is rejected: the `/Info` strings cannot be
/// decoded without the key, so reporting empty dates would be
/// misleading.
pub fn get_document_dates(input: &[u8], limits: &EngineLimits) -> PdfResult<DocDates> {
    let doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let meta = extract_metadata(input, limits)?;
    let to_string =
        |d: Option<super::extract_metadata::PdfDate>| d.map(|date| date.parsed.unwrap_or(date.raw));
    Ok(DocDates {
        creation_date: to_string(meta.creation_date),
        mod_date: to_string(meta.modification_date),
    })
}

/// Write `/CreationDate` and `/ModDate` and return the serialized PDF.
///
/// `creation` maps to `/CreationDate`, `mod_date` to `/ModDate`;
/// either may be `None` to leave that entry untouched (with the
/// `/ModDate`-always-refreshed caveat noted in the module docs).
/// Accepted formats and normalization are those of
/// [`set_metadata`]: PDF date (`D:...`) or RFC 3339. Encrypted input
/// is rejected.
pub fn set_document_dates(
    input: &[u8],
    creation: Option<&str>,
    mod_date: Option<&str>,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let patch = MetadataPatch {
        creation_date: creation.map(str::to_string),
        mod_date: mod_date.map(str::to_string),
        ..Default::default()
    };
    set_metadata(input, &patch, limits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::error::PdfError;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages_and_title;

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    #[test]
    fn set_then_get_roundtrips_pdf_dates() {
        let pdf = pdf_with_pages_and_title(&["a", "b"], "Dates");
        let out = set_document_dates(
            &pdf,
            Some("D:20240115143000Z"),
            Some("2024-02-03T04:05:06Z"),
            &limits(),
        )
        .unwrap();
        let dates = get_document_dates(&out, &limits()).unwrap();
        assert_eq!(dates.creation_date.as_deref(), Some("2024-01-15T14:30:00Z"));
        assert_eq!(dates.mod_date.as_deref(), Some("2024-02-03T04:05:06Z"));
    }

    #[test]
    fn get_returns_none_for_missing_dates() {
        let pdf = pdf_with_pages_and_title(&["a"], "No dates");
        let dates = get_document_dates(&pdf, &limits()).unwrap();
        assert_eq!(dates.creation_date, None);
        assert_eq!(dates.mod_date, None);
    }

    #[test]
    fn set_rejects_bad_date() {
        let pdf = pdf_with_pages_and_title(&["a"], "Bad");
        let err = set_document_dates(&pdf, Some("not-a-date"), None, &limits()).unwrap_err();
        assert!(matches!(err, PdfError::InvalidInput(_)));
    }

    #[test]
    fn set_creation_only_still_stamps_mod_date() {
        let pdf = pdf_with_pages_and_title(&["a"], "Partial");
        let out = set_document_dates(&pdf, Some("2024-06-01"), None, &limits()).unwrap();
        let dates = get_document_dates(&out, &limits()).unwrap();
        assert_eq!(dates.creation_date.as_deref(), Some("2024-06-01T00:00:00"));
        // /ModDate is unconditionally refreshed by set_metadata.
        assert!(dates.mod_date.is_some());
    }

    #[test]
    fn garbage_input_errors_not_panics() {
        assert!(get_document_dates(b"not a pdf", &limits()).is_err());
        assert!(set_document_dates(b"not a pdf", None, None, &limits()).is_err());
    }
}
