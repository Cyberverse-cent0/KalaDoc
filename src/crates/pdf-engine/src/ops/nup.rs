//! N-up: pack several source pages onto each output page.
//!
//! Every selected source page is imported into the document as a Form
//! XObject — the same technique [`overlay`] uses — except that no
//! object copying is needed: the forms live in the *same* document as
//! their source pages, so `/Resources` references stay valid as-is.
//! Each form is then painted onto a fresh output page with a `cm`
//! transform via `Do`, `cols * rows` pages per sheet in reading order
//! (left-to-right, top-to-bottom), each scaled uniformly to fit its
//! cell and centered.
//!
//! The output document consists *only* of the new layout pages: a
//! fresh `/Pages` root is created, the catalog is repointed at it, and
//! `lopdf`'s `prune_objects` drops everything that became unreachable
//! (the original page objects, page tree and any page-only resources).
//!
//! # Documented choices and limitations
//!
//! - `cols` and `rows` must each be in `1..=6`.
//! - The output page size is taken from the **first selected page's**
//!   `/MediaBox` (`[0, 0, w, h]`). Documents whose pages have mixed
//!   sizes still work — every source page is scaled to fit its own
//!   cell — but the sheet size follows page 1 of the selection.
//! - Only `/MediaBox` geometry is honored. `/CropBox`, `/Rotate`,
//!   per-page scaling quirks and annotations of the source pages are
//!   not carried over (annotations are dropped entirely, as with
//!   `overlay`).
//! - The last output page may contain empty cells when the selected
//!   page count is not a multiple of `cols * rows`; empty cells stay
//!   blank.
//! - Document-level structures that referenced the original page
//!   objects (outlines, named destinations, structure tree) are not
//!   remapped and may dangle after pruning.
//!
//! [`overlay`]: super::overlay

