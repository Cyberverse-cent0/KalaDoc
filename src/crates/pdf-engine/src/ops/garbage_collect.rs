//! Garbage-collect a PDF: drop unreachable objects, renumber, and
//! save with compressed object streams.
//!
//! Three real `lopdf` passes are applied, in this order:
//!
//! 1. [`Document::compress`] — FlateDecode level 9 on every stream that
//!    has no `/Filter` yet and where compression actually pays. Streams
//!    that are already filtered (images, encoded content) are left
//!    alone — same honest scope as the `compress` op.
//! 2. [`Document::prune_objects`] — removes every indirect object not
//!    reachable by traversal from the trailer (`/Root`, `/Info`,
//!    `/Encrypt`, ...). Objects orphaned by earlier edits or damaged
//!    files are dropped here.
//! 3. [`Document::renumber_objects`] — renumbers the survivors
//!    contiguously from object 1 so the file has no gaps.
//!
//! The result is then saved with [`lopdf::SaveOptions`] enabling
//! object streams and cross-reference streams (PDF 1.5+), so the many
//! small non-stream objects are packed into compressed object streams.
//!
//! All counts in [`GcReport`] are real: `objects_before`/`objects_after`
//! are `doc.objects.len()` measured in memory before and after the GC
//! passes, and `objects_pruned` is the exact length of the ID list
//! `prune_objects` returned. Note that `objects_after` counts
//! *pre-save* in-memory objects; the saved file additionally packs
//! non-stream objects into object streams, so a reload shows fewer
//! top-level objects — that packing is a serialization detail, not
//! additional collection.

use lopdf::SaveOptions;

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted};

/// Result of [`garbage_collect`].
#[derive(Debug, Clone)]
pub struct GcReport {
    /// Rewritten document bytes.
    pub bytes: Vec<u8>,
    /// `doc.objects.len()` before any GC pass.
    pub objects_before: usize,
    /// `doc.objects.len()` after prune + renumber (in-memory, pre-save).
    pub objects_after: usize,
    /// Exact number of objects `prune_objects` removed.
    pub objects_pruned: usize,
    /// Input size in bytes.
    pub bytes_before: usize,
    /// Output size in bytes.
    pub bytes_after: usize,
    /// Human-readable list of the techniques actually applied.
    pub techniques: Vec<String>,
}

/// Garbage-collect `input` and return the rewritten bytes plus a
/// [`GcReport`] with real before/after counts.
///
/// Encrypted documents are rejected — object-stream packing is skipped
/// for them by `lopdf` anyway, and rewriting an encrypted file's object
/// graph is out of scope for this op.
pub fn garbage_collect(input: &[u8], limits: &EngineLimits) -> PdfResult<GcReport> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let objects_before = doc.objects.len();
    let bytes_before = input.len();
    let mut techniques = Vec::new();

    // 1. Recompress unfiltered streams where it pays (lopdf decides).
    doc.compress();
    techniques.push(
        "stream recompression: FlateDecode level 9 applied to unfiltered streams \
         where smaller (already-filtered streams untouched)"
            .to_string(),
    );

    // 2. Drop objects unreachable from the trailer.
    let pruned = doc.prune_objects();
    let objects_pruned = pruned.len();
    techniques.push(format!(
        "pruned {objects_pruned} object(s) unreachable from the trailer"
    ));

    // 3. Renumber survivors contiguously from 1.
    doc.renumber_objects();
    techniques.push("renumbered remaining objects contiguously from 1".to_string());

    let objects_after = doc.objects.len();

    // Sanity: prune + renumber must conserve the count arithmetic.
    debug_assert_eq!(objects_before - objects_pruned, objects_after);

    // 4. Save with object streams + xref streams.
    let options = SaveOptions::builder()
        .use_object_streams(true)
        .use_xref_streams(true)
        .compression_level(9)
        .build();
    let mut bytes = Vec::new();
    doc.save_with_options(&mut bytes, options)
        .map_err(PdfError::Encode)?;
    techniques.push(
        "saved with compressed object streams and a cross-reference stream (PDF 1.5+); \
         non-stream objects are packed into ObjStm on save"
            .to_string(),
    );

    let bytes_after = bytes.len();
    Ok(GcReport {
        bytes,
        objects_before,
        objects_after,
        objects_pruned,
        bytes_before,
        bytes_after,
        techniques,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;

    #[test]
    fn counts_are_real_and_consistent() {
        let pdf = pdf_with_pages(&["page one", "page two"]);

        // Count the source document's objects for comparison.
        let src = lopdf::Document::load_mem(&pdf).unwrap();
        let src_objects = src.objects.len();

        let report = garbage_collect(&pdf, &EngineLimits::default()).unwrap();

        assert_eq!(report.objects_before, src_objects);
        assert_eq!(
            report.objects_before - report.objects_pruned,
            report.objects_after
        );
        assert_eq!(report.bytes_after, report.bytes.len());
        assert_eq!(report.bytes_before, pdf.len());
        assert!(report.objects_pruned == 0 || report.objects_pruned > 0);
        assert!(!report.techniques.is_empty());

        // Output must reload as a valid PDF with the same page count.
        let out = lopdf::Document::load_mem(&report.bytes).unwrap();
        assert_eq!(out.get_pages().len(), 2);
    }

    #[test]
    fn rejects_garbage() {
        assert!(garbage_collect(b"junk", &EngineLimits::default()).is_err());
    }
}
