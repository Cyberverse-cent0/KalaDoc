//! Declare PDF/X conformance intent via XMP metadata.
//!
//! # What this is — and is not
//!
//! Real PDF/X conformance is a *validation* result, not a flag: a file
//! is PDF/X-1a or PDF/X-3 only if it passes a conforming validator
//! (e.g. veraPDF, callas pdfToolbox) against the full ISO 15930
//! requirements — CMYK-only or restricted color use, embedded fonts,
//! an OutputIntent with a valid ICC profile, TrimBox/BleedBox rules,
//! no transparency (for 1a/3), and more.
//!
//! This operation performs the **achievable declaration subset**:
//!
//! * Writes (or merges into) the catalog `/Metadata` XMP stream a
//!   `pdfxid:part` + `pdfxid:conformance` declaration for the
//!   requested [`PdfxLevel`]. When the document already has an XMP
//!   packet the declaration is injected into it, preserving existing
//!   fields; otherwise a minimal valid packet is created.
//!
//! It deliberately does **not**:
//!
//! * Embed an ICC profile or add an `/OutputIntent` — fabricating one
//!   would produce a *dishonest* file. Validators will report the
//!   missing OutputIntent.
//! * Restrict color spaces to CMYK, audit transparency, check boxes,
//!   or touch content streams.
//! * Claim conformance. [`PdfxReport::warnings`] enumerates the gaps
//!   an external validator will flag.
//!
//! Treat the output as "intended for PDF/X validation", not as a
//! validated PDF/X file.

use lopdf::{Document, Object, SaveOptions, Stream, dictionary};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted};

/// PDF/X level this operation can *declare* via XMP metadata.
///
/// Both offered levels declare `pdfxid:conformance="G"` — the standard
/// (non-press-specific) conformance value. The GTS_PDFX variants
/// (GX) are not offered because this op cannot guarantee their extra
/// requirements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PdfxLevel {
    /// PDF/X-1a — CMYK-only exchange format, fully tagged.
    Pdfx1a,
    /// PDF/X-3 — CMYK + spot color exchange format, not tagged.
    Pdfx3,
}

impl PdfxLevel {
    /// The `pdfxid:part` value written into the XMP declaration.
    fn part(self) -> &'static str {
        match self {
            PdfxLevel::Pdfx1a => "1",
            PdfxLevel::Pdfx3 => "3",
        }
    }

    /// The `pdfxid:conformance` value written into the XMP declaration.
    fn conformance(self) -> &'static str {
        match self {
            PdfxLevel::Pdfx1a | PdfxLevel::Pdfx3 => "G",
        }
    }

    /// Short human-readable name for reports.
    fn label(self) -> &'static str {
        match self {
            PdfxLevel::Pdfx1a => "PDF/X-1a",
            PdfxLevel::Pdfx3 => "PDF/X-3",
        }
    }
}

/// Result of [`to_pdfx`]: the rewritten bytes plus what was declared
/// and the known conformance gaps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdfxReport {
    /// The document with the PDF/X XMP declaration written.
    pub bytes: Vec<u8>,
    /// The requested [`PdfxLevel`].
    pub declared_level: PdfxLevel,
    /// The `pdfxid:part` value written ("1" or "3").
    pub declared_part: String,
    /// The `pdfxid:conformance` value written (always "G").
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

/// Write a PDF/X declaration into `input`'s XMP metadata and return
/// the result plus a [`PdfxReport`].
///
/// This is a **declaration**, not validated conformance — see module
/// docs. Encrypted documents are rejected.
pub fn to_pdfx(input: &[u8], level: PdfxLevel, limits: &EngineLimits) -> PdfResult<PdfxReport> {
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
        if existing.contains("pdfxid:part") {
            warnings.push(
                "document already declares a pdfxid:part; it was replaced with the \
                 requested level"
                    .to_string(),
            );
        }
        match merge_pdfxid(existing, level) {
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
    // compatible with older PDF 1.3 baselines and the structure stays
    // easy for validators to inspect.
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
             — PDF/X requires font embedding; validation requires veraPDF or equivalent"
        ));
    } else {
        warnings.push(format!(
            "all {font_total} detected font(s) reference embedded font files (heuristic check only)"
        ));
    }
    warnings.push(
        "no OutputIntent ICC profile embedded — PDF/X requires one (usually a CMYK \
         characterization); add via an external tool before validating"
            .to_string(),
    );
    warnings.push(
        "color usage (CMYK/spot only), transparency, TrimBox/BleedBox and page box \
         rules were not audited; this op only writes the XMP declaration"
            .to_string(),
    );
    if level == PdfxLevel::Pdfx1a {
        warnings.push(
            "PDF/X-1a additionally requires a tagged structure tree — it was not \
             audited or created"
                .to_string(),
        );
    }
    warnings.push(format!(
        "{} is DECLARED via XMP metadata only — it is not validated conformance; \
             run veraPDF or an equivalent validator before shipping",
        level.label()
    ));

    Ok(PdfxReport {
        bytes,
        declared_level: level,
        declared_part: level.part().to_string(),
        declared_conformance: level.conformance().to_string(),
        merged_existing_xmp,
        warnings,
    })
}

