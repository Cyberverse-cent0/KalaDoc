//! PDF portfolios — a *navigational* collection of embedded pages.
//!
//! This is **not** a file-attachment portfolio: no embedded file
//! streams, no `/EmbeddedFiles` name tree, no launch actions. Instead,
//! [`create_portfolio`] merges the input documents' pages into a single
//! PDF (reusing [`super::merge`]'s object-copy machinery) and adds a
//! `/Collection` dictionary to the catalog so conforming viewers can
//! present the merged pages as named "documents" in a portfolio panel.
//! Each `/Names` entry maps a display name to the *page object* of that
//! input's first page; viewers that understand collections offer
//! navigation between them. Viewers that do not simply show a normal
//! merged PDF — the pages are fully embedded and readable either way.
//!
//! [`list_portfolio`] reads the `/Collection` `/Folder` `/Names` array
//! back into [`PortfolioEntry`] records. Only the flat `/Names` form is
//! supported; nested folder trees (`/Kids`) are not walked.

use std::collections::HashMap;

use lopdf::{Document, Object, ObjectId, dictionary};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};

/// One entry in a portfolio's folder name tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortfolioEntry {
    /// Display name from the `/Names` key string.
    pub name: String,
    /// 1-based page number of the entry's target page within the
    /// document, or `0` when the target does not resolve to a page
    /// (e.g. a file-specification entry in a foreign portfolio).
    pub page: u32,
}

/// Merge `inputs` into one PDF and mark it as a navigational portfolio.
///
/// `names` optionally supplies one display name per input, in order;
/// when empty, entries are named `"Document 1"`, `"Document 2"`, ....
/// A non-empty `names` must have exactly `inputs.len()` entries.
///
/// Each input's first page becomes a `/Names` entry so viewers can jump
/// to it; `/D` points at the first input's first page and `/View` is
/// `"H"`. Returns [`PdfError::Encrypted`] if any input is encrypted.
pub fn create_portfolio(
    inputs: &[&[u8]],
    names: &[String],
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    if inputs.is_empty() {
        return Err(PdfError::InvalidInput(
            "create_portfolio requires at least one input document".to_string(),
        ));
    }
    if !names.is_empty() && names.len() != inputs.len() {
        return Err(PdfError::InvalidInput(format!(
            "names has {} entries but there are {} inputs",
            names.len(),
            inputs.len()
        )));
    }

    // merge() performs the object-copy/remap work and enforces all
    // input-side limits (size, per-doc and combined page counts,
    // encryption). We then reopen the merged bytes to attach the
    // collection dictionary.
    let merged = super::merge::merge(inputs, limits)?;
    let mut doc = load_pdf(&merged, limits)?;

    // Boundaries between inputs inside the flat merged page list. The
    // merge output orders pages input-by-input, so the first page of
    // input i sits at the cumulative offset of all previous inputs'
    // page counts. Page counts are re-derived from the inputs (merge
    // does not expose them); the extra parse is cheap relative to the
    // merge itself and keeps this op honest about which pages belong
    // to which named entry.
    let mut first_page_index = Vec::with_capacity(inputs.len());
    let mut cumulative = 0usize;
    for bytes in inputs {
        first_page_index.push(cumulative);
        cumulative += load_pdf(bytes, limits)?.get_pages().len();
    }

    let page_ids: Vec<ObjectId> = doc.get_pages().into_values().collect();

    let mut pairs = Vec::with_capacity(inputs.len() * 2);
    for (i, &page_idx) in first_page_index.iter().enumerate() {
        let Some(&page_id) = page_ids.get(page_idx) else {
            return Err(PdfError::InvalidInput(format!(
                "input {i} contributed no pages to the merged document"
            )));
        };
        let name = names
            .get(i)
            .filter(|n| !n.is_empty())
            .cloned()
            .unwrap_or_else(|| format!("Document {}", i + 1));
        pairs.push(Object::string_literal(name));
        pairs.push(Object::Reference(page_id));
    }

    let names_id = doc
        .try_add_object(dictionary! {
            "Names" => Object::Array(pairs),
        })
        .map_err(lopdf_err)?;

    let first_page_id = page_ids
        .first()
        .copied()
        .ok_or_else(|| PdfError::InvalidInput("merged document has no pages".to_string()))?;

    let collection_id = doc
        .try_add_object(dictionary! {
            "Type" => "Collection",
            "View" => "H",
            "D" => Object::Reference(first_page_id),
            "Folder" => names_id,
        })
        .map_err(lopdf_err)?;

    doc.catalog_mut()
        .map_err(lopdf_err)?
        .set("Collection", collection_id);

    save_pdf(&mut doc)
}

