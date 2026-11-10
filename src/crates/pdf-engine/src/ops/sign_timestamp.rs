//! Document-timestamp (`DocTimeStamp`) inspection — **read-only**.
//!
//! [`list_signature_timestamps`] reports, for every signature field a
//! document carries, whether that signature embeds a document timestamp:
//!
//! - **Signature timestamps** — `/Reference` entries inside the
//!   signature dictionary whose `/TransformMethod` is `/DocTimeStamp`
//!   (or whose `/TransformParams` declares `/Type /DocTimeStamp`). These
//!   are the RFC 3161-style timestamps bound to a signature's byte
//!   range via a signature-reference transform.
//! - **Standalone document timestamps** — a signature dictionary stored
//!   under the catalog's `/Perms << /DocTimeStamp … >>`. It does not
//!   sign the byte range at all; it *is* the timestamp. It is reported
//!   as an extra entry after the AcroForm/`/Perms`-reference signatures
//!   (same convention `find_signatures` uses for ordering).
//!
//! This complements [`verify_signatures`](super::verify_signature::verify_signatures):
//! that op reports *structural* signature facts and detects embedded
//! timestamp tokens only as an opaque OID-presence check inside the CMS
//! blob (`timestamp_token_present`). This op reads the PDF-level
//! `/Reference` transform declarations instead — it does **not**
//! validate the timestamp token cryptographically, does not check the
//! TSA certificate chain, and does not prove the claimed time is
//! trustworthy.
//!
//! **Adding timestamps is intentionally not implemented.** A real
//! `DocTimeStamp` requires an RFC 3161 TSA round-trip (`openssl ts`
//! against a live authority) plus signature-reference transform
//! bookkeeping; `sign`'s internals are private and `SignOptions::
//! timestamp_authority` already returns
//! [`PdfError::UnsupportedOperation`](crate::crates::pdf_engine::PdfError::UnsupportedOperation).
//! [`has_timestamps`] exists so callers can cheaply gate on the
//! presence of timestamps before deciding whether deeper inspection is
//! worthwhile.
//!
//! Encrypted input is tolerated like `verify_signatures`: dictionary
//! keys are never encrypted, so detection works, but `/M` and `/T`
//! string values decode to `None`.

use lopdf::{Dictionary, Document, Object, decode_text_string};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::load_pdf;
use super::verify_signature::is_signature_field;

/// Maximum depth followed through field `/Kids` chains — mirrors
/// `verify_signature`'s guard against cyclic field trees.
const MAX_FIELD_DEPTH: usize = 32;

/// Timestamp facts extracted from one signature's `/Reference` array.
#[derive(Debug, Default)]
struct TimestampRef {
    /// `/SubFilter` name when declared (typically `ETSI.RFC3161`).
    sub_filter: Option<String>,
    /// `/M` time string from the timestamp transform parameters.
    time: Option<String>,
}

/// Timestamp status of one signature field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedTimestamp {
    /// Index of the signature in document traversal order — the same
    /// order [`verify_signatures`](super::verify_signature::verify_signatures)
    /// reports. A standalone `/Perms /DocTimeStamp` signature is listed
    /// after all AcroForm/`/Perms`-reference signatures.
    pub signature_index: usize,
    /// Whether this signature carries a document timestamp: either a
    /// `/Reference` entry with a `/DocTimeStamp` transform, or the
    /// signature itself being the catalog-level `/DocTimeStamp` entry.
    pub has_timestamp: bool,
    /// Timestamp sub-filter (e.g. `ETSI.RFC3161`) when declared — taken
    /// from the transform parameters first, then the signature's own
    /// `/SubFilter`. `None` when absent or undecodable.
    pub timestamp_subfilter: Option<String>,
    /// Timestamp time (`/M`, a PDF date string) when decodable.
    pub time: Option<String>,
}

