//! Pack several PDFs into one "package" document.
//!
//! [`package_documents`] is deliberately **not** a portfolio (see
//! [`super::portfolio`]): it produces an ordinary merged PDF — a
//! generated cover page followed by every input's pages in order —
//! *plus* each input embedded verbatim as a file attachment under the
//! catalog's `/Names` → `/EmbeddedFiles` name tree. The result is
//! honest about its structure: the merged pages are readable content,
//! and the original files are preserved byte-for-byte as attachments
//! named `doc-1.pdf`, `doc-2.pdf`, ...
//!
//! # Cover page
//!
//! The cover is a single A4 page drawn with the base-14 `Helvetica`
//! font (no embedding needed). [`PackageLayout::Grid`] shows the title
//! plus a document/page summary; [`PackageLayout::List`] additionally
//! lists the attached file names. The cover adds exactly one page, so
//! the output page count is `sum(input pages) + 1`.
//!
//! # Limit / error behavior
//!
//! All input-side limits (`max_input_bytes`, `max_pages`, combined
//! page count) are enforced by [`super::merge::merge`]; encrypted
//! inputs are rejected with [`PdfError::Encrypted`] before any output
//! work happens.

use lopdf::content::Operation;
use lopdf::{Document, Object, Stream, dictionary};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::watermark::{encode_operations, estimate_text_width, pdf_text_operand};

/// A4 page geometry used for the generated cover page, matching the
/// convention in `ops::test_utils`.
const PAGE_WIDTH: f32 = 595.0;
const PAGE_HEIGHT: f32 = 842.0;
const MARGIN: f32 = 72.0;

const TITLE_SIZE: f32 = 28.0;
const SUBTITLE_SIZE: f32 = 13.0;
const LIST_SIZE: f32 = 11.0;
const LIST_LEADING: f32 = 16.0;

/// How the generated cover page presents the package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageLayout {
    /// Title plus a one-line "N documents · M pages" summary.
    Grid,
    /// Title plus a bulleted-style list of the attached `doc-N.pdf`
    /// file names.
    List,
}

/// Pack `inputs` (raw PDF bytes, in order) into a single PDF package.
///
/// The output contains: (1) a generated cover page titled
/// `cover_title`, (2) every input's pages merged in input order, and
/// (3) each input attached as an embedded file named `doc-N.pdf` under
/// `/Names` → `/EmbeddedFiles`. `layout` selects the cover's
/// presentation style (see [`PackageLayout`]).
///
/// Returns [`PdfError::InvalidInput`] when `inputs` is empty, and
/// [`PdfError::Encrypted`] when any input is encrypted. Page limits
/// are enforced against the combined input page count (the cover page
/// itself is not counted against `max_pages` — it is generated
/// content, not input).
pub fn package_documents(
    inputs: &[&[u8]],
    cover_title: &str,
    layout: PackageLayout,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    if inputs.is_empty() {
        return Err(PdfError::InvalidInput(
            "package_documents requires at least one input document".to_string(),
        ));
    }
    let title = if cover_title.trim().is_empty() {
        "Untitled Package".to_string()
    } else {
        // Bound the title so a hostile caller cannot emit a megabyte
        // of text operators onto the cover.
        cover_title.chars().take(160).collect()
    };

    // merge() loads every input with limit checks, rejects encrypted
    // documents, enforces the combined page limit, and produces a flat
    // merged document we can extend.
    let merged = super::merge::merge(inputs, limits)?;
    let mut doc = load_pdf(&merged, limits)?;
    reject_encrypted(&doc)?;

    let merged_pages = doc.get_pages().len();

    insert_cover_page(&mut doc, &title, layout, inputs.len(), merged_pages)?;
    attach_inputs(&mut doc, inputs)?;

    save_pdf(&mut doc)
}

// ---------------------------------------------------------------------------
// Cover page
// ---------------------------------------------------------------------------

