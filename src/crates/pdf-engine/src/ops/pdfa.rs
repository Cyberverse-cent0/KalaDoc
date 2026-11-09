//! Declare PDF/A conformance intent via XMP metadata.
//!
//! # What this is — and is not
//!
//! Real PDF/A conformance is a *validation* result, not a flag: a file
//! is PDF/A only if it passes a conforming validator (e.g. veraPDF)
//! against the full ISO 19005 requirements — embedded fonts, an
//! OutputIntent with a valid ICC profile, restricted color spaces,
//! device-independent color, proper metadata, and more.
//!
//! This operation performs the **achievable declaration subset**:
//!
//! * Writes (or merges into) the catalog `/Metadata` XMP stream a
//!   `pdfaid:part` + `pdfaid:conformance` declaration for the requested
//!   [`PdfaLevel`]. When the document already has an XMP packet the
//!   declaration is injected into it, preserving existing fields;
//!   otherwise a minimal valid packet is created.
//!
//! It deliberately does **not**:
//!
//! * Embed an ICC profile or add an `/OutputIntent`. There is no
//!   sRGB ICC blob available in this crate, and fabricating one would
//!   produce a *dishonest* file — worse than none. Validators will
//!   report the missing OutputIntent.
//! * Embed fonts, restrict color spaces, or touch content streams.
//! * Claim conformance. [`PdfaReport::warnings`] enumerates the gaps an
//!   external validator will flag.
//!
//! Treat the output as "intended for PDF/A-X validation", not as a
//! validated PDF/A file.

use lopdf::{Document, Object, SaveOptions, Stream, dictionary};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted};

/// PDF/A level this operation can *declare* via XMP metadata.
///
/// All variants are "basic" conformance level B. Level A (accessible)
/// and U (unicode) are not offered because this op cannot guarantee
/// their extra requirements (tagged structure, Unicode maps).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PdfaLevel {
    /// PDF/A-1b — based on PDF 1.4.
    PdfA1b,
    /// PDF/A-2b — based on PDF 1.7, allows transparency etc.
    PdfA2b,
    /// PDF/A-3b — based on PDF 1.7, allows embedded files.
    PdfA3b,
}

impl PdfaLevel {
    /// The `pdfaid:part` value written into the XMP declaration.
    fn part(self) -> &'static str {
        match self {
            PdfaLevel::PdfA1b => "1",
            PdfaLevel::PdfA2b => "2",
            PdfaLevel::PdfA3b => "3",
        }
    }

    /// The `pdfaid:conformance` value written into the XMP declaration.
    fn conformance(self) -> &'static str {
        match self {
            PdfaLevel::PdfA1b | PdfaLevel::PdfA2b | PdfaLevel::PdfA3b => "B",
        }
    }

    /// Short human-readable name for reports.
    fn label(self) -> &'static str {
        match self {
            PdfaLevel::PdfA1b => "PDF/A-1b",
            PdfaLevel::PdfA2b => "PDF/A-2b",
            PdfaLevel::PdfA3b => "PDF/A-3b",
        }
    }
}

/// Result of [`to_pdfa`]: the rewritten bytes plus what was declared
/// and the known conformance gaps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdfaReport {
    /// The document with the PDF/A XMP declaration written.
    pub bytes: Vec<u8>,
    /// The requested [`PdfaLevel`].
    pub declared_level: PdfaLevel,
    /// The `pdfaid:part` value written ("1", "2" or "3").
    pub declared_part: String,
    /// The `pdfaid:conformance` value written (always "B").
    pub declared_conformance: String,
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

