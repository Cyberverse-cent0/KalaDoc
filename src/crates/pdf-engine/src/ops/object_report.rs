//! Low-level object census: classify every top-level object in the
//! document's object table and profile the streams.
//!
//! [`object_report`] iterates [`lopdf::Document::objects`] — the map of
//! indirect objects — and counts each [`lopdf::Object`] variant once per
//! top-level entry. Nested values inside arrays and dictionaries are
//! *not* counted separately; only the objects reachable through the
//! cross-reference table appear in `doc.objects`.
//!
//! Streams are inspected **without decompressing them**. `raw_len` is
//! the number of bytes `lopdf` stored for the stream (the encoded data
//! as it appears between `stream`/`endstream`), and `filtered_len` is
//! the decoded ("unfiltered") length — which is only knowable without
//! decoding when the stream carries no `/Filter`, so it equals
//! `raw_len` for unfiltered streams and is `0` for filtered ones.
//! `total_stream_bytes` sums `raw_len`; `uncompressed_stream_bytes`
//! sums `filtered_len`, i.e. only the bytes of streams that are known
//! to need no decoding.
//!
//! Encrypted input is tolerated (like [`super::info::inspect`]): the
//! object structure parses normally, but string and stream bytes are
//! ciphertext, so name/string statistics describe encrypted data.

use lopdf::{Document, Object, ObjectId};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::load_pdf;

/// Maximum entries kept in [`ObjectReport::biggest_streams`].
const MAX_BIGGEST_STREAMS: usize = 20;
/// Recursion cap for the catalog reachability walk.
const MAX_CATALOG_DEPTH: usize = 32;
/// Node cap for the catalog reachability walk.
const MAX_CATALOG_NODES: usize = 10_000;

/// Per-variant census of the document's top-level objects.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectStats {
    /// Total indirect objects in the cross-reference table.
    pub total_objects: usize,
    /// `Object::Stream` entries.
    pub streams: usize,
    /// `Object::Dictionary` entries.
    pub dictionaries: usize,
    /// `Object::Array` entries.
    pub arrays: usize,
    /// `Object::Name` entries.
    pub names: usize,
    /// `Object::String` entries (literal and hexadecimal).
    pub strings: usize,
    /// `Object::Integer` entries.
    pub integers: usize,
    /// `Object::Real` entries.
    pub reals: usize,
    /// `Object::Boolean` entries.
    pub booleans: usize,
    /// `Object::Null` entries.
    pub nulls: usize,
    /// `Object::Reference` entries.
    pub references: usize,
}

/// Summary of one stream object, largest-first in
/// [`ObjectReport::biggest_streams`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamInfo {
    /// Human-readable hint derived from the stream dictionary's
    /// `/Subtype` (preferred) or `/Type` name, e.g. `"/Image"`;
    /// `"stream"` when neither is present.
    pub name_hint: String,
    /// The stream dictionary's keys, each rendered as `"/Key"`.
    pub dict_keys: Vec<String>,
    /// Bytes stored for the stream content (`stream.content.len()`) —
    /// the encoded length as it appears in the file.
    pub raw_len: usize,
    /// Decoded ("unfiltered") byte length. Equals `raw_len` when the
    /// stream has no `/Filter`; `0` when filters are present because
    /// this report never decompresses stream data.
    pub filtered_len: usize,
    /// `/Filter` names in decoding order (a single name or each
    /// element of the filter array), lossy-decoded.
    pub filters: Vec<String>,
}

/// Object-level census of a PDF document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectReport {
    /// Variant counts over `doc.objects`.
    pub stats: ObjectStats,
    /// Sum of `StreamInfo::raw_len` over all streams.
    pub total_stream_bytes: usize,
    /// Sum of `StreamInfo::filtered_len` over all streams — only the
    /// bytes of streams known to carry no filters.
    pub uncompressed_stream_bytes: usize,
    /// Up to [`MAX_BIGGEST_STREAMS`] streams sorted by `raw_len`
    /// descending.
    pub biggest_streams: Vec<StreamInfo>,
    /// Number of page objects resolved via the document's page tree.
    pub page_objects: usize,
    /// Number of distinct indirect objects reachable from the
    /// trailer `/Root` catalog by a bounded walk (includes the
    /// catalog itself).
    pub catalog_objects: usize,
    /// Non-fatal observations: `/Length` mismatches, missing catalog,
    /// truncated catalog walk, and so on.
    pub warnings: Vec<String>,
}

