//! Font audit: honest inventory of embedded vs. non-embedded fonts.
//!
//! Reuses the [`super::inspect_fonts::inspect_fonts`] walk — which
//! covers page resources, inherited `/Resources`, and one level of
//! Form XObjects — then classifies every referenced font into one of
//! three buckets:
//!
//! - **embedded** — a `/FontDescriptor` carries `/FontFile`,
//!  `/FontFile2` or `/FontFile3` (the flag computed by the inspect
//!  op, including `/DescendantFonts` descendants). No risk.
//! - **base-14** — not embedded, but the `/BaseFont` name (with any
//!  `AAAAAA+` subset prefix stripped) is one of the PDF base-14
//!  fonts. Viewers are expected to supply standard metrics, so
//!  rendering is predictable.
//! - **other non-embedded** — recorded as a [`FontRisk`]. Rendering
//!  of these fonts varies across viewers and platforms; nothing is
//!  substituted or modified, the report is descriptive only.
//!
//! Like [`super::inspect_fonts::inspect_fonts`], encrypted documents
//! are tolerated: font names are PDF name objects and stay readable,
//! so the audit is still produced for structure lopdf can parse.

use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::inspect_fonts::inspect_fonts;

/// The 14 standard PDF base fonts that viewers are expected to
/// supply without an embedded program: the four Courier, Helvetica
/// and Times-Roman variants plus `Symbol` and `ZapfDingbats`.
pub const BASE14: [&str; 14] = [
    "Courier",
    "Courier-Bold",
    "Courier-Oblique",
    "Courier-BoldOblique",
    "Helvetica",
    "Helvetica-Bold",
    "Helvetica-Oblique",
    "Helvetica-BoldOblique",
    "Times-Roman",
    "Times-Bold",
    "Times-Italic",
    "Times-BoldItalic",
    "Symbol",
    "ZapfDingbats",
];

/// A non-embedded font that is not one of the base-14 set — its
/// rendering may vary across viewers and platforms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FontRisk {
    /// `/BaseFont` name (subset prefix kept verbatim), or the
    /// resource name when `/BaseFont` is absent.
    pub name: String,
    /// `/Subtype` name (`"Type1"`, `"TrueType"`, `"Type0"`, ...);
    /// empty string when the font dictionary does not declare one.
    pub subtype: String,
    /// Sorted, deduplicated 1-based page numbers referencing this
    /// font.
    pub pages: Vec<u32>,
    /// Why the font is flagged.
    pub reason: String,
}

/// Result of [`audit_fonts`]: counts per font class plus the list of
/// fonts whose rendering is not guaranteed by the PDF spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FontAudit {
    /// Total distinct fonts referenced by the document's pages.
    pub total_fonts: usize,
    /// Fonts with an embedded font program (`/FontFile*`).
    pub embedded_fonts: usize,
    /// Non-embedded fonts whose base name is one of [`BASE14`].
    pub base14_fonts: usize,
    /// Non-embedded, non-base-14 fonts — rendering varies.
    pub non_embedded: Vec<FontRisk>,
    /// Human-readable warnings (one per flagged font, plus any
    /// notable conditions).
    pub warnings: Vec<String>,
}

/// Audit every font referenced by `input`'s pages.
///
/// Input-side limits (`max_input_bytes`, `max_pages`) are enforced by
/// the underlying load inside [`inspect_fonts`]. Encrypted documents
/// are tolerated like [`super::inspect_fonts::inspect_fonts`]:
/// structure is still reported. Nothing is decrypted, substituted
/// or modified.
pub fn audit_fonts(input: &[u8], limits: &EngineLimits) -> PdfResult<FontAudit> {
    let fonts = inspect_fonts(input, limits)?;

    let mut embedded_fonts = 0usize;
    let mut base14_fonts = 0usize;
    let mut non_embedded = Vec::new();
    let mut warnings = Vec::new();

    for font in fonts {
        if font.embedded {
            embedded_fonts += 1;
            continue;
        }
        if BASE14.contains(&base_font_name(&font.name).as_str()) {
            base14_fonts += 1;
            continue;
        }
        let reason = format!(
            "font '{}' ({}) is not embedded and is not one of the 14 base fonts; \
             rendering varies across viewers and platforms",
            font.name,
            if font.subtype.is_empty() {
                "no /Subtype".to_string()
            } else {
                format!("/Subtype /{}", font.subtype)
            }
        );
        warnings.push(format!(
            "non-embedded non-base-14 font '{}' on pages {:?}",
            font.name, font.pages
        ));
        non_embedded.push(FontRisk {
            name: font.name,
            subtype: font.subtype,
            pages: font.pages,
            reason,
        });
    }

    Ok(FontAudit {
        total_fonts: embedded_fonts + base14_fonts + non_embedded.len(),
        embedded_fonts,
        base14_fonts,
        non_embedded,
        warnings,
    })
}

