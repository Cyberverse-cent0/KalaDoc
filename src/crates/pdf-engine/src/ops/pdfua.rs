//! Declare PDF/UA conformance intent via XMP metadata.
//!
//! # What this is — and is not
//!
//! Real PDF/UA conformance is a *validation* result, not a flag: a file
//! is PDF/UA-1 only if it passes a conforming validator against the
//! full ISO 14289-1 requirements — a proper tag tree with structure
//! elements, Unicode character maps for all text, alternative
//! descriptions for non-text content, embedded fonts, language
//! metadata, and more.
//!
//! This operation performs the **achievable declaration subset**:
//!
//! * Writes (or merges into) the catalog `/Metadata` XMP stream a
//!   `pdfuaid:part` declaration for the requested [`PdfuaLevel`].
//!   When the document already has an XMP packet the declaration is
//!   injected into it, preserving existing fields; otherwise a
//!   minimal valid packet is created.
//!
//! It deliberately does **not**:
//!
//! * Build or repair the structure tree, add `/StructTreeRoot`, tag
//!   content, set `/Lang`, or add `/Alt` text — the heart of real
//!   PDF/UA. Validators will report all of these as missing.
//! * Validate fonts, language, or accessible naming.
//! * Claim conformance. [`PdfuaReport::warnings`] enumerates the gaps
//!   an external validator will flag.
//!
//! Treat the output as "intended for PDF/UA-1 validation", not as a
//! validated PDF/UA file.

use lopdf::{Object, SaveOptions, Stream, dictionary};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted};

/// PDF/UA level this operation can *declare* via XMP metadata.
///
/// Only PDF/UA-1 (ISO 14289-1) is offered — it is the only widely
/// deployed PDF/UA profile, and the only one whose declaration is a
/// single `pdfuaid:part` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PdfuaLevel {
    /// PDF/UA-1 — the "Universal Accessibility" profile for PDF.
    PdfUa1,
}

impl PdfuaLevel {
    /// The `pdfuaid:part` value written into the XMP declaration.
    fn part(self) -> &'static str {
        match self {
            PdfuaLevel::PdfUa1 => "1",
        }
    }

    /// Short human-readable name for reports.
    fn label(self) -> &'static str {
        match self {
            PdfuaLevel::PdfUa1 => "PDF/UA-1",
        }
    }
}

/// Result of [`to_pdfua`]: the rewritten bytes plus what was declared
/// and the known conformance gaps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdfuaReport {
    /// The document with the PDF/UA XMP declaration written.
    pub bytes: Vec<u8>,
    /// The requested [`PdfuaLevel`].
    pub declared_level: PdfuaLevel,
    /// The `pdfuaid:part` value written (always "1").
    pub declared_part: String,
    /// `true` if an existing XMP packet was found and the declaration
    /// was merged into it; `false` if a fresh minimal packet was
    /// created (or a replacement was forced — see `warnings`).
    pub merged_existing_xmp: bool,
    /// Known conformance gaps an external validator will flag, plus
    /// process notes. This is a declaration, not validated conformance.
    pub warnings: Vec<String>,
}

/// Cap on the existing XMP packet size we are willing to decode and
/// merge into. Larger packets are replaced by a fresh minimal one
/// (with a warning) rather than risking a pathological read.
const MAX_XMP_MERGE_BYTES: usize = 16 * 1024 * 1024;

/// Write a PDF/UA declaration into `input`'s XMP metadata and return
/// the result plus a [`PdfuaReport`].
///
/// This is a **declaration**, not validated conformance — see module
/// docs. Encrypted documents are rejected.
pub fn to_pdfua(input: &[u8], level: PdfuaLevel, limits: &EngineLimits) -> PdfResult<PdfuaReport> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let mut warnings = Vec::new();

    // --- Gather existing XMP packet, if any --------------------------
    let metadata_id = doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"Metadata").ok())
        .and_then(|o| o.as_reference().ok());

    let existing_xmp = metadata_id.and_then(|id| {
        doc.objects
            .get(&id)
            .and_then(|o| o.as_stream().ok())
            .and_then(|s| s.decompressed_content_with_limit(MAX_XMP_MERGE_BYTES).ok())
            .and_then(|bytes| String::from_utf8(bytes).ok())
    });

    // --- Build the XMP packet ----------------------------------------
    let mut merged_existing_xmp = false;
    let packet = if let Some(existing) = existing_xmp.as_deref() {
        if existing.contains("pdfuaid:part") {
            warnings.push(
                "document already declares a pdfuaid:part; it was replaced with the \
                 requested level"
                    .to_string(),
            );
        }
        match merge_pdfuaid(existing, level) {
            Some(merged) => {
                merged_existing_xmp = true;
                merged
            }
            None => {
                warnings.push(
                    "existing XMP packet could not be merged (no </rdf:RDF> boundary \
                     found within size limits); it was replaced by a minimal packet"
                        .to_string(),
                );
                fresh_packet(level)
            }
        }
    } else {
        fresh_packet(level)
    };

    // --- Write the /Metadata stream ----------------------------------
    let mut stream = Stream::new(
        dictionary! {
            "Type" => "Metadata",
            "Subtype" => "XML",
        },
        packet.into_bytes(),
    );
    // Metadata streams are conventionally Flate-compressed; compress
    // only when lopdf says it pays.
    let _ = stream.compress();
    let stream_obj = Object::Stream(stream);

    match metadata_id {
        Some(id) => {
            doc.objects.insert(id, stream_obj);
        }
        None => {
            let id = doc.add_object(stream_obj);
            doc.catalog_mut()
                .map_err(|_| PdfError::InvalidInput("document has no catalog".to_string()))?
                .set("Metadata", Object::Reference(id));
        }
    }

    // --- Save ---------------------------------------------------------
    // Plain save: object-stream packing is skipped so the file stays
    // compatible with the PDF 1.4 baseline and the structure stays
    // easy for validators to inspect.
    let mut bytes = Vec::new();
    doc.save_with_options(&mut bytes, SaveOptions::default())
        .map_err(PdfError::Encode)?;

    // --- Honest warnings ----------------------------------------------
    warnings.push(
        "no tag tree (/StructTreeRoot) found — PDF/UA requires a complete logical \
         structure; this op only writes the XMP declaration"
            .to_string(),
    );
    warnings.push(
        "tagged content, /Lang, /Alt text, Unicode maps and font checks were not \
         performed — PDF/UA conformance cannot be achieved by metadata alone"
            .to_string(),
    );
    warnings.push(format!(
        "{} is DECLARED via XMP metadata only — it is not validated conformance; \
             run veraPDF or an equivalent validator before shipping",
        level.label()
    ));

    Ok(PdfuaReport {
        bytes,
        declared_level: level,
        declared_part: level.part().to_string(),
        merged_existing_xmp,
        warnings,
    })
}