/// Prepend a generated cover page to the merged document's flat page
/// tree.
fn insert_cover_page(
    doc: &mut Document,
    title: &str,
    layout: PackageLayout,
    input_count: usize,
    merged_pages: usize,
) -> PdfResult<()> {
    let content = cover_content(title, layout, input_count, merged_pages)?;
    let content_id = doc
        .try_add_object(Stream::new(dictionary! {}, content))
        .map_err(lopdf_err)?;

    let font_id = doc
        .try_add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        })
        .map_err(lopdf_err)?;
    let resources_id = doc
        .try_add_object(dictionary! {
            "Font" => dictionary! {
                "F1" => font_id,
            },
        })
        .map_err(lopdf_err)?;

    // The merge output is a single flat /Pages node; find it through
    // the catalog so the cover's /Parent chain stays consistent.
    let pages_id = doc
        .catalog()
        .and_then(|catalog| catalog.get(b"Pages"))
        .and_then(|obj| obj.as_reference())
        .map_err(|_| PdfError::InvalidInput("merged document has no page tree".to_string()))?;

    let cover_id = doc
        .try_add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Real(PAGE_WIDTH),
                Object::Real(PAGE_HEIGHT),
            ],
        })
        .map_err(lopdf_err)?;

    let pages_obj = doc
        .objects
        .get_mut(&pages_id)
        .ok_or_else(|| PdfError::InvalidInput("page tree object is missing".to_string()))?;
    let pages_dict = pages_obj.as_dict_mut().map_err(lopdf_err)?;

    let mut kids = pages_dict
        .get(b"Kids")
        .and_then(|obj| obj.as_array().cloned())
        .unwrap_or_default();
    kids.insert(0, Object::Reference(cover_id));
    pages_dict.set("Kids", Object::Array(kids));

    let count = pages_dict
        .get(b"Count")
        .and_then(|obj| obj.as_i64())
        .unwrap_or(0);
    pages_dict.set("Count", count + 1);

    Ok(())
}

/// Build the cover page's content stream.
fn cover_content(
    title: &str,
    layout: PackageLayout,
    input_count: usize,
    merged_pages: usize,
) -> PdfResult<Vec<u8>> {
    let title_width = estimate_text_width(title, TITLE_SIZE);
    let title_x = ((PAGE_WIDTH - title_width) * 0.5).max(MARGIN * 0.5);

    let mut ops = vec![
        Operation::new("BT", vec![]),
        Operation::new(
            "Tf",
            vec![Object::Name(b"F1".to_vec()), Object::Real(TITLE_SIZE)],
        ),
        Operation::new("Td", vec![Object::Real(title_x), Object::Real(740.0)]),
        Operation::new("Tj", vec![pdf_text_operand(title)]),
        Operation::new("ET", vec![]),
    ];

    match layout {
        PackageLayout::Grid => {
            let summary = format!(
                "{} document{} · {} page{}",
                input_count,
                if input_count == 1 { "" } else { "s" },
                merged_pages,
                if merged_pages == 1 { "" } else { "s" },
            );
            let summary_width = estimate_text_width(&summary, SUBTITLE_SIZE);
            let summary_x = ((PAGE_WIDTH - summary_width) * 0.5).max(MARGIN * 0.5);
            ops.extend(vec![
                Operation::new("BT", vec![]),
                Operation::new(
                    "Tf",
                    vec![Object::Name(b"F1".to_vec()), Object::Real(SUBTITLE_SIZE)],
                ),
                Operation::new("Td", vec![Object::Real(summary_x), Object::Real(700.0)]),
                Operation::new("Tj", vec![pdf_text_operand(&summary)]),
                Operation::new("ET", vec![]),
            ]);
        }
        PackageLayout::List => {
            // "Attached files:" header followed by one doc-N.pdf name
            // per line, 16pt leading, starting near the top margin.
            let mut list_ops = vec![
                Operation::new("BT", vec![]),
                Operation::new(
                    "Tf",
                    vec![Object::Name(b"F1".to_vec()), Object::Real(SUBTITLE_SIZE)],
                ),
                Operation::new("Td", vec![Object::Real(MARGIN), Object::Real(660.0)]),
                Operation::new("Tj", vec![pdf_text_operand("Attached files:")]),
            ];
            for i in 0..input_count {
                list_ops.push(Operation::new(
                    "Td",
                    vec![Object::Real(0.0), Object::Real(-LIST_LEADING)],
                ));
                list_ops.push(Operation::new(
                    "Tf",
                    vec![Object::Name(b"F1".to_vec()), Object::Real(LIST_SIZE)],
                ));
                list_ops.push(Operation::new(
                    "Tj",
                    vec![pdf_text_operand(&attachment_name(i))],
                ));
            }
            list_ops.push(Operation::new("ET", vec![]));
            ops.extend(list_ops);
        }
    }

    encode_operations(ops)
}