use lopdf::{Document, Object, ObjectId, Stream, dictionary};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::crop::{DEFAULT_MEDIA_BOX, inherited_rect, number_object};
use super::flatten::pdf_num;
use super::load::{check_page_limit, load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::pages::parse_page_range;

/// Largest allowed value for `cols` / `rows` in [`nup`].
pub(crate) const NUP_MAX_DIM: u32 = 6;

/// Pack the selected source pages `cols * rows` per output page.
///
/// `pages` selects source pages with the shared 1-based range spec;
/// `None` selects all of them. Pages are laid out in reading order —
/// row 0 is the top row, columns fill left-to-right — and each is
/// scaled uniformly to fit its cell. See the module docs for the
/// output-size rule and limitations.
pub fn nup(
    input: &[u8],
    cols: u32,
    rows: u32,
    pages: Option<&str>,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    if !(1..=NUP_MAX_DIM).contains(&cols) {
        return Err(PdfError::InvalidInput(format!(
            "cols must be in 1..={NUP_MAX_DIM}, got {cols}"
        )));
    }
    if !(1..=NUP_MAX_DIM).contains(&rows) {
        return Err(PdfError::InvalidInput(format!(
            "rows must be in 1..={NUP_MAX_DIM}, got {rows}"
        )));
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let numbers = selected_pages(&doc, pages)?;
    let forms = import_as_forms(&mut doc, &numbers)?;

    // Sheet size follows the first selected page's MediaBox.
    let first = forms[0].bbox;
    let sheet_w = (first[2] - first[0]).abs();
    let sheet_h = (first[3] - first[1]).abs();
    if !(sheet_w.is_finite() && sheet_h.is_finite() && sheet_w > 0.0 && sheet_h > 0.0) {
        return Err(PdfError::InvalidInput(
            "first selected page has a degenerate MediaBox".to_string(),
        ));
    }

    let per_sheet = (cols * rows) as usize;
    let sheet_count = numbers.len().div_ceil(per_sheet);
    let mut outs: Vec<OutPage> = (0..sheet_count)
        .map(|_| OutPage {
            mediabox: [0.0, 0.0, sheet_w, sheet_h],
            cropbox: None,
            paints: Vec::new(),
        })
        .collect();

    let cell_w = sheet_w / f64::from(cols);
    let cell_h = sheet_h / f64::from(rows);
    for (i, form) in forms.iter().enumerate() {
        let cell = i % per_sheet;
        let sheet = i / per_sheet;
        let col = (cell % cols as usize) as f64;
        let row = (cell / cols as usize) as f64; // 0 = top row

        let src_w = (form.bbox[2] - form.bbox[0]).abs();
        let src_h = (form.bbox[3] - form.bbox[1]).abs();
        if !(src_w > 0.0 && src_h > 0.0) {
            return Err(PdfError::InvalidInput(format!(
                "page {} has a degenerate MediaBox",
                form.number
            )));
        }
        // Uniform fit into the cell, centered.
        let s = (cell_w / src_w).min(cell_h / src_h);
        let dw = src_w * s;
        let dh = src_h * s;
        let dst_x = col * cell_w + (cell_w - dw) / 2.0;
        // PDF user space is y-up; row 0 is the top of the sheet.
        let dst_y = sheet_h - (row + 1.0) * cell_h + (cell_h - dh) / 2.0;

        // Map the source MediaBox rect onto the destination rect:
        // form point p -> s * p + t, with the bbox min corner landing
        // on (dst_x, dst_y).
        let m = [
            s,
            0.0,
            0.0,
            s,
            dst_x - form.bbox[0] * s,
            dst_y - form.bbox[1] * s,
        ];
        outs[sheet].paints.push(Paint {
            form_id: form.form_id,
            matrix: m,
            clip: None,
        });
    }

    emit_pages(&mut doc, &outs, limits)?;
    save_pdf(&mut doc)
}

// ---------------------------------------------------------------------------
// Shared layout machinery.
//
// Used by `nup`, `poster`, `single_page` and `booklet`: source pages are
// turned into Form XObjects inside the *same* document (no object copying
// or id remapping is required because nothing leaves the document), and
// brand-new output pages are emitted under a fresh `/Pages` root.
// ---------------------------------------------------------------------------

/// A source page imported as a Form XObject.
pub(crate) struct SourcePage {
    /// 1-based page number in the source document.
    pub number: u32,
    /// Effective `/MediaBox` as `[x0, y0, x1, y1]`.
    pub bbox: [f64; 4],
    /// Object id of the Form XObject stream.
    pub form_id: ObjectId,
}

/// One `Do` paint operation on an output page.
pub(crate) struct Paint {
    /// Form XObject to paint.
    pub form_id: ObjectId,
    /// `cm` matrix `[a b c d e f]` mapping form space to page space.
    pub matrix: [f64; 6],
    /// Optional clipping rectangle `[x, y, w, h]` in output-page space,
    /// emitted as a `re W n` before the paint. `None` = no clip.
    pub clip: Option<[f64; 4]>,
}

/// A fully described output page for [`emit_pages`].
pub(crate) struct OutPage {
    /// `/MediaBox` (and `/CropBox` when `cropbox` differs) of the page.
    pub mediabox: [f64; 4],
    /// Optional `/CropBox`; `None` leaves only `/MediaBox`.
    pub cropbox: Option<[f64; 4]>,
    /// Paint operations in emission order.
    pub paints: Vec<Paint>,
}

/// Selected 1-based page numbers: `Some(spec)` via the shared range
/// parser, `None` = every page. An empty document is rejected here so
/// callers never index into an empty result.
pub(crate) fn selected_pages(doc: &Document, pages: Option<&str>) -> PdfResult<Vec<u32>> {
    let page_count = doc.get_pages().len() as u32;
    let selected = match pages {
        Some(spec) => parse_page_range(spec, page_count)?,
        None => (1..=page_count).collect(),
    };
    if selected.is_empty() {
        return Err(PdfError::InvalidInput(
            "document has no pages to lay out".to_string(),
        ));
    }
    Ok(selected)
}

/// Turn each selected page into a Form XObject inside `doc`.
///
/// The form's `/BBox` is the page's effective `/MediaBox` and its
/// `/Resources` is the page's effective `/Resources` object — both
/// referenced in place, which is safe because source and output live
/// in the same document. Page content is read via
/// [`Document::get_page_content`], which concatenates and decompresses
/// the page's content streams.
pub(crate) fn import_as_forms(
    doc: &mut Document,
    numbers: &[u32],
) -> PdfResult<Vec<SourcePage>> {
    let pages_map = doc.get_pages();
    let mut out = Vec::with_capacity(numbers.len());
    for &number in numbers {
        let Some(&page_id) = pages_map.get(&number) else {
            return Err(PdfError::InvalidPageRange(format!(
                "page {number} not found in document"
            )));
        };
        let bbox = inherited_rect(doc, page_id, b"MediaBox")?.unwrap_or(DEFAULT_MEDIA_BOX);
        let content = doc.get_page_content(page_id);

        let mut form_dict = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "FormType" => 1,
            "BBox" => Object::Array(
                bbox.iter().map(|&v| number_object(v)).collect()
            ),
        };
        if let Some(res) = resources_object(doc, page_id) {
            form_dict.set("Resources", res);
        }
        let mut stream = Stream::new(form_dict, content);
        // Best-effort: compress the form stream when it pays off.
        let _ = stream.compress();
        let form_id = doc
            .try_add_object(Object::Stream(stream))
            .map_err(lopdf_err)?;
        out.push(SourcePage {
            number,
            bbox,
            form_id,
        });
    }
    Ok(out)
}

/// The page's effective `/Resources` as a raw [`Object`] (indirect
/// reference or direct dictionary), walking `/Parent` nodes — same
/// scheme as `overlay`, minus the id remapping (same document).
fn resources_object(doc: &Document, page_id: ObjectId) -> Option<Object> {
    let mut current = Some(page_id);
    let mut depth = 0usize;
    while let Some(id) = current {
        depth += 1;
        if depth > 64 {
            break;
        }
        let Ok(dict) = doc.get_dictionary(id) else {
            break;
        };
        match dict.get(b"Resources") {
            Ok(obj @ (Object::Reference(_) | Object::Dictionary(_))) => {
                return Some(obj.clone());
            }
            _ => {}
        }
        current = dict.get(b"Parent").ok().and_then(|p| p.as_reference().ok());
    }
    None
}

/// Replace the document's page tree with brand-new pages described by
/// `outs`, then drop everything the old tree leaves unreachable.
///
/// Each output page gets a fresh indirect `/Resources` dictionary whose
/// `/XObject` subdictionary names every painted form (`Fm0`, `Fm1`, ...
/// per page). `limits.max_pages` is enforced against `outs.len()`
/// before anything is created.
pub(crate) fn emit_pages(
    doc: &mut Document,
    outs: &[OutPage],
    limits: &EngineLimits,
) -> PdfResult<()> {
    check_page_limit(outs.len(), limits)?;
    if outs.is_empty() {
        return Err(PdfError::InvalidInput(
            "layout produced zero output pages".to_string(),
        ));
    }

    let pages_id = doc
        .try_add_object(dictionary! {
            "Type" => "Pages",
            "Kids" => Object::Array(Vec::new()),
            "Count" => 0,
        })
        .map_err(lopdf_err)?;

    let mut kids = Vec::with_capacity(outs.len());
    for out in outs {
        // Fresh /Resources: /XObject names each painted form.
        let mut xobjects = lopdf::Dictionary::new();
        let mut content = String::new();
        let mut name_of: std::collections::HashMap<ObjectId, Vec<u8>> =
            std::collections::HashMap::new();
        let mut next_name = 0u32;
        for paint in &out.paints {
            let name = match name_of.get(&paint.form_id) {
                Some(n) => n.clone(),
                None => {
                    let n = format!("Fm{next_name}").into_bytes();
                    next_name += 1;
                    name_of.insert(paint.form_id, n.clone());
                    n
                }
            };
            xobjects.set(name.clone(), Object::Reference(paint.form_id));
            if let Some([x, y, w, h]) = paint.clip {
                content.push_str(&format!(
                    "{} {} {} {} re W n\n",
                    pdf_num(x),
                    pdf_num(y),
                    pdf_num(w),
                    pdf_num(h)
                ));
            }
            let m = paint.matrix;
            content.push_str(&format!(
                "q\n{} {} {} {} {} {} cm\n/{} Do\nQ\n",
                pdf_num(m[0]),
                pdf_num(m[1]),
                pdf_num(m[2]),
                pdf_num(m[3]),
                pdf_num(m[4]),
                pdf_num(m[5]),
                String::from_utf8_lossy(&name)
            ));
        }

        let resources_id = doc
            .try_add_object(Object::Dictionary(dictionary! {
                "XObject" => Object::Dictionary(xobjects),
            }))
            .map_err(lopdf_err)?;
        let contents_id = doc
            .try_add_object(Object::Stream(Stream::new(
                lopdf::Dictionary::new(),
                content.into_bytes(),
            )))
            .map_err(lopdf_err)?;

        let media = Object::Array(
            out.mediabox.iter().map(|&v| number_object(v)).collect(),
        );
        let mut page_dict = dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => media,
            "Resources" => Object::Reference(resources_id),
            "Contents" => Object::Reference(contents_id),
        };
        if let Some(cb) = out.cropbox {
            page_dict.set(
                "CropBox",
                Object::Array(cb.iter().map(|&v| number_object(v)).collect()),
            );
        }
        let page_id = doc
            .try_add_object(Object::Dictionary(page_dict))
            .map_err(lopdf_err)?;
        kids.push(Object::Reference(page_id));
    }

    doc.get_dictionary_mut(pages_id)
        .map_err(lopdf_err)?
        .set("Kids", Object::Array(kids));
    doc.get_dictionary_mut(pages_id)
        .map_err(lopdf_err)?
        .set("Count", Object::Integer(outs.len() as i64));

    // Repoint the catalog at the new page tree.
    doc.catalog_mut()
        .map_err(lopdf_err)?
        .set("Pages", Object::Reference(pages_id));

    // Drop the old page objects, the old page tree and anything else
    // that became unreachable. Objects still referenced by the new
    // forms (fonts, images, resource dictionaries) survive.
    doc.prune_objects();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::{
        page_count as test_page_count, pdf_with_pages,
    };
    use lopdf::Document;

    fn media_box(pdf: &[u8], page: u32) -> Vec<f64> {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&page];
        let dict = doc.get_dictionary(page_id).unwrap();
        dict.get(b"MediaBox")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|o| match o {
                Object::Integer(v) => *v as f64,
                Object::Real(v) => f64::from(*v),
                _ => panic!("non-numeric MediaBox entry"),
            })
            .collect()
    }

    #[test]
    fn packs_pages_into_grid() {
        // test_utils pages are 595 x 842.
        let pdf = pdf_with_pages(&["a", "b", "c", "d", "e"]);
        let out = nup(&pdf, 2, 2, None, &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 2); // ceil(5 / 4)
        let mb = media_box(&out, 1);
        assert!((mb[2] - 595.0).abs() < 0.01 && (mb[3] - 842.0).abs() < 0.01);
    }

    #[test]
    fn honors_page_selection() {
        let pdf = pdf_with_pages(&["a", "b", "c", "d"]);
        let out = nup(&pdf, 1, 4, Some("1-4"), &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 1);
        // Selecting a single page with a 2x1 grid still yields 1 sheet.
        let out = nup(&pdf, 2, 1, Some("3"), &EngineLimits::default()).unwrap();
        assert_eq!(test_page_count(&out), 1);
    }

    #[test]
    fn rejects_bad_grid() {
        let pdf = pdf_with_pages(&["a"]);
        assert!(nup(&pdf, 0, 1, None, &EngineLimits::default()).is_err());
        assert!(nup(&pdf, 1, 0, None, &EngineLimits::default()).is_err());
        assert!(nup(&pdf, 7, 1, None, &EngineLimits::default()).is_err());
        assert!(nup(&pdf, 1, 7, None, &EngineLimits::default()).is_err());
        assert!(nup(&pdf, 1, 1, Some("9"), &EngineLimits::default()).is_err());
        assert!(nup(b"junk", 1, 1, None, &EngineLimits::default()).is_err());
    }

    #[test]
    fn enforces_output_page_limit() {
        let pdf = pdf_with_pages(&["a", "b", "c", "d", "e"]);
        let limits = EngineLimits {
            max_pages: Some(1),
            ..EngineLimits::default()
        };
        let err = nup(&pdf, 1, 1, None, &limits).unwrap_err();
        assert!(matches!(err, PdfError::LimitExceeded(_)), "got {err:?}");
    }
}
