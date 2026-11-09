//! Split a PDF into multiple files of at most `pages_per_file` pages.
//!
//! The last chunk may be smaller. Page order is preserved and each
//! output is an independent, valid PDF. The number of produced files is
//! checked against `max_outputs` *before* any splitting work happens.

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted, save_pdf};

/// Split `input` into chunks of `pages_per_file` pages each.
///
/// Returns one `Vec<u8>` per chunk, in page order. Fails with
/// [`PdfError::InvalidInput`] when `pages_per_file` is 0 or the document
/// has no pages, and [`PdfError::LimitExceeded`] when the chunk count
/// would exceed `max_outputs`.
pub fn split(input: &[u8], pages_per_file: u32, limits: &EngineLimits) -> PdfResult<Vec<Vec<u8>>> {
    if pages_per_file == 0 {
        return Err(PdfError::InvalidInput(
            "pages_per_file must be greater than 0".to_string(),
        ));
    }

    let doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    if page_count == 0 {
        return Err(PdfError::InvalidInput(
            "document has no pages to split".to_string(),
        ));
    }

    let chunk_count = page_count.div_ceil(pages_per_file);
    if let Some(max) = limits.max_outputs
        && chunk_count > max
    {
        return Err(PdfError::LimitExceeded(format!(
            "split would produce {chunk_count} files, exceeding max_outputs {max}"
        )));
    }

    let mut outputs = Vec::with_capacity(chunk_count as usize);
    let mut start = 1u32;
    while start <= page_count {
        let end = start.saturating_add(pages_per_file - 1).min(page_count);

        let mut chunk = doc.clone();
        let delete: Vec<u32> = (1..=page_count)
            .filter(|p| *p < start || *p > end)
            .collect();
        chunk.delete_pages(&delete);
        outputs.push(save_pdf(&mut chunk)?);

        start = end + 1;
    }
    Ok(outputs)
}
