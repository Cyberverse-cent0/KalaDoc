//! Extract a subset of pages into a new PDF.
//!
//! Pages are selected with the shared 1-based range spec
//! ([`parse_page_range`]); the spec is validated against the document's
//! page count before any page is removed.

use std::collections::BTreeSet;

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::{load_pdf, reject_encrypted, save_pdf};
use super::pages::parse_page_range;

/// Write a new PDF containing only the pages selected by `pages`
/// (e.g. `"1-3,8,10-12"` or `"all"`), preserving their original order.
pub fn extract_pages(input: &[u8], pages: &str, limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let keep: BTreeSet<u32> = parse_page_range(pages, page_count)?.into_iter().collect();

    let delete: Vec<u32> = (1..=page_count).filter(|p| !keep.contains(p)).collect();
    doc.delete_pages(&delete);

    save_pdf(&mut doc)
}