/// Inspect `input` and report the document-timestamp status of every
/// signature field. See the module docs for what is and is not checked.
pub fn list_signature_timestamps(
    input: &[u8],
    limits: &EngineLimits,
) -> PdfResult<Vec<SignedTimestamp>> {
    let doc = load_pdf(input, limits)?;
    let sigs = signature_value_dicts(&doc);

    let mut out = Vec::with_capacity(sigs.len());
    for (index, entry) in sigs.iter().enumerate() {
        let dict = &entry.dict;
        // A /Reference DocTimeStamp transform wins; standalone catalog
        // timestamp signatures fall back to their own identity.
        let reference_ts = doc_time_stamp_reference(dict, &doc);
        let (has_timestamp, sub_filter, time) = match reference_ts {
            Some(ts) => (
                true,
                ts.sub_filter
                    .or_else(|| name_entry(dict, b"SubFilter", &doc)),
                ts.time,
            ),
            None if entry.standalone_timestamp => (
                true,
                name_entry(dict, b"SubFilter", &doc),
                text_entry(dict, b"M"),
            ),
            None => (false, None, None),
        };
        out.push(SignedTimestamp {
            signature_index: index,
            has_timestamp,
            timestamp_subfilter: sub_filter,
            time,
        });
    }
    Ok(out)
}

/// Whether `input` contains at least one document timestamp — either a
/// `/DocTimeStamp` signature-reference transform inside a signature or
/// a standalone `/Perms /DocTimeStamp` signature. Cheaper than
/// [`list_signature_timestamps`] only in that it stops at the first hit
/// conceptually; both walk the same structures, so this is simply a
/// convenience wrapper.
pub fn has_timestamps(input: &[u8], limits: &EngineLimits) -> PdfResult<bool> {
    Ok(list_signature_timestamps(input, limits)?
        .iter()
        .any(|t| t.has_timestamp))
}

// ---------------------------------------------------------------------
// Signature-dictionary collection
// ---------------------------------------------------------------------

/// A signature value dictionary plus whether it is the standalone
/// catalog-level `/Perms /DocTimeStamp` entry.
struct SigEntry {
    dict: Dictionary,
    standalone_timestamp: bool,
}

/// Collect every signature value dictionary in the same traversal order
/// as `verify_signature::find_signatures` — AcroForm `/Fields` tree
/// first, then `/Perms` `/DocMDP` and `/UR` references — plus the
/// `/Perms /DocTimeStamp` signature, which `find_signatures` does not
/// report. `find_signatures` discards the dictionaries (it keeps only
/// the extracted facts), so this walk is necessarily a parallel
/// implementation; it reuses [`is_signature_field`] to stay consistent.
fn signature_value_dicts(doc: &Document) -> Vec<SigEntry> {
    let mut out = Vec::new();

    let Ok(catalog) = doc.catalog() else {
        return out;
    };

    // 1. AcroForm signature fields.
    if let Ok(acroform) = catalog.get_deref(b"AcroForm", doc)
        && let Ok(fields_obj) = acroform.as_dict().and_then(|d| d.get(b"Fields"))
        && let Ok((_, fields)) = doc.dereference(fields_obj)
        && let Object::Array(field_refs) = fields
    {
        for field_ref in field_refs {
            collect_sig_dicts(doc, field_ref, 0, &mut out);
        }
    }

    // 2. Catalog /Perms signature references.
    if let Ok(perms) = catalog.get_deref(b"Perms", doc)
        && let Ok(perms_dict) = perms.as_dict()
    {
        for key in [b"DocMDP".as_slice(), b"UR".as_slice()] {
            let Ok(reference_obj) = perms_dict.get(key) else {
                continue;
            };
            let Ok((_, reference)) = doc.dereference(reference_obj) else {
                continue;
            };
            let Ok(reference_dict) = reference.as_dict() else {
                continue;
            };
            if let Ok(refs_obj) = reference_dict.get(b"Reference")
                && let Ok((_, refs)) = doc.dereference(refs_obj)
                && let Object::Array(items) = refs
            {
                for item in items {
                    collect_sig_dicts(doc, item, 0, &mut out);
                }
            }
        }

        // 3. Standalone document timestamp: /Perms /DocTimeStamp is
        // itself a signature dictionary.
        if let Ok(ts_obj) = perms_dict.get(b"DocTimeStamp")
            && let Ok((_, resolved)) = doc.dereference(ts_obj)
            && let Ok(dict) = resolved.as_dict()
            && is_signature_field(doc, dict)
        {
            out.push(SigEntry {
                dict: sig_value_dict(doc, dict).clone(),
                standalone_timestamp: true,
            });
        }
    }

    out
}

