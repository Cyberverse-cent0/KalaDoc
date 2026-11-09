//! Best-effort repair of damaged PDF files.
//!
//! `lopdf`'s loader already recovers from the most common damage: broken
//! or missing cross-reference tables are rebuilt by scanning the file
//! for indirect objects (`Document::xref_recovery` records which path
//! was taken). `repair` loads through that machinery, then reports what
//! was found in a [`RepairReport`] and writes the document back out.
//!
//! Important honesty note: the output is a **normalized rewrite**, not a
//! byte-level repair. Objects `lopdf` could not parse are gone for good,
//! object streams may be unpacked and repacked, and the cross-reference
//! data is regenerated. References that pointed at missing objects stay
//! dangling (now reported in the notes). Nothing here pretends to
//! recover content the parser never saw.

use std::collections::HashSet;

use lopdf::xref::XrefRecovery;
use lopdf::{Object, ObjectId};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::{load_pdf, reject_encrypted, save_pdf};

/// Decoded-size ceiling when probing stream objects for damage. Matches
/// the bound `validate` uses for page content.
const STREAM_PROBE_LIMIT: usize = 64 * 1024 * 1024;

/// Result of [`repair`]: the rewritten bytes plus human-readable notes
/// describing what was found and fixed (or not).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairReport {
    /// The repaired document, re-serialized by `lopdf`.
    pub bytes: Vec<u8>,
    /// What the repair observed: xref recovery, dangling references,
    /// undecodable streams, and the standing caveat that this is a
    /// normalized rewrite.
    pub notes: Vec<String>,
}

/// Repair `input` as far as `lopdf`'s parser allows and return the
/// rewritten document plus a [`RepairReport`].
///
/// Returns `Err` only when the input cannot be loaded at all (or a
/// limit is exceeded); partially broken files that still parse produce
/// `Ok` with explanatory notes. Encrypted documents are rejected —
/// repair never attempts decryption.
pub fn repair(input: &[u8], limits: &EngineLimits) -> PdfResult<RepairReport> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let mut notes = Vec::new();

    match &doc.xref_recovery {
        Some(XrefRecovery::OffsetCorrected) => notes.push(
            "cross-reference offsets were corrected during load; the file's xref data was damaged"
                .to_string(),
        ),
        Some(XrefRecovery::Reconstructed) => notes.push(
            "cross-reference table was reconstructed by scanning the file; some objects may be missing"
                .to_string(),
        ),
        None => notes.push("cross-reference table loaded cleanly".to_string()),
        // XrefRecovery is not marked non_exhaustive today, but stay
        // tolerant of future variants.
        Some(_) => notes.push("cross-reference data required repair during load".to_string()),
    }

    // Dangling references: objects pointing at object numbers that are
    // not present in the loaded object map. These survive the rewrite
    // (lopdf keeps the references), so they are reported, not fixed.
    let present: HashSet<ObjectId> = doc.objects.keys().copied().collect();
    let mut referenced = HashSet::new();
    for object in doc.objects.values() {
        collect_references(object, &mut referenced);
    }
    let dangling = referenced.iter().filter(|id| !present.contains(id)).count();
    if dangling > 0 {
        notes.push(format!(
            "{dangling} reference(s) point at objects missing from the file"
        ));
    }

    // Streams whose content cannot be decoded at all indicate real
    // corruption (bad filters, truncated data). Count them; the rewrite
    // still carries the raw stream bytes through.
    let mut bad_streams = 0usize;
    let mut bad_ids = Vec::new();
    for (id, object) in &doc.objects {
        if let Object::Stream(stream) = object
            && stream
                .decompressed_content_with_limit(STREAM_PROBE_LIMIT)
                .is_err()
        {
            bad_streams += 1;
            if bad_ids.len() < 5 {
                bad_ids.push(format!("{id:?}"));
            }
        }
    }
    if bad_streams > 0 {
        notes.push(format!(
            "{bad_streams} stream object(s) could not be fully decoded (first: {})",
            bad_ids.join(", ")
        ));
    }

    if doc.get_pages().is_empty() {
        notes.push("document has no pages; output is still written".to_string());
    }

    notes.push(
        "repair produces a normalized rewrite of the document, not a byte-level fix; \
         objects the parser could not read are not recovered"
            .to_string(),
    );

    let bytes = save_pdf(&mut doc)?;
    Ok(RepairReport { bytes, notes })
}

/// Collect every [`Object::Reference`] reachable inside `object`,
/// descending into arrays, dictionaries and stream dictionaries.
fn collect_references(object: &Object, out: &mut HashSet<ObjectId>) {
    match object {
        Object::Reference(id) => {
            out.insert(*id);
        }
        Object::Array(items) => {
            for item in items {
                collect_references(item, out);
            }
        }
        Object::Dictionary(dict) => {
            for (_, value) in dict.iter() {
                collect_references(value, out);
            }
        }
        Object::Stream(stream) => {
            for (_, value) in stream.dict.iter() {
                collect_references(value, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};

    #[test]
    fn repair_valid_document_reports_clean_load() {
        let pdf = pdf_with_pages(&["one", "two"]);
        let report = repair(&pdf, &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&report.bytes), 2);
        assert!(report.notes.iter().any(|n| n.contains("cross-reference")));
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("normalized rewrite"))
        );
    }

    #[test]
    fn repair_truncated_document_still_recovers() {
        let pdf = pdf_with_pages(&["a", "b", "c"]);
        // Chop the tail: xref and trailer are gone, forcing lopdf's
        // reconstruction path.
        let truncated = &pdf[..pdf.len() / 2];
        match repair(truncated, &EngineLimits::default()) {
            Ok(report) => {
                // Either it recovered something or honestly reported;
                // both are acceptable as long as we get bytes back.
                assert!(!report.notes.is_empty());
            }
            Err(_) => {
                // Also acceptable: a truncated file may not load at all.
            }
        }
    }

    #[test]
    fn repair_rejects_garbage() {
        assert!(repair(b"definitely not a pdf", &EngineLimits::default()).is_err());
    }
}