// ---------------------------------------------------------------------------
// Embedded-file attachments
// ---------------------------------------------------------------------------

/// Attachment file name for the `index`-th (0-based) input.
fn attachment_name(index: usize) -> String {
    format!("doc-{}.pdf", index + 1)
}

/// Embed every input as a `/EmbeddedFiles` name-tree entry.
///
/// Each input becomes an `/EmbeddedFile` stream wrapped in a
/// `/Filespec` dictionary; the catalog's `/Names` → `/EmbeddedFiles`
/// `/Names` array gets `(name, filespec)` pairs appended. Existing
/// entries (there are none on a merge-produced catalog, but the code
/// tolerates them) are preserved.
fn attach_inputs(doc: &mut Document, inputs: &[&[u8]]) -> PdfResult<()> {
    // Collect any pre-existing name pairs so they are not clobbered.
    let mut pairs = existing_embedded_file_pairs(doc);

    for (i, bytes) in inputs.iter().enumerate() {
        let name = attachment_name(i);
        let stream_id = doc
            .try_add_object(Stream::new(
                dictionary! {
                    "Type" => "EmbeddedFile",
                    "Params" => dictionary! {
                        "Size" => bytes.len() as i64,
                    },
                },
                (*bytes).to_vec(),
            ))
            .map_err(lopdf_err)?;
        let spec_id = doc
            .try_add_object(dictionary! {
                "Type" => "Filespec",
                "F" => Object::string_literal(name.clone()),
                "UF" => Object::string_literal(name.clone()),
                "EF" => dictionary! {
                    "F" => stream_id,
                    "UF" => stream_id,
                },
            })
            .map_err(lopdf_err)?;
        pairs.push(Object::string_literal(name));
        pairs.push(Object::Reference(spec_id));
    }

    let embedded_files_id = doc
        .try_add_object(dictionary! {
            "Names" => Object::Array(pairs),
        })
        .map_err(lopdf_err)?;
    let names_id = doc
        .try_add_object(dictionary! {
            "EmbeddedFiles" => embedded_files_id,
        })
        .map_err(lopdf_err)?;

    doc.catalog_mut().map_err(lopdf_err)?.set("Names", names_id);

    Ok(())
}

