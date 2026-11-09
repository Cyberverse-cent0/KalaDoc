//! Annotation management via the page `/Annots` array.
//!
//! Four operations, all operating on lopdf dictionaries — no appearance
//! streams are synthesized:
//!
//! - [`add_comment`] — append `/Subtype /Text` sticky-note annotations.
//!   Text annotations carry no `/AP` (appearance) stream; conforming
//!   viewers render the standard note icon themselves.
//! - [`add_freetext`] — append `/Subtype /FreeText` annotations, which
//!   *do* render visible text in viewers through their `/DA` default
//!   appearance and `/Contents`.
//! - [`list_annotations`] — walk every page's `/Annots` and report
//!   subtype, rect, contents and author.
//! - [`remove_annotations`] — delete matching annotations from `/Annots`
//!   arrays (the key is removed entirely when the array becomes empty).
//!
//! # Honesty notes / limitations
//!
//! - Text strings are written with lopdf's [`lopdf::text_string`]:
//!   ASCII goes out as PDFDocEncoding literal strings, everything else
//!   as UTF-16BE hexadecimal strings with BOM. Reading uses
//!   [`lopdf::decode_text_string`], which additionally accepts UTF-8.
//! - `/Annots` arrays that are indirect objects are *not* mutated in
//!   place: the page dictionary is pointed at a fresh array and the old
//!   object is left unreferenced (same trade-off as `flatten`).
//! - `/FreeText` rendering depends on the viewer: `/DA` must name a
//!   font that exists in the page's (or an ancestor's) `/Resources`.
//!   The default `/Helv` is a standard-14 alias most viewers resolve;
//!   documents without any font resources may show nothing. Callers
//!   needing guaranteed rendering should embed a font first.
//! - Annotation `/P` (page back-reference), `/M` (modification date),
//!   `/NM` (unique name) and `/F` flags are not written.
//! - `remove_annotations` only removes entries that resolve to
//!   dictionaries. With `all: true` *every* `/Annots` entry on the
//!   selected pages is dropped, including non-dictionary entries.

use lopdf::{Document, Object, ObjectId, decode_text_string, dictionary, text_string};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::pages::parse_page_range;
use super::watermark::{media_box, selected_pages};

/// Sticky-note icon names valid for `/Subtype /Text` annotations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommentIcon {
    /// Speech-bubble comment (the usual sticky note).
    Comment,
    /// Key icon.
    Key,
    /// Music-note icon.
    Note,
    /// Help icon.
    Help,
    /// Paragraph-with-new-lines icon.
    NewParagraph,
    /// Paragraph icon.
    Paragraph,
    /// Insert-text cursor icon.
    Insert,
}

impl CommentIcon {
    fn name(self) -> &'static [u8] {
        match self {
            Self::Comment => b"Comment",
            Self::Key => b"Key",
            Self::Note => b"Note",
            Self::Help => b"Help",
            Self::NewParagraph => b"NewParagraph",
            Self::Paragraph => b"Paragraph",
            Self::Insert => b"Insert",
        }
    }
}

/// Specification for [`add_comment`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommentSpec {
    /// Note body — becomes the annotation `/Contents`.
    pub text: String,
    /// Optional 1-based page range (`"1-3,8"`, `"all"`). `None` = every page.
    pub pages: Option<String>,
    /// Optional `/Rect` `[x0, y0, x1, y1]` in default user space.
    /// `None` places a small box in the top-right corner of the page's
    /// `/MediaBox`.
    pub rect: Option<[f64; 4]>,
    /// Optional author — becomes `/T` (the popup "by" line).
    pub author: Option<String>,
    /// Optional `/Name` icon; defaults to a plain comment.
    pub icon: Option<CommentIcon>,
}