/// A minimal, well-formed XMP packet carrying only the `pdfxid`
/// declaration. Uses the standard `pdfxid` namespace.
fn fresh_packet(level: PdfxLevel) -> String {
    format!(
        concat!(
            "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n",
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n",
            " <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n",
            "  <rdf:Description rdf:about=\"\"\n",
            "   xmlns:pdfxid=\"http://www.npes.org/pdfx/ns/id/\"\n",
            "   pdfxid:part=\"{part}\"\n",
            "   pdfxid:conformance=\"{conf}\"/>\n",
            " </rdf:RDF>\n",
            "</x:xmpmeta>\n",
            "<?xpacket end=\"w\"?>\n"
        ),
        part = level.part(),
        conf = level.conformance(),
    )
}

/// The `rdf:Description` fragment injected into an existing packet.
fn pdfxid_fragment(level: PdfxLevel) -> String {
    format!(
        "  <rdf:Description rdf:about=\"\"\n   \
         xmlns:pdfxid=\"http://www.npes.org/pdfx/ns/id/\"\n   \
         pdfxid:part=\"{part}\"\n   \
         pdfxid:conformance=\"{conf}\"/>\n",
        part = level.part(),
        conf = level.conformance(),
    )
}

/// Merge the `pdfxid` declaration into an existing XMP packet by
/// inserting an `rdf:Description` before the closing `</rdf:RDF>`.
///
/// Returns `None` when no `</rdf:RDF>` boundary can be found — the
/// caller then falls back to a fresh packet. If the existing packet
/// already contains a `pdfxid:` attribute we still insert ours (the
/// caller warns); a full XMP/RDF rewrite is out of scope.
fn merge_pdfxid(existing: &str, level: PdfxLevel) -> Option<String> {
    let needle = "</rdf:RDF>";
    let pos = existing.rfind(needle)?;
    let mut merged = String::with_capacity(existing.len() + 256);
    merged.push_str(&existing[..pos]);
    merged.push_str(&pdfxid_fragment(level));
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
    fn fresh_xmp_packet_declares_pdfxid() {
        let pdf = pdf_with_pages(&["hello"]);
        let report = to_pdfx(&pdf, PdfxLevel::Pdfx3, &EngineLimits::default()).unwrap();

        assert_eq!(report.declared_part, "3");
        assert_eq!(report.declared_conformance, "G");
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
        assert!(text.contains("pdfxid:part=\"3\""));
        assert!(text.contains("pdfxid:conformance=\"G\""));
        assert!(text.contains("http://www.npes.org/pdfx/ns/id/"));
    }

    #[test]
    fn part_values_match_levels() {
        let pdf = pdf_with_pages(&["x"]);
        for (level, part) in [(PdfxLevel::Pdfx1a, "1"), (PdfxLevel::Pdfx3, "3")] {
            let r = to_pdfx(&pdf, level, &EngineLimits::default()).unwrap();
            assert_eq!(r.declared_part, part);
            assert_eq!(r.declared_conformance, "G");
        }
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
        let report = to_pdfx(&pdf, PdfxLevel::Pdfx1a, &EngineLimits::default()).unwrap();
        assert!(report.merged_existing_xmp);

        let doc = Document::load_mem(&report.bytes).unwrap();
        let catalog = doc.catalog().unwrap();
        let meta_id = catalog.get(b"Metadata").unwrap().as_reference().unwrap();
        let stream = doc.objects.get(&meta_id).unwrap().as_stream().unwrap();
        let content = stream.decompressed_content_with_limit(1 << 20).unwrap();
        let text = String::from_utf8(content).unwrap();
        assert!(text.contains("pdfxid:part=\"1\""));
        assert!(text.contains("keep me"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(to_pdfx(b"junk", PdfxLevel::Pdfx1a, &EngineLimits::default()).is_err());
    }
}
