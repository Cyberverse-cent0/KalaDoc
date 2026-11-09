//! Read and write the document catalog's `/Metadata` stream — the XMP
//! packet viewers and indexers use for document metadata.
//!
//! In PDF, XMP metadata is simply a stream object referenced from the
//! catalog's `/Metadata` entry, conventionally with
//! `/Type /Metadata` and `/Subtype /XML` in its dictionary. This op
//! implements the two primitives:
//!
//! - [`set_xmp`] validates the packet loosely (see below), then either
//!   **updates the existing `/Metadata` stream in place** (same object
//!   id, so other references keep working) or creates a new stream
//!   object and points the catalog at it. The stream content is stored
//!   uncompressed — `lopdf` applies whatever filters the writer
//!   chooses at save time.
//! - [`get_xmp`] returns the stream's **raw decompressed bytes**
//!   (bounded by [`MAX_XMP_BYTES`]); it does not interpret XML.
//!
//! Input validation for [`set_xmp`] is deliberately loose — a real XML
//! parser is out of scope — but the packet must plausibly be XMP: after
//! leading ASCII whitespace (and an optional UTF-8 BOM) it must start
//! with `<?xpacket` or `<x:xmpmeta`. Packets starting with a bare
//! `<?xml` declaration followed by other content are rejected; wrap
//! them in an `x:xmpmeta` element first. Non-XML bytes are rejected
//! with [`PdfError::InvalidInput`].
//!
//! An empty `/Names`-style absence is reported by [`get_xmp`] as
//! [`PdfError::InvalidInput`] — there is no dedicated "not found"
//! variant. Encrypted input is rejected by both functions.

use lopdf::{Dictionary, Object, Stream};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};

/// Maximum accepted size of a decompressed `/Metadata` stream when
/// reading — matches `extract_metadata`'s XMP cap.
const MAX_XMP_BYTES: usize = 8 * 1024 * 1024;

/// Loose XMP check: optional BOM + leading whitespace, then an XMP
/// processing-instruction or `x:xmpmeta` root start tag.
fn looks_like_xmp(packet: &[u8]) -> bool {
    let mut rest = packet;
    // UTF-8 BOM.
    if rest.starts_with(&[0xEF, 0xBB, 0xBF]) {
        rest = &rest[3..];
    }
    while let [first, tail @ ..] = rest {
        if first.is_ascii_whitespace() {
            rest = tail;
        } else {
            break;
        }
    }
    rest.starts_with(b"<?xpacket") || rest.starts_with(b"<x:xmpmeta")
}

/// Resolve the catalog dictionary (trailer `/Root`).
fn catalog_dict<'a>(doc: &'a lopdf::Document) -> PdfResult<&'a Dictionary> {
    let root = doc
        .trailer
        .get(b"Root")
        .and_then(Object::as_reference)
        .map_err(|_| PdfError::InvalidInput("PDF has no catalog (/Root)".to_string()))?;
    doc.get_dictionary(root).map_err(lopdf_err)
}

/// Write `xmp_packet` as the catalog `/Metadata` stream.
///
/// If the catalog already references a `/Metadata` stream indirectly,
/// that object is replaced in place (preserving its id); otherwise a
/// new stream object is created. An existing stream dictionary's extra
/// entries are preserved where possible; `/Type /Metadata` and
/// `/Subtype /XML` are always (re)set.
///
/// The packet must plausibly be XMP — see the module docs for the
/// accepted prefixes. Returns the serialized PDF.
pub fn set_xmp(input: &[u8], xmp_packet: &[u8], limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    if xmp_packet.is_empty() {
        return Err(PdfError::InvalidInput("XMP packet is empty".to_string()));
    }
    if !looks_like_xmp(xmp_packet) {
        return Err(PdfError::InvalidInput(
            "XMP packet must start with `<?xpacket` or `<x:xmpmeta`".to_string(),
        ));
    }

    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let root_id = doc
        .trailer
        .get(b"Root")
        .and_then(Object::as_reference)
        .map_err(|_| PdfError::InvalidInput("PDF has no catalog (/Root)".to_string()))?;

    // Preserve the existing stream dict's extra keys when the metadata
    // entry resolves to a stream (indirect or — pathologically — direct).
    let mut stream_dict = Dictionary::new();
    if let Ok(existing) = catalog_dict(&doc)?.get(b"Metadata") {
        if let Ok((_, obj)) = doc.dereference(existing)
            && let Ok(stream) = obj.as_stream()
        {
            stream_dict = stream.dict.clone();
        }
    }
    stream_dict.set("Type", Object::Name(b"Metadata".to_vec()));
    stream_dict.set("Subtype", Object::Name(b"XML".to_vec()));
    let new_stream = Object::Stream(Stream::new(stream_dict, xmp_packet.to_vec()));

    // Clone the /Metadata entry so the immutable catalog borrow ends
    // before the mutable object/doc writes below.
    let metadata_entry = catalog_dict(&doc)?.get(b"Metadata").ok().cloned();
    match metadata_entry {
        // Indirect reference: replace the object in place so any other
        // references to it see the new packet too.
        Some(Object::Reference(id)) => {
            *doc.get_object_mut(id).map_err(lopdf_err)? = new_stream;
        }
        // Anything else (missing, direct, non-stream): fresh object.
        _ => {
            let id = doc.try_add_object(new_stream).map_err(lopdf_err)?;
            doc.get_dictionary_mut(root_id)
                .map_err(lopdf_err)?
                .set("Metadata", Object::Reference(id));
        }
    }

    save_pdf(&mut doc)
}

