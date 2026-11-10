//! Reduce the size of a PDF by recompressing what can be recompressed.
//!
//! `compress` does two honest things:
//!
//! 1. **Stream recompression** — [`Document::compress`] walks every stream
//!    object and, for streams that carry no `/Filter` yet, applies
//!    `FlateDecode` at level 9 *only when the compressed form is actually
//!    smaller* (`lopdf` skips streams where compression would not pay).
//!    Streams that are already filtered (`/FlateDecode`, `/DCTDecode`,
//!    `/JPXDecode`, ...) are **left untouched**: re-encoding an already
//!    compressed image or content stream is lossy or pointless, and
//!    `lopdf` does not attempt it.
//! 2. **Object-stream packing** — the document is saved with
//!    [`lopdf::SaveOptions`] enabling object streams and cross-reference
//!    streams, so the many small non-stream objects (dictionaries,
//!    numbers, names) are packed into compressed object streams instead
//!    of being written as individual indirect objects.
//!
//! What this does **not** do: it does not recompress image or other
//! already-filtered streams, does not downsample or re-encode images,
//! and does not strip metadata or unused objects. A document that is
//! already fully compressed may come out only marginally smaller (or
//! even slightly larger on tiny inputs, due to object-stream overhead);
//! the operation never claims a size it cannot deliver.

use lopdf::SaveOptions;

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted};

/// Compress `input` and return the smaller (or equal-effort) rewrite.
///
/// The input is loaded with the usual [`EngineLimits`] enforcement and
/// encrypted documents are rejected. See the module docs for exactly
/// which compression steps are applied.
pub fn compress(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    // Level-9 FlateDecode on unfiltered streams, applied only where it
    // reduces size (lopdf's Stream::compress checks `compressed + 19 <
    // content.len()`). Already-filtered streams are skipped by lopdf.
    doc.compress();

    let options = SaveOptions::builder()
        .use_object_streams(true)
        .use_xref_streams(true)
        .compression_level(9)
        .build();

    let mut buf = Vec::new();
    doc.save_with_options(&mut buf, options)
        .map_err(PdfError::Encode)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};

    #[test]
    fn compress_produces_loadable_pdf_with_same_pages() {
        let pdf = pdf_with_pages(&["alpha", "beta", "gamma", "delta"]);
        let out = compress(&pdf, &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&out), 4);
    }

    #[test]
    fn compress_does_not_grow_absurdly() {
        // A document with many small objects benefits from object
        // streams; the output must at least remain sane. We assert a
        // generous bound rather than exact size equality.
        let texts: Vec<String> = (0..20).map(|i| format!("page {i}")).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let pdf = pdf_with_pages(&refs);
        let out = compress(&pdf, &EngineLimits::default()).unwrap();
        assert!(
            out.len() <= pdf.len() + 4096,
            "{} -> {}",
            pdf.len(),
            out.len()
        );
        assert_eq!(page_count(&out), 20);
    }

    #[test]
    fn compress_rejects_garbage() {
        assert!(compress(b"this is not a pdf", &EngineLimits::default()).is_err());
    }
}