/// Strip a `AAAAAA+` subset prefix (six uppercase letters followed by
/// `+`) from a `/BaseFont` name so it can be compared against
/// [`BASE14`]. Names without a valid prefix are returned unchanged.
fn base_font_name(name: &str) -> String {
    let bytes = name.as_bytes();
    if bytes.len() > 7 && bytes[6] == b'+' && bytes[..6].iter().all(|b| b.is_ascii_uppercase()) {
        name[7..].to_string()
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::{Document, Stream, dictionary};

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    /// The fixture's `/Courier` Type1 font is a non-embedded base-14
    /// font — counted in `base14_fonts`, no risks.
    #[test]
    fn classifies_base14_font() {
        let pdf = pdf_with_pages(&["a", "b"]);
        let audit = audit_fonts(&pdf, &limits()).unwrap();
        assert_eq!(audit.total_fonts, 1);
        assert_eq!(audit.embedded_fonts, 0);
        assert_eq!(audit.base14_fonts, 1);
        assert!(audit.non_embedded.is_empty());
        assert!(audit.warnings.is_empty());
    }

    /// A font whose `/FontDescriptor` carries `/FontFile2` counts as
    /// embedded even though its base name is not base-14.
    #[test]
    fn counts_embedded_font_via_descriptor() {
        let mut doc = Document::load_mem(&pdf_with_pages(&["x"])).unwrap();
        let font_id = doc
            .get_pages()
            .values()
            .next()
            .and_then(|page_id| {
                let page = doc.get_dictionary(*page_id).ok()?;
                let res = page.get_deref(b"Resources", &doc).ok()?;
                let res = res.as_dict().ok()?;
                let font = res.get_deref(b"Font", &doc).ok()?;
                font.as_dict().ok()?.get(b"F1").ok()?.as_reference().ok()
            })
            .unwrap();

        let descriptor_id = doc.add_object(dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => "CustomFont",
            "Flags" => 4i64,
            "FontBBox" => vec![0.into(), 0.into(), 0.into(), 0.into()],
            "ItalicAngle" => 0,
            "Ascent" => 0,
            "Descent" => 0,
            "CapHeight" => 0,
            "StemV" => 0,
            "FontFile2" => Stream::new(dictionary! {}, vec![1u8, 2, 3]),
        });
        {
            let font = doc.get_dictionary_mut(font_id).unwrap();
            font.set("BaseFont", "CustomSans");
            font.set("FontDescriptor", lopdf::Object::Reference(descriptor_id));
        }

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        let audit = audit_fonts(&buf, &limits()).unwrap();
        assert_eq!(audit.total_fonts, 1);
        assert_eq!(audit.embedded_fonts, 1);
        assert_eq!(audit.base14_fonts, 0);
        assert!(audit.non_embedded.is_empty());
    }

    /// A non-embedded font outside the base-14 set is reported as a
    /// [`FontRisk`] with its subtype, pages and reason.
    #[test]
    fn flags_non_embedded_non_base14_font() {
        let mut doc = Document::load_mem(&pdf_with_pages(&["x"])).unwrap();
        let custom_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "TrueType",
            "BaseFont" => "MyCustomFont",
        });
        let (_, page_id) = doc
            .get_pages()
            .iter()
            .next()
            .map(|(n, i)| (*n, *i))
            .unwrap();
        let res_id = {
            let page = doc.get_dictionary(page_id).unwrap();
            page.get(b"Resources").unwrap().as_reference().unwrap()
        };
        {
            let res = doc.get_dictionary_mut(res_id).unwrap();
            let mut font = res.get(b"Font").unwrap().as_dict().unwrap().clone();
            font.set("F2", custom_id);
            res.set("Font", font);
        }

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        let audit = audit_fonts(&buf, &limits()).unwrap();
        assert_eq!(audit.total_fonts, 2);
        assert_eq!(audit.base14_fonts, 1);
        assert_eq!(audit.non_embedded.len(), 1);
        let risk = &audit.non_embedded[0];
        assert_eq!(risk.name, "MyCustomFont");
        assert_eq!(risk.subtype, "TrueType");
        assert_eq!(risk.pages, vec![1]);
        assert!(risk.reason.contains("not embedded"), "{:?}", risk.reason);
        assert_eq!(audit.warnings.len(), 1);
    }

    /// A `AAAAAA+` subset prefix is stripped before the base-14
    /// check: `ABCDEF+Helvetica` counts as base-14.
    #[test]
    fn strips_subset_prefix_for_base14_match() {
        let mut doc = Document::load_mem(&pdf_with_pages(&["x"])).unwrap();
        let font_id = doc
            .get_pages()
            .values()
            .next()
            .and_then(|page_id| {
                let page = doc.get_dictionary(*page_id).ok()?;
                let res = page.get_deref(b"Resources", &doc).ok()?;
                let res = res.as_dict().ok()?;
                let font = res.get_deref(b"Font", &doc).ok()?;
                font.as_dict().ok()?.get(b"F1").ok()?.as_reference().ok()
            })
            .unwrap();
        doc.get_dictionary_mut(font_id)
            .unwrap()
            .set("BaseFont", "ABCDEF+Helvetica");

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        let audit = audit_fonts(&buf, &limits()).unwrap();
        assert_eq!(audit.total_fonts, 1);
        assert_eq!(audit.base14_fonts, 1);
        assert!(audit.non_embedded.is_empty());
    }

    /// Garbage input surfaces as an error, not a panic.
    #[test]
    fn rejects_garbage_input() {
        assert!(audit_fonts(b"not a pdf", &limits()).is_err());
    }
}