/// List the entries of a portfolio's collection dictionary.
///
/// Returns an empty vector when the document has no `/Collection`.
/// Entries whose `/Names` value is not a page reference (for example a
/// file specification in an attachment-style portfolio this engine did
/// not create) are reported with `page == 0`.
pub fn list_portfolio(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<PortfolioEntry>> {
    let doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let catalog = match doc.catalog() {
        Ok(catalog) => catalog,
        Err(_) => return Ok(Vec::new()),
    };

    let collection = match catalog.get(b"Collection") {
        Ok(obj) => obj,
        Err(_) => return Ok(Vec::new()),
    };
    let (_, collection) = doc.dereference(collection).map_err(lopdf_err)?;
    let collection = collection.as_dict().map_err(lopdf_err)?;

    let folder = match collection.get(b"Folder") {
        Ok(obj) => obj,
        Err(_) => return Ok(Vec::new()),
    };
    let (_, folder) = doc.dereference(folder).map_err(lopdf_err)?;
    let folder = folder.as_dict().map_err(lopdf_err)?;

    let names = match folder.get(b"Names") {
        Ok(obj) => obj,
        Err(_) => return Ok(Vec::new()),
    };
    let (_, names) = doc.dereference(names).map_err(lopdf_err)?;
    let names = names
        .as_array()
        .map_err(|_| PdfError::InvalidInput("portfolio /Names is not an array".to_string()))?;
    if names.len() % 2 != 0 {
        return Err(PdfError::InvalidInput(
            "portfolio /Names array has an odd number of elements".to_string(),
        ));
    }

    // page object id -> 1-based page number, for resolving targets.
    let page_numbers: HashMap<ObjectId, u32> = doc
        .get_pages()
        .into_iter()
        .map(|(num, id)| (id, num))
        .collect();

    let mut entries = Vec::with_capacity(names.len() / 2);
    for pair in names.chunks_exact(2) {
        let name = pair[0]
            .as_str()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_default();
        entries.push(PortfolioEntry {
            name,
            page: resolve_page_number(&doc, &pair[1], &page_numbers),
        });
    }
    Ok(entries)
}

/// Best-effort resolution of a `/Names` value to a 1-based page number.
///
/// Handles the forms this engine writes (`Object::Reference` to a page
/// object) plus common foreign forms: an indirect destination array
/// whose first element is a page reference, and a destination
/// dictionary carrying `/D`. Anything else yields `0`.
fn resolve_page_number(
    doc: &Document,
    value: &Object,
    page_numbers: &HashMap<ObjectId, u32>,
) -> u32 {
    // A direct reference to a page object is the form this engine
    // writes — check it *before* dereferencing, which would resolve
    // the reference into the page dictionary itself.
    if let Ok(id) = value.as_reference()
        && let Some(&n) = page_numbers.get(&id)
    {
        return n;
    }
    let (_, target) = match doc.dereference(value) {
        Ok(pair) => pair,
        Err(_) => return 0,
    };
    match target {
        Object::Reference(id) => page_numbers.get(id).copied().unwrap_or(0),
        Object::Array(items) => match items.first() {
            Some(Object::Reference(id)) => page_numbers.get(id).copied().unwrap_or(0),
            _ => 0,
        },
        Object::Dictionary(dict) => dict
            .get(b"D")
            .ok()
            .and_then(|d| d.as_array().ok())
            .and_then(|arr| arr.first())
            .and_then(|first| first.as_reference().ok())
            .and_then(|id| page_numbers.get(&id).copied())
            .unwrap_or(0),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};

    #[test]
    fn create_then_list_roundtrip() {
        let a = pdf_with_pages(&["alpha one", "alpha two"]);
        let b = pdf_with_pages(&["beta one"]);
        let names = vec!["alpha.pdf".to_string(), "beta.pdf".to_string()];

        let out = create_portfolio(&[&a, &b], &names, &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 3);

        // The output must carry a /Collection dictionary in its catalog.
        let doc = Document::load_mem(&out).unwrap();
        let catalog = doc.catalog().unwrap();
        assert!(catalog.get(b"Collection").is_ok());

        let entries = list_portfolio(&out, &EngineLimits::default()).unwrap();
        assert_eq!(
            entries,
            vec![
                PortfolioEntry {
                    name: "alpha.pdf".into(),
                    page: 1,
                },
                PortfolioEntry {
                    name: "beta.pdf".into(),
                    page: 3,
                },
            ]
        );
    }

    #[test]
    fn auto_names_when_names_empty() {
        let a = pdf_with_pages(&["x"]);
        let b = pdf_with_pages(&["y"]);
        let out = create_portfolio(&[&a, &b], &[], &EngineLimits::default()).unwrap();
        let entries = list_portfolio(&out, &EngineLimits::default()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "Document 1");
        assert_eq!(entries[1].name, "Document 2");
        assert_eq!(entries[0].page, 1);
        assert_eq!(entries[1].page, 2);
    }

    #[test]
    fn list_plain_pdf_returns_empty() {
        let pdf = pdf_with_pages(&["plain"]);
        assert!(
            list_portfolio(&pdf, &EngineLimits::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rejects_bad_inputs() {
        let a = pdf_with_pages(&["x"]);
        assert!(create_portfolio(&[], &[], &EngineLimits::default()).is_err());
        assert!(create_portfolio(&[b"junk".as_slice()], &[], &EngineLimits::default()).is_err());
        assert!(
            create_portfolio(
                &[&a],
                &["one".to_string(), "two".to_string()],
                &EngineLimits::default()
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_encrypted_input() {
        let plain = pdf_with_pages(&["secret"]);
        let encrypted = encrypt(
            &plain,
            "pw",
            None,
            PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        let other = pdf_with_pages(&["open"]);
        assert!(create_portfolio(&[&encrypted, &other], &[], &EngineLimits::default()).is_err());
        assert!(list_portfolio(&encrypted, &EngineLimits::default()).is_err());
    }
}