/// Recursively walk a field dictionary and its `/Kids`, pushing the
/// signature value dictionary (`/V`, else the field dict itself) for
/// every dictionary that looks like a signature.
fn collect_sig_dicts(doc: &Document, field_obj: &Object, depth: usize, out: &mut Vec<SigEntry>) {
    if depth > MAX_FIELD_DEPTH {
        return;
    }
    let Ok((_, field)) = doc.dereference(field_obj) else {
        return;
    };
    let Ok(dict) = field.as_dict() else {
        return;
    };

    if is_signature_field(doc, dict) {
        out.push(SigEntry {
            dict: sig_value_dict(doc, dict).clone(),
            standalone_timestamp: false,
        });
    }

    if let Ok(kids_obj) = dict.get(b"Kids")
        && let Ok((_, kids)) = doc.dereference(kids_obj)
        && let Object::Array(items) = kids
    {
        for kid in items {
            collect_sig_dicts(doc, kid, depth + 1, out);
        }
    }
}

/// The signature value dictionary for a signature field: `/V` when it
/// resolves to a dictionary, otherwise the field dictionary itself
/// (merged field/value dicts).
fn sig_value_dict<'a>(doc: &'a Document, field: &'a Dictionary) -> &'a Dictionary {
    field
        .get_deref(b"V", doc)
        .ok()
        .and_then(|v| v.as_dict().ok())
        .unwrap_or(field)
}

// ---------------------------------------------------------------------
// /Reference DocTimeStamp detection
// ---------------------------------------------------------------------

/// Find the first `/Reference` entry of `sig` that declares a document
/// timestamp transform and extract its declared sub-filter and time.
fn doc_time_stamp_reference(sig: &Dictionary, doc: &Document) -> Option<TimestampRef> {
    let refs_obj = sig.get(b"Reference").ok()?;
    let (_, refs) = doc.dereference(refs_obj).ok()?;
    let Object::Array(items) = refs else {
        return None;
    };

    for item in items {
        let Ok((_, resolved)) = doc.dereference(item) else {
            continue;
        };
        let Ok(reference) = resolved.as_dict() else {
            continue;
        };

        // /TransformMethod /DocTimeStamp, or /TransformParams whose
        // /Type is /DocTimeStamp (defensive: some writers omit the
        // method name but keep typed params).
        let method_is_ts = reference
            .get(b"TransformMethod")
            .ok()
            .and_then(|o| deref(doc, o))
            .is_some_and(|o| matches!(o, Object::Name(n) if n == b"DocTimeStamp"));
        let params = reference
            .get(b"TransformParams")
            .ok()
            .and_then(|o| deref(doc, o))
            .and_then(|o| o.as_dict().ok());
        let params_type_is_ts = params.is_some_and(|p| {
            p.get(b"Type")
                .ok()
                .and_then(|o| deref(doc, o))
                .is_some_and(|o| matches!(o, Object::Name(n) if n == b"DocTimeStamp"))
        });

        if !(method_is_ts || params_type_is_ts) {
            continue;
        }

        return Some(TimestampRef {
            sub_filter: params
                .and_then(|p| p.get(b"SubFilter").ok())
                .and_then(|o| deref(doc, o))
                .and_then(name_object),
            time: params
                .and_then(|p| p.get(b"M").ok())
                .and_then(|o| decode_text_string(o).ok()),
        });
    }
    None
}

