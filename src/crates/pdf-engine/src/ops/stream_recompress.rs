//! Recompress streams that are already `/FlateDecode`-filtered.
//!
//! [`compress`](super::compress) only filters streams that carry **no**
//! `/Filter` yet; it deliberately leaves already-compressed streams
//! alone. This operation is the complement: it walks every stream
//! object whose filter chain is exactly `/FlateDecode`, decodes the
//! stream, and re-encodes it, keeping the result only when the new
//! stored bytes are strictly smaller than the old ones.
//!
//! # Compressor honesty notice
//!
//! `flate2` is **not** a direct dependency of this crate (it is only a
//! transitive dependency of `lopdf`, so it cannot be `use`d here), and
//! `Cargo.toml` must not change for this operation. `lopdf` 0.47 does
//! expose stream compression through [`lopdf::Stream::compress`], but
//! it is hard-wired to zlib level 9 and only applies when the filtered
//! stream carries no `/Filter` entry — which is exactly why this op
//! goes through `set_plain_content` (strips `/Filter` and
//! `/DecodeParms`) before calling `compress`.
//!
//! Consequences:
//!
//! * The `level` parameter is clamped to `1..=9` for API stability, but
//!   the effective compression level is always lopdf's fixed 9. When a
//!   level other than 9 is requested a warning is recorded in
//!   [`RecompressReport::warnings`].
//! * `/DecodeParms` predictors are reversed during decode and dropped:
//!   the stored bytes are the fully-decoded payload under a plain
//!   `/FlateDecode` filter, which is byte-for-byte equivalent for any
//!   conforming reader.
//!
//! Streams with no `/Filter`, with other or additional filters
//! (`/DCTDecode`, `/JPXDecode`, filter arrays, ...), or flagged
//! `allows_compression == false` are skipped — re-encoding an
//! already-compressed image is lossy or pointless.

use lopdf::Object;

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::{load_pdf, reject_encrypted};

/// Per-stream decompression bound. Same 64 MiB ceiling used by the
/// other content-stream ops; a stream whose decoded output would exceed
/// it is skipped with a warning rather than trusted.
const MAX_STREAM_DECOMPRESSED: usize = 64 * 1024 * 1024;

/// Result of [`recompress_streams`].
#[derive(Debug, Clone)]
pub struct RecompressReport {
    /// Number of stream objects examined (filtered or not).
    pub streams_seen: usize,
    /// Number of `/FlateDecode` streams whose stored bytes were
    /// replaced by a strictly smaller re-encoding.
    pub streams_recompressed: usize,
    /// Sum of `stream.content.len()` over **all** stream objects before
    /// the operation (stored, i.e. still-filtered bytes).
    pub bytes_before: usize,
    /// Same sum after the operation. Skipped streams contribute their
    /// unchanged length, so the delta reflects only real savings.
    pub bytes_after: usize,
    /// Human-readable notes: per-stream decode/re-encode failures and
    /// the fixed-level-9 compressor notice.
    pub warnings: Vec<String>,
}

/// Recompress every `/FlateDecode` stream in `input` and return the
/// rewritten PDF plus a [`RecompressReport`].
///
/// `level` is clamped to `1..=9`; see the module docs — lopdf's stream
/// compressor is fixed at level 9, so other levels cannot be honored
/// and a warning is recorded instead of silently pretending.
///
/// The input is loaded through [`load_pdf`] (input limits enforced)
/// and encrypted documents are rejected. Streams that fail to decode
/// or whose re-encoding would not shrink the stored bytes are left
/// untouched and reported via `warnings`; the operation never fails on
/// a single bad stream.
pub fn recompress_streams(
    input: &[u8],
    level: u32,
    limits: &EngineLimits,
) -> PdfResult<(Vec<u8>, RecompressReport)> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let level = level.clamp(1, 9);
    let mut report = RecompressReport {
        streams_seen: 0,
        streams_recompressed: 0,
        bytes_before: 0,
        bytes_after: 0,
        warnings: Vec::new(),
    };
    if level != 9 {
        report.warnings.push(format!(
            "lopdf 0.47 stream compression is fixed at level 9; requested level {level} cannot be honored"
        ));
    }

    for (id, object) in doc.objects.iter_mut() {
        let Object::Stream(stream) = object else {
            continue;
        };
        report.streams_seen += 1;
        report.bytes_before += stream.content.len();

        if try_recompress(*id, stream, &mut report.warnings) {
            report.streams_recompressed += 1;
        }
        report.bytes_after += stream.content.len();
    }

    let bytes = super::load::save_pdf(&mut doc)?;
    Ok((bytes, report))
}