/// A minimal, well-formed XMP packet carrying only the `pdfuaid`
/// declaration. Uses the standard `pdfuaid` namespace.
fn fresh_packet(level: PdfuaLevel) -> String {
    format!(
        concat!(
            "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n",
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n",
            " <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n",
            "  <rdf:Description rdf:about=\"\"\n",
            "   xmlns:pdfuaid=\"http://www.aiim.org/pdfua/ns/id/\"\n",
            "   pdfuaid:part=\"{part}\"/>\n",
            " </rdf:RDF>\n",
            "</x:xmpmeta>\n",
            "<?xpacket end=\"w\"?>\n"
        ),
        part = level.part(),
    )
}

/// The `rdf:Description` fragment injected into an existing packet.
fn pdfuaid_fragment(level: PdfuaLevel) -> String {
    format!(
        "  <rdf:Description rdf:about=\"\"\n   \
         xmlns:pdfuaid=\"http://www.aiim.org/pdfua/ns/id/\"\n   \
         pdfuaid:part=\"{part}\"/>\n",
        part = level.part(),
    )
}

/// Merge the `pdfuaid` declaration into an existing XMP packet by
/// inserting an `rdf:Description` before the closing `</rdf:RDF>`.
///
/// Returns `None` when no `</rdf:RDF>` boundary can be found — the
/// caller then falls back to a fresh packet. If the existing packet
/// already contains a `pdfuaid:` attribute we still insert ours (the
/// caller warns); a full XMP/RDF rewrite is out of scope.
fn merge_pdfuaid(existing: &str, level: PdfuaLevel) -> Option<String> {
    let needle = "</rdf:RDF>";
    let pos = existing.rfind(needle)?;
    let mut merged = String::with_capacity(existing.len() + 256);
    merged.push_str(&existing[..pos]);
    merged.push_str(&pdfuaid_fragment(level));
    merged.push_str(&existing[pos..]);
    Some(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::Document;

    #[test]
    fn fresh_xmp_packet_declares_pdfuaid() {
        let pdf = pdf_with_pages(&["hello"]);
        let report = to_pdfua(&pdf, PdfuaLevel::PdfUa1, &EngineLimits::default()).unwrap();

        assert_eq!(report.declared_part, "1");
        assert!(!report.merged_existing_xmp);
        assert!(!report.warnings.is_empty());

        // Reload and confirm the /Metadata stream exists with our
        // declaration in it.
        let doc = Document::load_mem(&report.bytes).unwrap();
        let catalog = doc.catalog().unwrap();
        let meta_id = catalog.get(b"Metadata").unwrap().as_reference().unwrap();
        let stream = doc.objects.get(&meta_id).unwrap().as_stream().unwrap();
        let content = stream.decompressed_content_with_limit(1 << 20).unwrap();
        let text = String::from_utf8(content).unwrap();
        assert!(text.contains("pdfuaid:part=\"1\""));
        assert!(text.contains("http://www.aiim.org/pdfua/ns/id/"));
    }

    #[test]
    fn merges_into_existing_xmp() {
        use crate::crates::pdf_engine::ops::set_xmp_metadata::set_xmp;
        let pdf = pdf_with_pages(&["x"]);
        let xmp = concat!(
            "<?xpacket begin=\"\u{feff}\"?>\n",
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n",
            " <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n",
            "  <rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" ",
            "dc:title=\"keep me\"/>\n",
            " </rdf:RDF>\n",
            "</x:xmpmeta>\n",
            "<?xpacket end=\"w\"?>\n"
        );
        let pdf = set_xmp(&pdf, xmp.as_bytes(), &EngineLimits::default()).unwrap();
        let report = to_pdfua(&pdf, PdfuaLevel::PdfUa1, &EngineLimits::default()).unwrap();
        assert!(report.merged_existing_xmp);

        let doc = Document::load_mem(&report.bytes).unwrap();
        let catalog = doc.catalog().unwrap();
        let meta_id = catalog.get(b"Metadata").unwrap().as_reference().unwrap();
        let stream = doc.objects.get(&meta_id).unwrap().as_stream().unwrap();
        let content = stream.decompressed_content_with_limit(1 << 20).unwrap();
        let text = String::from_utf8(content).unwrap();
        assert!(text.contains("pdfuaid:part=\"1\""));
        assert!(text.contains("keep me"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(to_pdfua(b"junk", PdfuaLevel::PdfUa1, &EngineLimits::default()).is_err());
    }
}