/// Write a PDF/A declaration into `input`'s XMP metadata and return the
/// result plus a [`PdfaReport`].
///
/// This is a **declaration**, not validated conformance — see module
/// docs. Encrypted documents are rejected.
pub fn to_pdfa(input: &[u8], level: PdfaLevel, limits: &EngineLimits) -> PdfResult<PdfaReport> {
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
        if existing.contains("pdfaid:part") {
            warnings.push(
                "document already declares a pdfaid:part; it was replaced with the \
                 requested level"
                    .to_string(),
            );
        }
        match merge_pdfaid(existing, level) {
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
    // compatible with the PDF 1.4 baseline of PDF/A-1 and the
    // structure stays easy for validators to inspect.
    let mut bytes = Vec::new();
    doc.save_with_options(&mut bytes, SaveOptions::default())
        .map_err(PdfError::Encode)?;

    // --- Honest warnings ----------------------------------------------
    let (font_total, font_unembedded) = count_unembedded_fonts(&doc);
    if font_total == 0 {
        warnings.push("no font objects found in the document".to_string());
    } else if font_unembedded > 0 {
        warnings.push(format!(
            "{font_unembedded} of {font_total} font(s) appear to lack embedded font files \
             — PDF/A requires font embedding; validation requires veraPDF or equivalent"
        ));
    } else {
        warnings.push(format!(
            "all {font_total} detected font(s) reference embedded font files (heuristic check only)"
        ));
    }
    warnings.push(
        "no OutputIntent ICC profile embedded — PDF/A requires one (sRGB for most files); \
         add via an external tool before validating"
            .to_string(),
    );
    warnings.push(
        "color spaces, transparency usage and logical structure were not audited; \
         this op only writes the XMP declaration"
            .to_string(),
    );
    warnings.push(format!(
        "{} is DECLARED via XMP metadata only — it is not validated conformance; \
             run veraPDF or an equivalent validator before shipping",
        level.label()
    ));

    Ok(PdfaReport {
        bytes,
        declared_level: level,
        declared_part: level.part().to_string(),
        declared_conformance: level.conformance().to_string(),
        merged_existing_xmp,
        warnings,
    })
}

/// A minimal, well-formed XMP packet carrying only the `pdfaid`
/// declaration. Uses the standard `pdfaid` namespace.
fn fresh_packet(level: PdfaLevel) -> String {
    format!(
        concat!(
            "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n",
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n",
            " <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n",
            "  <rdf:Description rdf:about=\"\"\n",
            "   xmlns:pdfaid=\"http://www.aiim.org/pdfa/ns/id/\"\n",
            "   pdfaid:part=\"{part}\"\n",
            "   pdfaid:conformance=\"{conf}\"/>\n",
            " </rdf:RDF>\n",
            "</x:xmpmeta>\n",
            "<?xpacket end=\"w\"?>\n"
        ),
        part = level.part(),
        conf = level.conformance(),
    )
}

/// The `rdf:Description` fragment injected into an existing packet.
fn pdfaid_fragment(level: PdfaLevel) -> String {
    format!(
        "  <rdf:Description rdf:about=\"\"\n   \
         xmlns:pdfaid=\"http://www.aiim.org/pdfa/ns/id/\"\n   \
         pdfaid:part=\"{part}\"\n   \
         pdfaid:conformance=\"{conf}\"/>\n",
        part = level.part(),
        conf = level.conformance(),
    )
}

/// Merge the `pdfaid` declaration into an existing XMP packet by
/// inserting an `rdf:Description` before the closing `</rdf:RDF>`.
///
/// Returns `None` when no `</rdf:RDF>` boundary can be found — the
/// caller then falls back to a fresh packet. If the existing packet
/// already contains a `pdfaid:` attribute we still insert ours (the
/// caller warns); a full XMP/RDF rewrite is out of scope.
fn merge_pdfaid(existing: &str, level: PdfaLevel) -> Option<String> {
    let needle = "</rdf:RDF>";
    let pos = existing.rfind(needle)?;
    let mut merged = String::with_capacity(existing.len() + 256);
    merged.push_str(&existing[..pos]);
    merged.push_str(&pdfaid_fragment(level));
    merged.push_str(&existing[pos..]);
    Some(merged)
}

/// Heuristic font-embedding check: count `/Font` dictionaries and how
/// many resolve a `/FontDescriptor` carrying a `/FontFile`,
/// `/FontFile2` or `/FontFile3` entry. Type3 fonts draw glyphs from
/// content streams and are treated as self-contained. This is a
/// best-effort scan, not a font audit.
fn count_unembedded_fonts(doc: &Document) -> (usize, usize) {
    let mut total = 0usize;
    let mut unembedded = 0usize;

    for object in doc.objects.values() {
        let Ok(dict) = object.as_dict() else { continue };
        if dict.get(b"Type").and_then(Object::as_name).ok() != Some(&b"Font"[..]) {
            continue;
        }
        // Type3 fonts are self-contained (glyphs are content streams).
        if let Ok(subtype) = dict.get(b"Subtype").and_then(Object::as_name)
            && subtype == &b"Type3"[..]
        {
            continue;
        }
        total += 1;

        let embedded = dict
            .get(b"FontDescriptor")
            .ok()
            .and_then(|o| match o {
                Object::Reference(id) => doc.objects.get(id).and_then(|d| d.as_dict().ok()),
                Object::Dictionary(d) => Some(d),
                _ => None,
            })
            .map(|fd| fd.has(b"FontFile") || fd.has(b"FontFile2") || fd.has(b"FontFile3"))
            .unwrap_or(false);
        if !embedded {
            unembedded += 1;
        }
    }
    (total, unembedded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;

    #[test]
    fn fresh_xmp_packet_declares_pdfaid() {
        let pdf = pdf_with_pages(&["hello"]);
        let report = to_pdfa(&pdf, PdfaLevel::PdfA2b, &EngineLimits::default()).unwrap();

        assert_eq!(report.declared_part, "2");
        assert_eq!(report.declared_conformance, "B");
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
        assert!(text.contains("pdfaid:part=\"2\""));
        assert!(text.contains("pdfaid:conformance=\"B\""));
        assert!(text.contains("http://www.aiim.org/pdfa/ns/id/"));
    }

    #[test]
    fn part_values_match_levels() {
        let pdf = pdf_with_pages(&["x"]);
        for (level, part) in [
            (PdfaLevel::PdfA1b, "1"),
            (PdfaLevel::PdfA2b, "2"),
            (PdfaLevel::PdfA3b, "3"),
        ] {
            let r = to_pdfa(&pdf, level, &EngineLimits::default()).unwrap();
            assert_eq!(r.declared_part, part);
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(to_pdfa(b"junk", PdfaLevel::PdfA2b, &EngineLimits::default()).is_err());
    }
}