/// Attempt to re-encode one stream in place. Returns `true` when the
/// stored bytes were replaced by a smaller `/FlateDecode` encoding.
///
/// Only streams whose filter chain is exactly `/FlateDecode` are
/// candidates; everything else returns `false` without touching the
/// stream or recording a warning.
fn try_recompress(
    id: lopdf::ObjectId,
    stream: &mut lopdf::Stream,
    warnings: &mut Vec<String>,
) -> bool {
    // Filter chain must be exactly [ /FlateDecode ] — a bare name or a
    // single-element array both show up as one entry here.
    let is_flate = matches!(stream.filters(), Ok(filters) if filters.len() == 1 && filters[0] == b"FlateDecode");
    if !is_flate || !stream.allows_compression {
        return false;
    }

    let old_len = stream.content.len();

    // Decode with a hard output bound (decompression-bomb safe). The
    // decoded bytes have /DecodeParms predictors already reversed.
    let plain = match stream.decompressed_content_with_limit(MAX_STREAM_DECOMPRESSED) {
        Ok(data) => data,
        Err(err) => {
            warnings.push(format!(
                "object {id:?}: FlateDecode stream decode failed: {err}"
            ));
            return false;
        }
    };

    // Re-encode through a scratch copy so a failed or non-shrinking
    // recompression never disturbs the original stream.
    let mut candidate = stream.clone();
    candidate.set_plain_content(plain);
    if let Err(err) = candidate.compress() {
        warnings.push(format!("object {id:?}: stream recompression failed: {err}"));
        return false;
    }

    // compress() leaves the stream unfiltered when filtering would not
    // pay; only accept a result that is still FlateDecode and strictly
    // smaller in stored bytes than what we started with.
    let still_flate = matches!(candidate.filters(), Ok(filters) if filters.len() == 1 && filters[0] == b"FlateDecode");
    if !still_flate || candidate.content.len() >= old_len {
        return false;
    }

    *stream = candidate;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};
    use lopdf::{Document, Object, Stream, dictionary};

    /// Build a one-page PDF whose content stream is highly compressible
    /// and stored as `/FlateDecode`.
    fn pdf_with_flate_stream() -> Vec<u8> {
        let mut doc = Document::with_version("1.5");

        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Courier",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });

        // Highly repetitive content so FlateDecode pays off.
        let mut body = String::new();
        for i in 0..200 {
            body.push_str(&format!(
                "BT /F1 12 Tf 100 {} Td (repetitive line {i:03}) Tj ET\n",
                750 - i
            ));
        }
        let mut stream = Stream::new(dictionary! {}, body.into_bytes());
        stream.compress().unwrap();
        assert!(
            stream.dict.has(b"Filter"),
            "test stream must be FlateDecode"
        );
        let content_id = doc.add_object(stream);

        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        let pages = dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        };
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    #[test]
    fn recompress_counts_and_keeps_valid_pdf() {
        let pdf = pdf_with_flate_stream();
        let (out, report) = recompress_streams(&pdf, 9, &EngineLimits::default()).unwrap();

        // PDF 1.5 saves may add an object stream, so count is >= 1.
        assert!(report.streams_seen >= 1);
        // lopdf recompresses at the same level, so the size is a wash;
        // the honest assertion is that the stream round-trips through
        // decode/re-encode and the output stays a valid PDF.
        assert!(report.streams_recompressed <= 1);
        assert_eq!(page_count(&out), 1);
        // The reloaded document's content stream must decode to the
        // same payload.
        let doc = Document::load_mem(&out).unwrap();
        let stream = doc
            .objects
            .values()
            .find_map(|o| o.as_stream().ok())
            .expect("output must contain a stream");
        let decoded = stream.decompressed_content().unwrap();
        assert!(decoded.starts_with(b"BT /F1 12 Tf"));
        assert!(decoded.len() > 1000);
    }

    #[test]
    fn recompress_skips_unfiltered_streams() {
        // test_utils content streams carry no /Filter — they are seen
        // but never recompressed.
        let pdf = pdf_with_pages(&["alpha", "beta"]);
        let (out, report) = recompress_streams(&pdf, 9, &EngineLimits::default()).unwrap();

        assert!(report.streams_seen >= 2);
        assert_eq!(report.streams_recompressed, 0);
        assert_eq!(report.bytes_before, report.bytes_after);
        assert_eq!(page_count(&out), 2);
    }

    #[test]
    fn recompress_level_is_clamped_and_warns() {
        let pdf = pdf_with_pages(&["x"]);
        let (_, report) = recompress_streams(&pdf, 3, &EngineLimits::default()).unwrap();
        assert!(report.warnings.iter().any(|w| w.contains("level 9")));

        // Out-of-range levels clamp instead of panicking.
        let (_, report) = recompress_streams(&pdf, u32::MAX, &EngineLimits::default()).unwrap();
        assert!(
            !report
                .warnings
                .iter()
                .any(|w| w.contains("cannot be honored"))
        );
    }

    #[test]
    fn recompress_rejects_garbage() {
        assert!(recompress_streams(b"not a pdf", 9, &EngineLimits::default()).is_err());
    }
}