/// Census `input`'s indirect objects and streams.
///
/// Malformed input fails through [`load_pdf`] as
/// [`PdfError::InvalidPdf`](crate::crates::pdf_engine::error::PdfError);
/// the report itself never fails on unusual-but-parseable documents —
/// oddities land in [`ObjectReport::warnings`] instead.
pub fn object_report(input: &[u8], limits: &EngineLimits) -> PdfResult<ObjectReport> {
    let doc = load_pdf(input, limits)?;

    let mut stats = ObjectStats::default();
    let mut streams: Vec<StreamInfo> = Vec::new();
    let mut total_stream_bytes = 0usize;
    let mut uncompressed_stream_bytes = 0usize;
    let mut warnings: Vec<String> = Vec::new();

    // `doc.objects` is a BTreeMap<ObjectId, Object>; iterate in a
    // deterministic id order.
    for (id, object) in &doc.objects {
        stats.total_objects += 1;
        match object {
            Object::Null => stats.nulls += 1,
            Object::Boolean(_) => stats.booleans += 1,
            Object::Integer(_) => stats.integers += 1,
            Object::Real(_) => stats.reals += 1,
            Object::Name(_) => stats.names += 1,
            Object::String(..) => stats.strings += 1,
            Object::Array(_) => stats.arrays += 1,
            Object::Dictionary(_) => stats.dictionaries += 1,
            Object::Reference(_) => stats.references += 1,
            Object::Stream(stream) => {
                stats.streams += 1;
                total_stream_bytes += stream.content.len();
                let filters = stream_filters(&doc, stream);
                // Decoded length is unknowable without decompressing;
                // only unfiltered streams have a known decoded size.
                let filtered_len = if filters.is_empty() {
                    stream.content.len()
                } else {
                    0
                };
                uncompressed_stream_bytes += filtered_len;

                if let Some(declared) = declared_length(&doc, stream) {
                    // `/Length` counts the encoded bytes; a mismatch
                    // means lopdf's stored content disagrees with the
                    // file's own declaration.
                    if declared as usize != stream.content.len() {
                        warnings.push(format!(
                            "object {} {}: /Length {declared} != stored {} bytes",
                            id.0,
                            id.1,
                            stream.content.len()
                        ));
                    }
                }

                streams.push(StreamInfo {
                    name_hint: stream_name_hint(&doc, stream),
                    dict_keys: stream
                        .dict
                        .iter()
                        .map(|(key, _)| format!("/{}", String::from_utf8_lossy(key)))
                        .collect(),
                    raw_len: stream.content.len(),
                    filtered_len,
                    filters,
                });
            }
        }
    }

    streams.sort_by(|a, b| {
        b.raw_len
            .cmp(&a.raw_len)
            .then_with(|| a.name_hint.cmp(&b.name_hint))
    });
    streams.truncate(MAX_BIGGEST_STREAMS);

    let page_objects = doc
        .get_pages()
        .values()
        .filter(|id| doc.objects.contains_key(*id))
        .count();

    let (catalog_objects, catalog_warning) = catalog_reachability(&doc);
    if let Some(w) = catalog_warning {
        warnings.push(w);
    }

    Ok(ObjectReport {
        stats,
        total_stream_bytes,
        uncompressed_stream_bytes,
        biggest_streams: streams,
        page_objects,
        catalog_objects,
        warnings,
    })
}

/// `/Filter` names in decoding order: a single name or each element of
/// a filter array. The entry (and the array itself, if referenced) is
/// dereferenced first; non-name entries are skipped.
fn stream_filters(doc: &Document, stream: &lopdf::Stream) -> Vec<String> {
    let Ok(filter) = stream.dict.get(b"Filter") else {
        return Vec::new();
    };
    let Ok((_, resolved)) = doc.dereference(filter) else {
        return Vec::new();
    };
    let name_of = |obj: &Object| -> Option<String> {
        doc.dereference(obj)
            .ok()
            .and_then(|(_, resolved)| resolved.as_name().ok())
            .map(|n| String::from_utf8_lossy(n).into_owned())
    };
    match resolved {
        Object::Array(items) => items.iter().filter_map(name_of).collect(),
        other => name_of(other).into_iter().collect(),
    }
}