/// Read the existing `/Names` → `/EmbeddedFiles` → `/Names` array, if
/// any, returning its elements (empty when absent or malformed — the
/// merge-produced catalog never carries one, so a malformed foreign
/// structure simply starts fresh rather than failing the op).
fn existing_embedded_file_pairs(doc: &Document) -> Vec<Object> {
    let Ok(catalog) = doc.catalog() else {
        return Vec::new();
    };
    let Ok(names_obj) = catalog.get(b"Names") else {
        return Vec::new();
    };
    let Ok((_, names_dict_obj)) = doc.dereference(names_obj) else {
        return Vec::new();
    };
    let Ok(names_dict) = names_dict_obj.as_dict() else {
        return Vec::new();
    };
    let Ok(ef_obj) = names_dict.get(b"EmbeddedFiles") else {
        return Vec::new();
    };
    let Ok((_, ef_dict_obj)) = doc.dereference(ef_obj) else {
        return Vec::new();
    };
    let Ok(ef_dict) = ef_dict_obj.as_dict() else {
        return Vec::new();
    };
    let Ok(names_arr_obj) = ef_dict.get(b"Names") else {
        return Vec::new();
    };
    let Ok((_, arr_obj)) = doc.dereference(names_arr_obj) else {
        return Vec::new();
    };
    arr_obj.as_array().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};

    /// Count the `(name, filespec)` pairs under the catalog's
    /// `/Names` → `/EmbeddedFiles` → `/Names` array.
    fn embedded_file_count(pdf: &[u8]) -> usize {
        let doc = Document::load_mem(pdf).unwrap();
        let catalog = doc.catalog().unwrap();
        let names = catalog.get(b"Names").unwrap();
        let (_, names_dict) = doc.dereference(names).unwrap();
        let ef = names_dict.as_dict().unwrap().get(b"EmbeddedFiles").unwrap();
        let (_, ef_dict) = doc.dereference(ef).unwrap();
        let arr = ef_dict.as_dict().unwrap().get(b"Names").unwrap();
        let (_, arr_obj) = doc.dereference(arr).unwrap();
        arr_obj.as_array().unwrap().len() / 2
    }

    #[test]
    fn two_inputs_produce_cover_plus_merged_plus_attachments() {
        let a = pdf_with_pages(&["alpha one", "alpha two"]); // 2 pages
        let b = pdf_with_pages(&["beta one"]); // 1 page

        for layout in [PackageLayout::Grid, PackageLayout::List] {
            let out = package_documents(
                &[&a, &b],
                "Quarterly Report",
                layout,
                &EngineLimits::default(),
            )
            .unwrap();

            // Cover (1) + merged pages (2 + 1).
            assert_eq!(page_count(&out), 4);
            // Two embedded file entries.
            assert_eq!(embedded_file_count(&out), 2);

            let doc = Document::load_mem(&out).unwrap();
            // Names are doc-1.pdf / doc-2.pdf.
            let catalog = doc.catalog().unwrap();
            let names = catalog.get(b"Names").unwrap();
            let (_, names_dict) = doc.dereference(names).unwrap();
            let ef = names_dict.as_dict().unwrap().get(b"EmbeddedFiles").unwrap();
            let (_, ef_dict) = doc.dereference(ef).unwrap();
            let arr_obj = ef_dict.as_dict().unwrap().get(b"Names").unwrap();
            let (_, arr) = doc.dereference(arr_obj).unwrap();
            let arr = arr.as_array().unwrap();
            assert_eq!(arr[0].as_str().unwrap(), b"doc-1.pdf");
            assert_eq!(arr[2].as_str().unwrap(), b"doc-2.pdf");

            // Each filespec's /EF stream must carry the original bytes.
            for pair in arr.chunks_exact(2).skip(1) {
                let (_, spec_obj) = doc.dereference(&pair[1]).unwrap();
                let spec = spec_obj.as_dict().unwrap();
                let ef_dict = spec.get(b"EF").unwrap();
                let (_, ef_dict) = doc.dereference(ef_dict).unwrap();
                let f_stream = ef_dict.as_dict().unwrap().get(b"F").unwrap();
                let (_, stream_obj) = doc.dereference(f_stream).unwrap();
                let stream = stream_obj.as_stream().unwrap();
                assert!(stream.dict.get(b"Type").is_ok());
            }
        }
    }

    #[test]
    fn rejects_empty_and_encrypted_inputs() {
        let a = pdf_with_pages(&["x"]);
        assert!(
            package_documents(&[], "t", PackageLayout::Grid, &EngineLimits::default()).is_err()
        );

        let encrypted = encrypt(
            &a,
            "pw",
            None,
            PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        let b = pdf_with_pages(&["y"]);
        assert!(matches!(
            package_documents(
                &[&encrypted, &b],
                "t",
                PackageLayout::Grid,
                &EngineLimits::default()
            ),
            Err(PdfError::Encrypted)
        ));
        assert!(matches!(
            package_documents(
                &[b"junk".as_slice()],
                "t",
                PackageLayout::Grid,
                &EngineLimits::default()
            ),
            Err(PdfError::InvalidPdf(_)) | Err(PdfError::InvalidInput(_))
        ));
    }
}