/// Specification for [`add_freetext`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FreeTextSpec {
    /// Visible text — becomes `/Contents`.
    pub text: String,
    /// Optional 1-based page range. `None` = every page.
    pub pages: Option<String>,
    /// Optional `/Rect`; `None` places a box in the top-right corner of
    /// the `/MediaBox`.
    pub rect: Option<[f64; 4]>,
    /// Optional author — becomes `/T`.
    pub author: Option<String>,
    /// Font size written to `/F`. `None` leaves `/F` unset (viewers use
    /// the size inside `/DA`).
    pub font_size: Option<f64>,
    /// Text color `[r, g, b]` in 0.0–1.0 written to `/C`. `None` = black
    /// via the default `/DA`.
    pub color: Option<[f64; 3]>,
    /// Full `/DA` default-appearance override, e.g.
    /// `"0 0 0 rg /Helv 12 Tf"`. `None` uses that same default.
    pub da: Option<String>,
}

/// One row of [`list_annotations`] output.
#[derive(Debug, Clone, Serialize)]
pub struct AnnotationInfo {
    /// 1-based page number the annotation sits on.
    pub page: u32,
    /// `/Subtype` name (e.g. `"Text"`, `"FreeText"`, `"Link"`).
    pub subtype: String,
    /// `/Rect` as `[x0, y0, x1, y1]`, or `None` when absent/malformed.
    pub rect: Option<[f64; 4]>,
    /// `/Contents` decoded as a text string, truncated to 200 chars.
    /// `None` when the annotation has no readable contents.
    pub contents: Option<String>,
    /// `/T` author, decoded as a text string.
    pub author: Option<String>,
}

/// Filter for [`remove_annotations`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotFilter {
    /// Optional 1-based page range. `None` = every page.
    pub pages: Option<String>,
    /// Optional `/Subtype` names to match (e.g. `["Text", "FreeText"]`).
    /// Compared case-insensitively; `None` or empty matches every
    /// subtype on the selected pages.
    pub subtypes: Option<Vec<String>>,
    /// When `true`, remove *all* annotations on the selected pages,
    /// ignoring `subtypes`.
    pub all: bool,
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Parse a `/Rect`-style 4-number array into `[x0, y0, x1, y1]`.
fn rect_from_object(obj: &Object) -> Option<[f64; 4]> {
    let items = obj.as_array().ok()?;
    if items.len() != 4 {
        return None;
    }
    let mut out = [0.0; 4];
    for (i, item) in items.iter().enumerate() {
        out[i] = item.as_float().ok()? as f64;
    }
    Some(out)
}

/// `[x0, y0, x1, y1]` as PDF number objects for a `/Rect` entry.
fn rect_objects(rect: [f64; 4]) -> Vec<Object> {
    rect.iter().map(|v| Object::from(*v as f32)).collect()
}

/// Reject non-finite or degenerate rects; normalize so x0<=x1, y0<=y1.
fn checked_rect(rect: [f64; 4]) -> PdfResult<[f64; 4]> {
    if !rect.iter().all(|v| v.is_finite()) {
        return Err(PdfError::InvalidInput(
            "annotation rect must contain only finite numbers".to_string(),
        ));
    }
    let mut out = rect;
    if out[0] > out[2] {
        out.swap(0, 2);
    }
    if out[1] > out[3] {
        out.swap(1, 3);
    }
    if out[0] == out[2] || out[1] == out[3] {
        return Err(PdfError::InvalidInput(
            "annotation rect must have non-zero width and height".to_string(),
        ));
    }
    Ok(out)
}

/// Default placement: a 40x40pt box in the top-right corner of the
/// page's `/MediaBox`, 10pt from the right edge and 20pt from the top.
fn default_rect(mediabox: [f64; 4]) -> [f64; 4] {
    let w = (mediabox[2] - mediabox[0]).max(0.0);
    let h = (mediabox[3] - mediabox[1]).max(0.0);
    let w = w.min(200.0);
    let h = h.min(120.0);
    [
        mediabox[2] - 10.0 - w,
        mediabox[3] - 20.0 - h,
        mediabox[2] - 10.0,
        mediabox[3] - 20.0,
    ]
}

/// Resolve the `pages` selection of `doc` to `(page_number, page_id)`
/// pairs in page order.
fn selected_page_ids(doc: &Document, pages: Option<&str>) -> PdfResult<Vec<(u32, ObjectId)>> {
    let wanted = selected_pages(doc, pages)?;
    let pages_map = doc.get_pages();
    Ok(wanted
        .into_iter()
        .filter_map(|n| pages_map.get(&n).map(|id| (n, *id)))
        .collect())
}