/// The stream's declared `/Length`, dereferenced to an integer.
fn declared_length(doc: &Document, stream: &lopdf::Stream) -> Option<i64> {
    stream
        .dict
        .get(b"Length")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, resolved)| resolved.as_i64().ok())
}

/// Hint naming the stream's role, from `/Subtype` (preferred) or
/// `/Type`. Falls back to `"stream"`.
fn stream_name_hint(doc: &Document, stream: &lopdf::Stream) -> String {
    for key in [b"Subtype".as_slice(), b"Type".as_slice()] {
        if let Some(name) = stream
            .dict
            .get(key)
            .ok()
            .and_then(|o| doc.dereference(o).ok())
            .and_then(|(_, resolved)| resolved.as_name().ok())
        {
            let hint = String::from_utf8_lossy(name).into_owned();
            if !hint.is_empty() {
                return format!("/{hint}");
            }
        }
    }
    "stream".to_string()
}

/// Count distinct indirect objects reachable from the trailer `/Root`
/// catalog, walking dictionaries and arrays with a visited set, a
/// depth cap and a node cap. Returns `(count, warning)`.
fn catalog_reachability(doc: &Document) -> (usize, Option<String>) {
    use std::collections::HashSet;

    let mut visited: HashSet<ObjectId> = HashSet::new();
    let mut truncated = false;
    let mut stack: Vec<(&Object, usize)> = Vec::new();

    match doc.trailer.get(b"Root") {
        Ok(root) => stack.push((root, 0)),
        Err(_) => {
            return (
                0,
                Some("trailer has no /Root entry; catalog walk skipped".to_string()),
            );
        }
    }

    while let Some((object, depth)) = stack.pop() {
        if visited.len() >= MAX_CATALOG_NODES {
            truncated = true;
            break;
        }
        // `dereference` follows reference chains and reports the id it
        // resolved through — that id is what `visited` dedupes on.
        let Ok((resolved_id, resolved)) = doc.dereference(object) else {
            continue;
        };
        if let Some(id) = resolved_id {
            if !visited.insert(id) {
                continue;
            }
        }
        match resolved {
            Object::Dictionary(dict) => {
                if depth >= MAX_CATALOG_DEPTH {
                    truncated = true;
                    continue;
                }
                for (_, value) in dict.iter() {
                    stack.push((value, depth + 1));
                }
            }
            Object::Array(items) => {
                if depth >= MAX_CATALOG_DEPTH {
                    truncated = true;
                    continue;
                }
                for item in items {
                    stack.push((item, depth + 1));
                }
            }
            _ => {}
        }
    }

    let warning = truncated.then(|| {
        format!(
            "catalog reachability walk hit a cap ({MAX_CATALOG_DEPTH} depth / {MAX_CATALOG_NODES} nodes); catalog_objects is a lower bound"
        )
    });
    (visited.len(), warning)
}

#[cfg(test)]
mod tests {
    use lopdf::{Document, Object, Stream, dictionary};

    use crate::crates::core::engine::EngineLimits;

    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;

    use super::object_report;