// ---------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------

/// Dereference an object one level if it is a reference.
fn deref<'a>(doc: &'a Document, object: &'a Object) -> Option<&'a Object> {
    doc.dereference(object).ok().map(|(_, o)| o)
}

/// Decode a `/Name`-valued object to a `String`.
fn name_object(object: &Object) -> Option<String> {
    match object {
        Object::Name(n) => Some(String::from_utf8_lossy(n).into_owned()),
        _ => None,
    }
}

/// Read a name-valued entry (`/SubFilter`, ...) from `dict`,
/// dereferencing once.
fn name_entry(dict: &Dictionary, key: &[u8], doc: &Document) -> Option<String> {
    dict.get(key)
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(name_object)
}

/// Read a text-string entry (`/M`, ...) from `dict`, dereferencing
/// once. Returns `None` for encrypted/ciphertext or non-string values.
fn text_entry(dict: &Dictionary, key: &[u8]) -> Option<String> {
    dict.get(key).ok().and_then(|o| decode_text_string(o).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;

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
    fn unsigned_document_has_no_timestamps() {
        let input = pdf_with_pages(&["Hello"]);
        let list = list_signature_timestamps(&input, &unlimited()).unwrap();
        assert!(list.is_empty());
        assert!(!has_timestamps(&input, &unlimited()).unwrap());
    }

    /// Build a PDF carrying one signature field with a
    /// `/Reference`-embedded DocTimeStamp transform — no real signing,
    /// just the dictionary structure this op inspects. `/Contents` is
    /// a dummy non-empty string so `is_signature_field` accepts it.
    fn pdf_with_timestamped_signature() -> Vec<u8> {
        use lopdf::{Document, Stream, dictionary};

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let content = Stream::new(dictionary! {}, b"BT /F1 12 Tf ET".to_vec());
        let content_id = doc.add_object(content);
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);

        // Signature value dict with a DocTimeStamp reference transform.
        let params = dictionary! {
            "Type" => "DocTimeStamp",
            "M" => Object::string_literal("D:20240102030405Z"),
            "SubFilter" => "ETSI.RFC3161",
        };
        let reference = dictionary! {
            "TransformMethod" => "DocTimeStamp",
            "TransformParams" => Object::Dictionary(params),
            "Data" => Object::Integer(0),
            "DigestMethod" => "SHA256",
        };
        let sig = dictionary! {
            "Type" => "Sig",
            "Filter" => "Adobe.PPKLite",
            "SubFilter" => "adbe.pkcs7.detached",
            "ByteRange" => vec![0.into(), 100.into(), 200.into(), 300.into()],
            "Contents" => Object::string_literal("dummy-cms"),
            "Reference" => vec![Object::Dictionary(reference)],
        };
        let sig_id = doc.add_object(Object::Dictionary(sig));
        let field = dictionary! {
            "FT" => "Sig",
            "T" => Object::string_literal("Signature1"),
            "V" => Object::Reference(sig_id),
        };
        let field_id = doc.add_object(Object::Dictionary(field));
        let acro = doc.add_object(dictionary! {
            "Fields" => vec![Object::Reference(field_id)],
            "SigFlags" => 1,
        });
        doc.catalog_mut()
            .unwrap()
            .set("AcroForm", Object::Reference(acro));

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    #[test]
    fn doc_timestamp_transform_is_detected() {
        let input = pdf_with_timestamped_signature();
        let list = list_signature_timestamps(&input, &unlimited()).unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].has_timestamp);
        assert_eq!(list[0].time.as_deref(), Some("D:20240102030405Z"));
        assert_eq!(list[0].timestamp_subfilter.as_deref(), Some("ETSI.RFC3161"));
        assert!(has_timestamps(&input, &unlimited()).unwrap());
    }
}