/// Read a page's `/Annots` array as cloned [`Object`]s — the array may
/// be direct or a single indirection, and entries may be direct
/// dictionaries or references.
fn annots_entries(doc: &Document, page_id: ObjectId) -> Vec<Object> {
    let Ok(page) = doc.get_dictionary(page_id) else {
        return Vec::new();
    };
    let Ok(annots) = page.get(b"Annots") else {
        return Vec::new();
    };
    let array = match annots {
        Object::Array(items) => Some(items.clone()),
        Object::Reference(id) => doc
            .get_object(*id)
            .ok()
            .and_then(|o| o.as_array().ok())
            .cloned(),
        _ => None,
    };
    array.unwrap_or_default()
}

/// Resolve an `/Annots` entry to a dictionary (direct or referenced).
fn resolve_dict<'a>(doc: &'a Document, entry: &'a Object) -> Option<&'a lopdf::Dictionary> {
    match entry {
        Object::Dictionary(d) => Some(d),
        Object::Reference(id) => doc.get_dictionary(*id).ok(),
        _ => None,
    }
}

/// Decode a `/Contents`- or `/T`-style text string, tolerating a direct
/// string or an array of strings (concatenated without separator).
fn decode_text_object(obj: &Object) -> Option<String> {
    match obj {
        Object::String(..) => decode_text_string(obj).ok(),
        Object::Array(items) => {
            let mut out = String::new();
            for item in items {
                match item {
                    Object::String(..) => out.push_str(&decode_text_string(item).ok()?),
                    _ => return None,
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// Truncate to at most `max` *characters* (not bytes) with an ellipsis.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}…")
}

/// Build the shared annotation skeleton (`/Type`, `/Subtype`, `/Rect`,
/// optional `/Contents` and `/T`) and return it as an indirect object.
fn add_annot_object(
    doc: &mut Document,
    subtype: &[u8],
    rect: [f64; 4],
    contents: &str,
    author: Option<&str>,
    extra: lopdf::Dictionary,
) -> PdfResult<ObjectId> {
    let mut dict = dictionary! {
        "Type" => "Annot",
        "Subtype" => lopdf::Object::Name(subtype.to_vec()),
        "Rect" => rect_objects(rect),
        "Contents" => text_string(contents),
    };
    if let Some(author) = author.filter(|a| !a.is_empty()) {
        dict.set("T", text_string(author));
    }
    for (key, value) in extra.iter() {
        dict.set(key.clone(), value.clone());
    }
    doc.try_add_object(Object::Dictionary(dict))
        .map_err(lopdf_err)
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// Append `/Subtype /Text` sticky-note annotations — one per selected
/// page — to `input` and return the rewritten PDF.
///
/// See module docs for the no-appearance-stream rationale.
pub fn add_comment(input: &[u8], spec: &CommentSpec, limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    if spec.text.is_empty() {
        return Err(PdfError::InvalidInput(
            "comment text must not be empty".to_string(),
        ));
    }
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let rect = match spec.rect {
        Some(r) => Some(checked_rect(r)?),
        None => None,
    };

    for (page_no, page_id) in selected_page_ids(&doc, spec.pages.as_deref())? {
        let rect = match rect {
            Some(r) => r,
            None => default_rect(media_box(&doc, page_id)),
        };
        let mut extra = lopdf::Dictionary::new();
        if let Some(icon) = spec.icon {
            extra.set("Name", Object::Name(icon.name().to_vec()));
        }
        let annot_id = add_annot_object(
            &mut doc,
            b"Text",
            rect,
            &spec.text,
            spec.author.as_deref(),
            extra,
        )?;
        append_annot(&mut doc, page_id, Object::Reference(annot_id))?;
        let _ = page_no;
    }

    save_pdf(&mut doc)
}

/// Append `/Subtype /FreeText` annotations — visible text boxes — to
/// `input` and return the rewritten PDF.
///
/// `/DA` defaults to `"0 0 0 rg /Helv 12 Tf"` (black, Helvetica-ish
/// standard-14 alias at 12pt). `spec.da` overrides it verbatim; when
/// overridden the caller is responsible for keeping `/F` and `/C`
/// consistent with it. See module docs for font-resource caveats.
pub fn add_freetext(
    input: &[u8],
    spec: &FreeTextSpec,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    if spec.text.is_empty() {
        return Err(PdfError::InvalidInput(
            "freetext contents must not be empty".to_string(),
        ));
    }
    if let Some(size) = spec.font_size
        && (!size.is_finite() || size <= 0.0)
    {
        return Err(PdfError::InvalidInput(
            "font_size must be a positive finite number".to_string(),
        ));
    }
    if let Some(color) = spec.color
        && !color
            .iter()
            .all(|c| c.is_finite() && (0.0..=1.0).contains(c))
    {
        return Err(PdfError::InvalidInput(
            "color components must be finite numbers in 0.0..=1.0".to_string(),
        ));
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let rect = match spec.rect {
        Some(r) => Some(checked_rect(r)?),
        None => None,
    };

    let da = spec
        .da
        .clone()
        .unwrap_or_else(|| "0 0 0 rg /Helv 12 Tf".to_string());

    for (_page_no, page_id) in selected_page_ids(&doc, spec.pages.as_deref())? {
        let rect = match rect {
            Some(r) => r,
            None => {
                let mb = media_box(&doc, page_id);
                let w = (mb[2] - mb[0]).min(300.0);
                let h = 60.0f64.min((mb[3] - mb[1]).max(0.0));
                [
                    mb[2] - 10.0 - w,
                    mb[3] - 20.0 - h,
                    mb[2] - 10.0,
                    mb[3] - 20.0,
                ]
            }
        };
        let mut extra = lopdf::Dictionary::new();
        extra.set("DA", text_string(&da));
        extra.set("Q", Object::Integer(0));
        if let Some(size) = spec.font_size {
            extra.set("F", Object::Real(size as f32));
        }
        if let Some(color) = spec.color {
            extra.set(
                "C",
                Object::Array(color.iter().map(|c| Object::Real(*c as f32)).collect()),
            );
        }
        let annot_id = add_annot_object(
            &mut doc,
            b"FreeText",
            rect,
            &spec.text,
            spec.author.as_deref(),
            extra,
        )?;
        append_annot(&mut doc, page_id, Object::Reference(annot_id))?;
    }

    save_pdf(&mut doc)
}

/// Append `entry` to the page's `/Annots` array, creating it when
/// absent. Any pre-existing array (direct or indirect) is preserved by
/// cloning its entries into a fresh direct array.
fn append_annot(doc: &mut Document, page_id: ObjectId, entry: Object) -> PdfResult<()> {
    let mut entries = annots_entries(doc, page_id);
    entries.push(entry);
    let page = doc.get_dictionary_mut(page_id).map_err(lopdf_err)?;
    page.set("Annots", Object::Array(entries));
    Ok(())
}

/// Walk every page's `/Annots` and return one [`AnnotationInfo`] per
/// annotation dictionary found, in page order then array order.
///
/// Non-dictionary `/Annots` entries are skipped. Contents are truncated
/// to 200 characters.
pub fn list_annotations(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<AnnotationInfo>> {
    let doc = load_pdf(input, limits)?;
    // Read-only op: encrypted documents still list fine.

    let pages_map = doc.get_pages();
    let mut out = Vec::new();
    for (page_no, page_id) in &pages_map {
        for entry in annots_entries(&doc, *page_id) {
            let Some(annot) = resolve_dict(&doc, &entry) else {
                continue;
            };
            let subtype = annot
                .get(b"Subtype")
                .ok()
                .and_then(|o| o.as_name().ok())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_else(|| "Unknown".to_string());
            let rect = annot.get(b"Rect").ok().and_then(rect_from_object);
            let contents = annot
                .get(b"Contents")
                .ok()
                .and_then(decode_text_object)
                .map(|c| truncate_chars(&c, 200));
            let author = annot.get(b"T").ok().and_then(decode_text_object);
            out.push(AnnotationInfo {
                page: *page_no,
                subtype,
                rect,
                contents,
                author,
            });
        }
    }
    Ok(out)
}

/// Remove annotations matching `filter` from `input` and return the
/// rewritten PDF.
///
/// Matching rules:
///
/// - `all: true` — every `/Annots` entry on the pages selected by
///   `pages` (`None` = all pages) is removed.
/// - `all: false` — at least one of `pages`/`subtypes` must be given
///   (otherwise [`PdfError::InvalidInput`]). An annotation is removed
///   when its page is selected (or `pages` is `None`) **and** its
///   `/Subtype` is in `subtypes` (or `subtypes` is `None`/empty).
///   Subtype matching is case-insensitive. Entries that do not resolve
///   to a dictionary are never removed in this mode.
///
/// When a page's `/Annots` array becomes empty the key is deleted from
/// the page dictionary.
pub fn remove_annotations(
    input: &[u8],
    filter: &AnnotFilter,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let has_subtypes = filter.subtypes.as_ref().is_some_and(|s| !s.is_empty());
    if !filter.all && filter.pages.is_none() && !has_subtypes {
        return Err(PdfError::InvalidInput(
            "remove_annotations needs all=true or at least one of pages/subtypes".to_string(),
        ));
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    // Resolve the page selection once; `pages: None` means every page.
    let selected: Option<std::collections::BTreeSet<u32>> = match &filter.pages {
        Some(spec) => Some(
            parse_page_range(spec, doc.get_pages().len() as u32)?
                .into_iter()
                .collect(),
        ),
        None => None,
    };
    let wanted_subtypes: Option<std::collections::BTreeSet<String>> = if filter.all {
        None
    } else {
        filter.subtypes.as_ref().map(|list| {
            list.iter()
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        })
    };

    let page_ids: Vec<(u32, ObjectId)> = doc
        .get_pages()
        .into_iter()
        .filter(|(n, _)| selected.as_ref().is_none_or(|s| s.contains(n)))
        .collect();

    for (page_no, page_id) in page_ids {
        let entries = annots_entries(&doc, page_id);
        if entries.is_empty() {
            continue;
        }
        let remaining: Vec<Object> = entries
            .into_iter()
            .filter(|entry| {
                if filter.all {
                    return false;
                }
                let Some(annot) = resolve_dict(&doc, entry) else {
                    return true; // keep unresolvable entries
                };
                if let Some(wanted) = &wanted_subtypes {
                    let subtype = annot
                        .get(b"Subtype")
                        .ok()
                        .and_then(|o| o.as_name().ok())
                        .map(|n| String::from_utf8_lossy(n).to_ascii_lowercase());
                    match subtype {
                        Some(name) if wanted.contains(&name) => return false,
                        Some(_) => return true,
                        // No /Subtype: cannot match a subtype filter.
                        None => return true,
                    }
                }
                // No subtype filter: every dictionary annotation on a
                // selected page is removed.
                false
            })
            .collect();
        let page = doc.get_dictionary_mut(page_id).map_err(lopdf_err)?;
        if remaining.is_empty() {
            page.remove(b"Annots");
        } else {
            page.set("Annots", Object::Array(remaining));
        }
        let _ = page_no;
    }

    save_pdf(&mut doc)
}

#[cfg(test)]
mod tests {
    use lopdf::{Document, decode_text_string};

    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    fn annots_of(doc: &Document, page: u32) -> Vec<lopdf::Dictionary> {
        let page_id = doc.get_pages()[&page];
        let page_dict = doc.get_dictionary(page_id).unwrap();
        let Ok(annots) = page_dict.get(b"Annots") else {
            return Vec::new();
        };
        let items = match annots {
            Object::Array(items) => items.clone(),
            Object::Reference(id) => doc.get_object(*id).unwrap().as_array().unwrap().clone(),
            _ => panic!("Annots is not an array"),
        };
        items
            .iter()
            .map(|entry| match entry {
                Object::Reference(id) => doc.get_dictionary(*id).unwrap().clone(),
                Object::Dictionary(d) => d.clone(),
                _ => panic!("annot entry is not a dict"),
            })
            .collect()
    }

    #[test]
    fn add_comment_creates_text_annot_on_each_page() {
        let input = pdf_with_pages(&["one", "two", "three"]);
        let spec = CommentSpec {
            text: "please review".to_string(),
            pages: Some("1-2".to_string()),
            rect: None,
            author: Some("alice".to_string()),
            icon: Some(CommentIcon::Key),
        };
        let out = add_comment(&input, &spec, &limits()).unwrap();
        let doc = Document::load_mem(&out).unwrap();

        let infos = list_annotations(&out, &limits()).unwrap();
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].page, 1);
        assert_eq!(infos[0].subtype, "Text");
        assert_eq!(infos[0].contents.as_deref(), Some("please review"));
        assert_eq!(infos[0].author.as_deref(), Some("alice"));
        assert!(infos[0].rect.is_some());
        assert!(infos[1].page == 2);

        // Verify the raw dictionary entries.
        for page in [1u32, 2] {
            let annots = annots_of(&doc, page);
            assert_eq!(annots.len(), 1);
            assert_eq!(
                annots[0].get(b"Subtype").unwrap().as_name().unwrap(),
                b"Text".as_slice()
            );
            assert_eq!(
                annots[0].get(b"Name").unwrap().as_name().unwrap(),
                b"Key".as_slice()
            );
            assert_eq!(
                decode_text_string(annots[0].get(b"Contents").unwrap()).unwrap(),
                "please review"
            );
        }
        // Page 3 untouched.
        let page3 = doc.get_dictionary(doc.get_pages()[&3]).unwrap();
        assert!(page3.get(b"Annots").is_err());
    }

    #[test]
    fn add_comment_utf16_contents_roundtrip() {
        let input = pdf_with_pages(&["x"]);
        let spec = CommentSpec {
            text: "héllo wörld ✓".to_string(),
            pages: None,
            rect: Some([10.0, 10.0, 110.0, 60.0]),
            author: Some("José".to_string()),
            icon: None,
        };
        let out = add_comment(&input, &spec, &limits()).unwrap();
        let infos = list_annotations(&out, &limits()).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].contents.as_deref(), Some("héllo wörld ✓"));
        assert_eq!(infos[0].author.as_deref(), Some("José"));
        assert_eq!(infos[0].rect, Some([10.0, 10.0, 110.0, 60.0]));
    }

    #[test]
    fn add_freetext_writes_da_and_contents() {
        let input = pdf_with_pages(&["p1", "p2"]);
        let spec = FreeTextSpec {
            text: "DRAFT".to_string(),
            pages: Some("2".to_string()),
            rect: Some([50.0, 700.0, 250.0, 740.0]),
            author: None,
            font_size: Some(18.0),
            color: Some([1.0, 0.0, 0.0]),
            da: None,
        };
        let out = add_freetext(&input, &spec, &limits()).unwrap();
        let doc = Document::load_mem(&out).unwrap();
        let annots = annots_of(&doc, 2);
        assert_eq!(annots.len(), 1);
        assert_eq!(
            annots[0].get(b"Subtype").unwrap().as_name().unwrap(),
            b"FreeText".as_slice()
        );
        let da = decode_text_string(annots[0].get(b"DA").unwrap()).unwrap();
        assert!(da.contains("/Helv 12 Tf"), "DA was {da}");
        assert_eq!(annots[0].get(b"F").unwrap().as_float().unwrap(), 18.0);
        let color = annots[0].get(b"C").unwrap().as_array().unwrap();
        assert_eq!(color.len(), 3);
        assert_eq!(infos_len(&out), 1);
    }

    fn infos_len(pdf: &[u8]) -> usize {
        list_annotations(pdf, &EngineLimits::default())
            .unwrap()
            .len()
    }

    #[test]
    fn list_reports_all_subtypes() {
        let input = pdf_with_pages(&["a"]);
        let comment = CommentSpec {
            text: "note".to_string(),
            pages: None,
            rect: None,
            author: None,
            icon: None,
        };
        let mut pdf = add_comment(&input, &comment, &limits()).unwrap();
        let free = FreeTextSpec {
            text: "visible".to_string(),
            pages: None,
            rect: None,
            author: Some("bob".to_string()),
            font_size: None,
            color: None,
            da: None,
        };
        pdf = add_freetext(&pdf, &free, &limits()).unwrap();

        let infos = list_annotations(&pdf, &limits()).unwrap();
        assert_eq!(infos.len(), 2);
        let subtypes: Vec<&str> = infos.iter().map(|i| i.subtype.as_str()).collect();
        assert!(subtypes.contains(&"Text"));
        assert!(subtypes.contains(&"FreeText"));
        let ft = infos.iter().find(|i| i.subtype == "FreeText").unwrap();
        assert_eq!(ft.contents.as_deref(), Some("visible"));
        assert_eq!(ft.author.as_deref(), Some("bob"));
    }

    #[test]
    fn remove_by_subtype_and_page() {
        let input = pdf_with_pages(&["a", "b"]);
        let comment = CommentSpec {
            text: "sticky".to_string(),
            pages: None,
            rect: None,
            author: None,
            icon: None,
        };
        let free = FreeTextSpec {
            text: "box".to_string(),
            pages: None,
            rect: None,
            author: None,
            font_size: None,
            color: None,
            da: None,
        };
        let mut pdf = add_comment(&input, &comment, &limits()).unwrap();
        pdf = add_freetext(&pdf, &free, &limits()).unwrap();
        assert_eq!(list_annotations(&pdf, &limits()).unwrap().len(), 4);

        // Remove only FreeText on page 1.
        let filter = AnnotFilter {
            pages: Some("1".to_string()),
            subtypes: Some(vec!["freetext".to_string()]),
            all: false,
        };
        let out = remove_annotations(&pdf, &filter, &limits()).unwrap();
        let infos = list_annotations(&out, &limits()).unwrap();
        assert_eq!(infos.len(), 3);
        assert!(
            infos
                .iter()
                .all(|i| !(i.page == 1 && i.subtype == "FreeText"))
        );

        // Remove all remaining annotations.
        let filter = AnnotFilter {
            pages: None,
            subtypes: None,
            all: true,
        };
        let out = remove_annotations(&out, &filter, &limits()).unwrap();
        assert!(list_annotations(&out, &limits()).unwrap().is_empty());
        let doc = Document::load_mem(&out).unwrap();
        for (_, page_id) in doc.get_pages() {
            let page = doc.get_dictionary(page_id).unwrap();
            assert!(page.get(b"Annots").is_err(), "Annots key should be gone");
        }
    }

    #[test]
    fn remove_rejects_empty_filter() {
        let input = pdf_with_pages(&["a"]);
        let filter = AnnotFilter {
            pages: None,
            subtypes: None,
            all: false,
        };
        assert!(matches!(
            remove_annotations(&input, &filter, &limits()),
            Err(PdfError::InvalidInput(_))
        ));
        // Empty subtype list also counts as empty.
        let filter = AnnotFilter {
            pages: None,
            subtypes: Some(vec![]),
            all: false,
        };
        assert!(remove_annotations(&input, &filter, &limits()).is_err());
    }

    #[test]
    fn add_validates_inputs() {
        let input = pdf_with_pages(&["a"]);
        let bad_rect = CommentSpec {
            text: "x".to_string(),
            pages: None,
            rect: Some([f64::NAN, 0.0, 1.0, 1.0]),
            author: None,
            icon: None,
        };
        assert!(matches!(
            add_comment(&input, &bad_rect, &limits()),
            Err(PdfError::InvalidInput(_))
        ));
        let empty_text = CommentSpec {
            text: String::new(),
            pages: None,
            rect: None,
            author: None,
            icon: None,
        };
        assert!(add_comment(&input, &empty_text, &limits()).is_err());
        let bad_range = CommentSpec {
            text: "x".to_string(),
            pages: Some("9".to_string()),
            rect: None,
            author: None,
            icon: None,
        };
        assert!(matches!(
            add_comment(&input, &bad_range, &limits()),
            Err(PdfError::InvalidPageRange(_))
        ));
    }
}