    /// Build a document exercising every object variant plus two
    /// streams: an unfiltered content stream and a `/FlateDecode`
    /// image XObject with a declared `/Length` that matches its raw
    /// bytes.
    fn rich_pdf() -> Vec<u8> {
        let mut doc = Document::with_version("1.5");

        let pages_id = doc.new_object_id();
        let content_id = doc.add_object(Stream::new(
            dictionary! {
                "Length" => 0,
            },
            b"BT /F1 12 Tf ET".to_vec(),
        ));
        // Fix the declared length to match the stored bytes.
        if let Some(Object::Stream(s)) = doc.objects.get_mut(&content_id) {
            let len = s.content.len() as i64;
            s.dict.set("Length", len);
        }
        let image_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 1,
                "Height" => 1,
                "ColorSpace" => "DeviceGray",
                "BitsPerComponent" => 8,
                "Filter" => "FlateDecode",
                "Length" => 4,
            },
            vec![0x78, 0x9c, 0x03, 0x00],
        ));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
            "Resources" => dictionary! {
                "XObject" => dictionary! { "Im1" => image_id },
            },
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );

        // Standalone scalar/array/dictionary objects of every variant.
        doc.add_object(Object::Name(b"TestName".to_vec()));
        doc.add_object(Object::string_literal("hello"));
        doc.add_object(Object::Integer(42));
        doc.add_object(Object::Real(1.5));
        doc.add_object(Object::Boolean(true));
        doc.add_object(Object::Null);
        doc.add_object(Object::Array(vec![
            Object::Integer(1),
            Object::Name(b"A".to_vec()),
        ]));
        doc.add_object(dictionary! { "K" => "V" });

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
    fn reports_test_utils_pdf() {
        let pdf = pdf_with_pages(&["one", "two"]);
        let report = object_report(&pdf, &EngineLimits::default()).unwrap();

        assert!(report.stats.total_objects > 0);
        assert_eq!(
            report.stats.total_objects,
            report.stats.streams
                + report.stats.dictionaries
                + report.stats.arrays
                + report.stats.names
                + report.stats.strings
                + report.stats.integers
                + report.stats.reals
                + report.stats.booleans
                + report.stats.nulls
                + report.stats.references
        );
        assert!(report.stats.streams >= 2, "one content stream per page");
        assert_eq!(report.page_objects, 2);
        assert!(report.catalog_objects > 0);
        assert!(report.total_stream_bytes > 0);
        assert!(report.uncompressed_stream_bytes <= report.total_stream_bytes);
        assert!(!report.biggest_streams.is_empty());
        // Streams are sorted by raw_len descending.
        for pair in report.biggest_streams.windows(2) {
            assert!(pair[0].raw_len >= pair[1].raw_len);
        }
    }

    #[test]
    fn reports_every_variant_and_stream_details() {
        let pdf = rich_pdf();
        let report = object_report(&pdf, &EngineLimits::default()).unwrap();

        assert_eq!(report.stats.names, 1);
        assert_eq!(report.stats.strings, 1);
        assert_eq!(report.stats.integers, 1);
        assert_eq!(report.stats.reals, 1);
        assert_eq!(report.stats.booleans, 1);
        assert_eq!(report.stats.nulls, 1);
        assert_eq!(report.stats.arrays, 1);
        // Top-level dictionaries: /Pages, /Page, /Catalog and the
        // standalone {K: V} object (stream and page-resource dicts are
        // nested, not indirect objects).
        assert_eq!(report.stats.dictionaries, 4);
        assert_eq!(report.stats.streams, 2);
        assert_eq!(report.page_objects, 1);

        let image = report
            .biggest_streams
            .iter()
            .find(|s| s.name_hint == "/Image")
            .expect("image stream reported");
        assert_eq!(image.raw_len, 4);
        assert_eq!(image.filtered_len, 0, "filtered stream is not decoded");
        assert_eq!(image.filters, vec!["FlateDecode".to_string()]);
        assert!(image.dict_keys.iter().any(|k| k == "/Filter"));
        assert!(image.dict_keys.iter().any(|k| k == "/Subtype"));

        let content = report
            .biggest_streams
            .iter()
            .find(|s| s.filters.is_empty())
            .expect("unfiltered content stream reported");
        assert_eq!(content.filtered_len, content.raw_len);

        // Both streams are uncompressed-known? Only the content stream
        // counts toward uncompressed bytes.
        assert_eq!(
            report.uncompressed_stream_bytes, content.raw_len,
            "filtered stream contributes no uncompressed bytes"
        );
    }

    #[test]
    fn biggest_streams_capped_at_twenty() {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![],
                "Count" => 0,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);

        // 25 streams of increasing size.
        for i in 0..25usize {
            doc.add_object(Stream::new(dictionary! {}, vec![0u8; i + 1]));
        }

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        let report = object_report(&buf, &EngineLimits::default()).unwrap();

        assert_eq!(report.stats.streams, 25);
        assert_eq!(report.biggest_streams.len(), 20);
        assert_eq!(report.biggest_streams[0].raw_len, 25);
        assert_eq!(report.biggest_streams[19].raw_len, 6);
    }

    #[test]
    fn rejects_garbage_input() {
        assert!(object_report(b"not a pdf", &EngineLimits::default()).is_err());
    }

    #[test]
    fn tolerates_missing_root() {
        // A document whose trailer lacks /Root still produces a
        // report, with a warning instead of an error.
        let mut doc = Document::with_version("1.4");
        doc.add_object(Object::Integer(1));
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        let report = object_report(&buf, &EngineLimits::default()).unwrap();
        assert_eq!(report.catalog_objects, 0);
        assert!(!report.warnings.is_empty());
    }
}