/// Read the catalog `/Metadata` stream and return its decompressed raw
/// bytes (at most [`MAX_XMP_BYTES`]).
///
/// Fails with [`PdfError::InvalidInput`] when the document has no
/// `/Metadata` entry or it does not resolve to a stream.
pub fn get_xmp(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let metadata = catalog_dict(&doc)?
        .get(b"Metadata")
        .map_err(|_| PdfError::InvalidInput("document has no /Metadata stream".to_string()))?;
    let (_, object) = doc.dereference(metadata).map_err(lopdf_err)?;
    let stream = object
        .as_stream()
        .map_err(|_| PdfError::InvalidInput("catalog /Metadata is not a stream".to_string()))?;
    stream
        .decompressed_content_with_limit(MAX_XMP_BYTES)
        .map_err(lopdf_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::{Document, Object};

    const PACKET: &[u8] = b"<?xpacket begin=\"\xef\xbb\xbf\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF/></x:xmpmeta>";

    fn catalog_metadata(pdf: &[u8]) -> Option<Object> {
        let doc = Document::load_mem(pdf).unwrap();
        let root = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        doc.get_dictionary(root)
            .unwrap()
            .get(b"Metadata")
            .ok()
            .cloned()
    }

    #[test]
    fn set_then_get_roundtrip() {
        let pdf = pdf_with_pages(&["doc"]);
        // No /Metadata initially.
        assert!(catalog_metadata(&pdf).is_none());

        let out = set_xmp(&pdf, PACKET, &EngineLimits::default()).unwrap();
        assert_eq!(get_xmp(&out, &EngineLimits::default()).unwrap(), PACKET);

        // Catalog points at a stream with the right dict keys.
        let doc = Document::load_mem(&out).unwrap();
        let root = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        let catalog = doc.get_dictionary(root).unwrap();
        let meta = catalog.get(b"Metadata").unwrap().as_reference().unwrap();
        let stream = doc.get_object(meta).unwrap().as_stream().unwrap();
        assert_eq!(
            stream.dict.get(b"Type").unwrap().as_name().unwrap(),
            b"Metadata"
        );
        assert_eq!(
            stream.dict.get(b"Subtype").unwrap().as_name().unwrap(),
            b"XML"
        );
    }

    #[test]
    fn updates_existing_metadata_in_place() {
        let pdf = pdf_with_pages(&["doc"]);
        let once = set_xmp(&pdf, PACKET, &EngineLimits::default()).unwrap();
        let old_id = catalog_metadata(&once).unwrap().as_reference().unwrap();

        let packet2 = b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF><rdf:Description/></rdf:RDF></x:xmpmeta>";
        let twice = set_xmp(&once, packet2, &EngineLimits::default()).unwrap();
        let new_id = catalog_metadata(&twice).unwrap().as_reference().unwrap();
        assert_eq!(old_id, new_id, "existing metadata object reused");
        assert_eq!(get_xmp(&twice, &EngineLimits::default()).unwrap(), packet2);
    }

    #[test]
    fn rejects_non_xmp_packets() {
        let pdf = pdf_with_pages(&["doc"]);
        for bad in [
            &b""[..],
            b"plain text",
            b"<?xml version=\"1.0\"?><notxmp/>",
            b"\x00\x01\x02binary",
        ] {
            assert!(
                set_xmp(&pdf, bad, &EngineLimits::default()).is_err(),
                "packet {bad:?} should fail"
            );
        }
        // Leading whitespace / BOM tolerated.
        let padded = b"  \n\t<?xpacket begin=\"x\"?><x:xmpmeta/>";
        assert!(set_xmp(&pdf, padded, &EngineLimits::default()).is_ok());
    }

    #[test]
    fn get_xmp_errors_without_metadata() {
        let pdf = pdf_with_pages(&["doc"]);
        assert!(matches!(
            get_xmp(&pdf, &EngineLimits::default()),
            Err(PdfError::InvalidInput(_))
        ));
        assert!(matches!(
            get_xmp(b"junk", &EngineLimits::default()),
            Err(PdfError::InvalidPdf(_))
        ));
    }

    #[test]
    fn encrypted_input_rejected() {
        let pdf = pdf_with_pages(&["doc"]);
        let encrypted = crate::crates::pdf_engine::ops::encrypt::encrypt(
            &pdf,
            "pw",
            None,
            crate::crates::pdf_engine::ops::encrypt::PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        assert!(matches!(
            set_xmp(&encrypted, PACKET, &EngineLimits::default()),
            Err(PdfError::Encrypted)
        ));
        assert!(matches!(
            get_xmp(&encrypted, &EngineLimits::default()),
            Err(PdfError::Encrypted)
        ));
    }
}
