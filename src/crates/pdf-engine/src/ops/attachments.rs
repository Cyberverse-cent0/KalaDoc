//! Embedded file attachments — the catalog `/Names` → `/EmbeddedFiles`
//! name tree.
//!
//! Attachments are stored as file-specification dictionaries whose
//! `/EF` dictionary points at an `/EmbeddedFile` stream. The stream's
//! `/Params` dictionary carries the declared byte size and optional
//! creation/modification dates; the filespec's `/Desc` entry carries
//! the human-readable description.
//!
//! The name tree is read in both legal shapes: a flat `/Names` array
//! of alternating `(name, filespec)` pairs and intermediate `/Kids`
//! nodes. Writes normalize the tree to a single flat `/Names` array
//! while preserving every collected entry and every sibling category
//! under the catalog `/Names` dictionary. Orphaned old tree objects are
//! left in place, matching the engine's no-GC convention.

use lopdf::{Dictionary, Document, Object, ObjectId, Stream, decode_text_string, dictionary};

use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{doc_is_encrypted, load_pdf, lopdf_err, reject_encrypted, save_pdf};

/// Maximum `/Kids` nesting accepted while walking the name tree.
const MAX_TREE_DEPTH: u32 = 32;

/// Maximum attachment entries collected from one document.
const MAX_ATTACHMENTS: usize = 100_000;

/// Maximum byte length of an attachment name accepted for writing.
const MAX_NAME_BYTES: usize = 4096;

/// Extraction bound for one decoded embedded-file stream. This is the
/// same decompression budget `lopdf` applies to page content.
const MAX_ATTACHMENT_BYTES: usize = lopdf::DEFAULT_MAX_DECOMPRESSED_SIZE;

/// Metadata for one embedded file attachment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddedFileInfo {
    /// Name-tree key for the attachment.
    pub name: String,
    /// Filespec `/Desc`, when present and decodable.
    pub description: Option<String>,
    /// `/Params` `/Size` in bytes, or `0` when absent/malformed.
    pub size: u64,
    /// `/Params` `/CreationDate` as stored in the PDF.
    pub created: Option<String>,
    /// `/Params` `/ModDate` as stored in the PDF.
    pub modified: Option<String>,
}

/// Specification for [`add_attachment`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentSpec {
    /// Attachment name written as the name-tree key and as the
    /// filespec's `/F` and `/UF`. Must not be empty, contain `/`, or
    /// contain control characters.
    pub name: String,
    /// Raw file bytes embedded in the `/EmbeddedFile` stream.
    pub data: Vec<u8>,
    /// Optional filespec `/Desc` description.
    pub description: Option<String>,
}

/// One decoded `(name, filespec)` pair collected from the name tree.
struct NamePair {
    name: String,
    value: Object,
}

/// List every embedded file attachment in `input`.
///
/// Entries are returned in name-tree traversal order. Malformed nodes
/// are skipped rather than failing the scan. Encrypted input returns an
/// empty list because names and strings cannot be decoded without
/// decrypting, matching [`super::outline::list_outline`].
pub fn list_attachments(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<EmbeddedFileInfo>> {
    let doc = load_pdf(input, limits)?;
    if doc_is_encrypted(&doc) {
        return Ok(Vec::new());
    }

    let pairs = collect_embedded_file_pairs(&doc)?;
    let mut out = Vec::with_capacity(pairs.len());
    for pair in pairs {
        let Some(spec) = dict_of(&doc, &pair.value) else {
            continue;
        };
        let stream = embedded_file_stream(&doc, spec);
        let params = stream.and_then(|stream| params_dict(&doc, &stream.dict));
        out.push(EmbeddedFileInfo {
            name: pair.name,
            description: spec
                .get(b"Desc")
                .ok()
                .and_then(|value| pdf_text(&doc, value)),
            size: params
                .and_then(|params| integer_value(&doc, params, b"Size"))
                .and_then(|size| u64::try_from(size).ok())
                .unwrap_or(0),
            created: params
                .and_then(|params| params.get(b"CreationDate").ok())
                .and_then(|value| pdf_text(&doc, value)),
            modified: params
                .and_then(|params| params.get(b"ModDate").ok())
                .and_then(|value| pdf_text(&doc, value)),
        });
    }
    Ok(out)
}

/// Extract the decoded bytes of the attachment named `name`.
///
/// `name` is matched against the name-tree key. The `/EF` dictionary's
/// `/F` entry is preferred, with `/UF` and then any other stream-valued
/// entry as fallbacks. Stream filters are decoded with
/// [`MAX_ATTACHMENT_BYTES`] as the output bound. Encrypted documents
/// are rejected.
pub fn extract_attachment(input: &[u8], name: &str, limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;
    validate_lookup_name(name)?;

    let pairs = collect_embedded_file_pairs(&doc)?;
    let pair = pairs
        .iter()
        .find(|pair| pair.name == name)
        .ok_or_else(|| PdfError::InvalidInput(format!("attachment {name:?} not found")))?;
    let spec = dict_of(&doc, &pair.value).ok_or_else(|| {
        PdfError::InvalidInput(format!("attachment {name:?} has no file specification"))
    })?;
    let stream = embedded_file_stream(&doc, spec).ok_or_else(|| {
        PdfError::InvalidInput(format!("attachment {name:?} has no embedded file stream"))
    })?;

    decode_attachment_stream(stream)
}

/// Add `spec` as an embedded file attachment and return the rewritten PDF.
///
/// The attachment is written as an `/EmbeddedFile` stream plus a
/// `/Filespec` dictionary (`/Type`, `/F`, `/UF`, optional `/Desc`, and
/// `/EF` → `/F`/`/UF`). It is inserted into the catalog's `/Names` →
/// `/EmbeddedFiles` name tree, creating `/Names` when absent and
/// preserving sibling name-tree categories. If an attachment with the
/// same name already exists it is replaced.
///
/// Encrypted input is rejected. Names must be non-empty, at most
/// [`MAX_NAME_BYTES`] bytes, and must not contain `/` or control
/// characters.
pub fn add_attachment(
    input: &[u8],
    spec: &AttachmentSpec,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;
    validate_new_name(&spec.name)?;

    let mut pairs = collect_embedded_file_pairs(&doc)?;
    let had_same_name = pairs.iter().any(|pair| pair.name == spec.name);
    pairs.retain(|pair| pair.name != spec.name);
    if !had_same_name && pairs.len() >= MAX_ATTACHMENTS {
        return Err(PdfError::LimitExceeded(format!(
            "embedded file attachments would exceed the {MAX_ATTACHMENTS} cap"
        )));
    }

    let size = i64::try_from(spec.data.len()).map_err(|_| {
        PdfError::LimitExceeded("attachment data exceeds the PDF integer size range".to_string())
    })?;
    let stream_id = doc
        .try_add_object(Stream::new(
            dictionary! {
                "Type" => "EmbeddedFile",
                "Params" => dictionary! {
                    "Size" => size,
                },
            },
            spec.data.clone(),
        ))
        .map_err(lopdf_err)?;

    let name = lopdf::text_string(&spec.name);
    let mut filespec = dictionary! {
        "Type" => "Filespec",
        "F" => name.clone(),
        "UF" => name,
        "EF" => dictionary! {
            "F" => stream_id,
            "UF" => stream_id,
        },
    };
    if let Some(description) = &spec.description {
        filespec.set("Desc", lopdf::text_string(description));
    }
    let spec_id = doc.try_add_object(filespec).map_err(lopdf_err)?;

    pairs.push(NamePair {
        name: spec.name.clone(),
        value: Object::Reference(spec_id),
    });
    write_embedded_files(&mut doc, pairs, false)?;
    save_pdf(&mut doc)
}

/// Remove every attachment named `name` and return the rewritten PDF.
///
/// Other entries in the `/EmbeddedFiles` tree and all sibling `/Names`
/// categories are preserved. If the removed entry was the last one, the
/// `/EmbeddedFiles` key is dropped from the `/Names` dictionary. A
/// missing name is a no-op (the document is still re-serialized).
/// Encrypted input is rejected.
pub fn remove_attachment(input: &[u8], name: &str, limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;
    validate_lookup_name(name)?;

    let pairs = collect_embedded_file_pairs(&doc)?;
    let mut kept = Vec::with_capacity(pairs.len());
    let mut removed = false;
    for pair in pairs {
        if pair.name == name {
            removed = true;
        } else {
            kept.push(pair);
        }
    }
    if !removed {
        return save_pdf(&mut doc);
    }

    write_embedded_files(&mut doc, kept, true)?;
    save_pdf(&mut doc)
}

// ---------------------------------------------------------------------------
// Name-tree traversal
// ---------------------------------------------------------------------------

/// Collect all `(name, filespec)` pairs under `/Names` →
/// `/EmbeddedFiles`, walking both `/Names` arrays and `/Kids` nodes.
fn collect_embedded_file_pairs(doc: &Document) -> PdfResult<Vec<NamePair>> {
    let mut out = Vec::new();
    let Some(tree) = embedded_files_object(doc) else {
        return Ok(out);
    };
    collect_tree_pairs(doc, tree, 0, &mut out)?;
    Ok(out)
}

fn collect_tree_pairs(
    doc: &Document,
    tree: &Object,
    depth: u32,
    out: &mut Vec<NamePair>,
) -> PdfResult<()> {
    if depth > MAX_TREE_DEPTH {
        return Err(PdfError::InvalidInput(
            "embedded file name tree exceeds the depth limit".to_string(),
        ));
    }
    let Some(dict) = dict_of(doc, tree) else {
        return Ok(());
    };

    if let Some(names) = dict
        .get(b"Names")
        .ok()
        .and_then(|object| array_of(doc, object))
    {
        for pair in names.chunks_exact(2) {
            if out.len() >= MAX_ATTACHMENTS {
                return Err(PdfError::LimitExceeded(format!(
                    "embedded file attachments exceed the {MAX_ATTACHMENTS} cap"
                )));
            }
            let Some(name) = pdf_text(doc, &pair[0]) else {
                continue;
            };
            out.push(NamePair {
                name,
                value: pair[1].clone(),
            });
        }
    }

    if let Some(kids) = dict
        .get(b"Kids")
        .ok()
        .and_then(|object| array_of(doc, object))
    {
        for kid in kids {
            collect_tree_pairs(doc, kid, depth + 1, out)?;
        }
    }
    Ok(())
}

/// The catalog `/Names` → `/EmbeddedFiles` value, when present and
/// resolvable to a dictionary-shaped name tree.
fn embedded_files_object<'a>(doc: &'a Document) -> Option<&'a Object> {
    let catalog = doc.catalog().ok()?;
    let names = catalog.get(b"Names").ok()?;
    let names_dict = dict_of(doc, names)?;
    names_dict.get(b"EmbeddedFiles").ok()
}

// ---------------------------------------------------------------------------
// Filespec / stream helpers
// ---------------------------------------------------------------------------

/// Resolve `object` through references and return it as a dictionary.
fn dict_of<'a>(doc: &'a Document, object: &'a Object) -> Option<&'a Dictionary> {
    deref_object(doc, object)?.as_dict().ok()
}

/// Resolve `object` through references and return it as an array slice.
fn array_of<'a>(doc: &'a Document, object: &'a Object) -> Option<&'a [Object]> {
    deref_object(doc, object)?
        .as_array()
        .ok()
        .map(Vec::as_slice)
}

/// Follow a reference chain to its final object.
fn deref_object<'a>(doc: &'a Document, object: &'a Object) -> Option<&'a Object> {
    doc.dereference(object).ok().map(|(_, object)| object)
}

/// Decode a PDF text string, tolerating name objects and undecodable
/// payloads by falling back to a lossy byte rendering.
fn pdf_text(doc: &Document, object: &Object) -> Option<String> {
    let object = deref_object(doc, object)?;
    match object {
        Object::String(_, _) => decode_text_string(object).ok().or_else(|| {
            object
                .as_str()
                .ok()
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        }),
        Object::Name(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        _ => None,
    }
}

/// Read an integer dictionary value, following one level of reference.
fn integer_value(doc: &Document, dict: &Dictionary, key: &[u8]) -> Option<i64> {
    let object = dict.get(key).ok()?;
    deref_object(doc, object)?.as_i64().ok()
}

/// The filespec's `/EF` dictionary as a dictionary, when present.
fn ef_dict<'a>(doc: &'a Document, spec: &'a Dictionary) -> Option<&'a Dictionary> {
    let ef = spec.get(b"EF").ok()?;
    dict_of(doc, ef)
}

/// The embedded-file stream referenced by a filespec.
///
/// `/EF` → `/F` is preferred, then `/UF`, then the first stream-valued
/// `/EF` entry (for nonstandard producers).
fn embedded_file_stream<'a>(doc: &'a Document, spec: &'a Dictionary) -> Option<&'a Stream> {
    let ef = ef_dict(doc, spec)?;
    for key in [b"F".as_slice(), b"UF".as_slice()] {
        if let Ok(object) = ef.get(key)
            && let Some(stream) =
                deref_object(doc, object).and_then(|object| object.as_stream().ok())
        {
            return Some(stream);
        }
    }
    for (_, object) in ef.iter() {
        if let Some(stream) = deref_object(doc, object).and_then(|object| object.as_stream().ok()) {
            return Some(stream);
        }
    }
    None
}

/// The stream's `/Params` dictionary, when present.
fn params_dict<'a>(doc: &'a Document, dict: &'a Dictionary) -> Option<&'a Dictionary> {
    let params = dict.get(b"Params").ok()?;
    dict_of(doc, params)
}

/// Decode an embedded-file stream with a hard output bound.
fn decode_attachment_stream(stream: &Stream) -> PdfResult<Vec<u8>> {
    match stream.decompressed_content_with_limit(MAX_ATTACHMENT_BYTES) {
        Ok(bytes) => Ok(bytes),
        Err(lopdf::Error::Decompress(lopdf::DecompressError::MemoryLimitExceeded { limit })) => {
            Err(PdfError::LimitExceeded(format!(
                "attachment exceeds the {limit}-byte extraction limit"
            )))
        }
        Err(error) => Err(lopdf_err(error)),
    }
}

// ---------------------------------------------------------------------------
// Name-tree writing
// ---------------------------------------------------------------------------

/// The shape of the catalog's `/Names` entry — inspected before
/// mutating, the same pattern used by `remove_objects`.
enum NamesShape {
    Direct,
    Indirect(ObjectId),
    Absent,
    Invalid,
}

fn names_shape(doc: &Document) -> NamesShape {
    let Ok(root_id) = doc.trailer.get(b"Root").and_then(Object::as_reference) else {
        return NamesShape::Absent;
    };
    let Ok(catalog) = doc.get_dictionary(root_id) else {
        return NamesShape::Absent;
    };
    let Ok(names) = catalog.get(b"Names") else {
        return NamesShape::Absent;
    };
    match names {
        Object::Dictionary(_) => NamesShape::Direct,
        Object::Reference(id) => NamesShape::Indirect(*id),
        _ => NamesShape::Invalid,
    }
}

/// Return a mutable view of the catalog `/Names` dictionary, creating
/// an indirect empty dictionary when the catalog has no `/Names`.
fn names_dict_mut<'a>(doc: &'a mut Document) -> PdfResult<&'a mut Dictionary> {
    match names_shape(doc) {
        NamesShape::Absent => {
            let names_id = doc.try_add_object(Dictionary::new()).map_err(lopdf_err)?;
            doc.catalog_mut().map_err(lopdf_err)?.set("Names", names_id);
            doc.get_dictionary_mut(names_id).map_err(lopdf_err)
        }
        NamesShape::Direct => {
            let catalog = doc.catalog_mut().map_err(lopdf_err)?;
            let names = catalog.get_mut(b"Names").map_err(lopdf_err)?;
            names.as_dict_mut().map_err(lopdf_err)
        }
        NamesShape::Indirect(id) => doc.get_dictionary_mut(id).map_err(lopdf_err),
        NamesShape::Invalid => Err(PdfError::InvalidInput(
            "catalog /Names is not a dictionary".to_string(),
        )),
    }
}

/// Replace the `/EmbeddedFiles` subtree with a flat `/Names` array
/// built from `pairs`, or remove the key entirely when `pairs` is empty
/// and `prune_empty` is set.
fn write_embedded_files(
    doc: &mut Document,
    pairs: Vec<NamePair>,
    prune_empty: bool,
) -> PdfResult<()> {
    if pairs.is_empty() {
        if prune_empty {
            names_dict_mut(doc)?.remove(b"EmbeddedFiles");
        }
        return Ok(());
    }

    let mut flat = Vec::with_capacity(pairs.len() * 2);
    for pair in pairs {
        flat.push(lopdf::text_string(&pair.name));
        flat.push(pair.value);
    }
    let embedded_files_id = doc
        .try_add_object(dictionary! {
            "Names" => Object::Array(flat),
        })
        .map_err(lopdf_err)?;
    names_dict_mut(doc)?.set("EmbeddedFiles", embedded_files_id);
    Ok(())
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Names accepted by lookup operations: non-empty and bounded. Lookup
/// deliberately does not reject `/` or control characters because
/// foreign name trees may contain them.
fn validate_lookup_name(name: &str) -> PdfResult<()> {
    if name.is_empty() {
        return Err(PdfError::InvalidInput(
            "attachment name must not be empty".to_string(),
        ));
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(PdfError::InvalidInput(format!(
            "attachment name exceeds {MAX_NAME_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Names accepted for writing: lookup rules plus no `/` and no control
/// characters.
fn validate_new_name(name: &str) -> PdfResult<()> {
    validate_lookup_name(name)?;
    if name.contains('/') || name.chars().any(|ch| ch.is_control()) {
        return Err(PdfError::InvalidInput(
            "attachment name must not contain '/' or control characters".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::Stream;

    fn spec(name: &str, data: &[u8], description: Option<&str>) -> AttachmentSpec {
        AttachmentSpec {
            name: name.to_string(),
            data: data.to_vec(),
            description: description.map(str::to_string),
        }
    }

    /// Build a PDF with a hand-written `/Names` → `/EmbeddedFiles`
    /// tree. `flat` entries go into the root `/Names` array; `kids`
    /// entries go into one intermediate `/Kids` node.
    fn pdf_with_attachment_tree(flat: &[(&str, &[u8])], kids: &[(&str, &[u8])]) -> Vec<u8> {
        let mut doc = Document::load_mem(&pdf_with_pages(&["attachments"])).unwrap();

        let add_filespec = |doc: &mut Document, name: &str, data: &[u8]| {
            let stream_id = doc.add_object(Stream::new(
                dictionary! {
                    "Type" => "EmbeddedFile",
                    "Params" => dictionary! {
                        "Size" => data.len() as i64,
                        "CreationDate" => Object::string_literal("D:20240102030405Z"),
                        "ModDate" => Object::string_literal("D:20240203040506Z"),
                    },
                },
                data.to_vec(),
            ));
            let name_obj = lopdf::text_string(name);
            doc.add_object(dictionary! {
                "Type" => "Filespec",
                "F" => name_obj.clone(),
                "UF" => name_obj,
                "Desc" => Object::string_literal(format!("desc for {name}")),
                "EF" => dictionary! {
                    "F" => stream_id,
                    "UF" => stream_id,
                },
            })
        };

        let mut root_pairs = Vec::new();
        for (name, data) in flat {
            root_pairs.push(Object::string_literal(*name));
            root_pairs.push(Object::Reference(add_filespec(&mut doc, name, data)));
        }

        let embedded_files = if kids.is_empty() {
            dictionary! {
                "Names" => Object::Array(root_pairs),
            }
        } else {
            let mut kid_pairs = Vec::new();
            for (name, data) in kids {
                kid_pairs.push(Object::string_literal(*name));
                kid_pairs.push(Object::Reference(add_filespec(&mut doc, name, data)));
            }
            let kid_id = doc.add_object(dictionary! {
                "Names" => Object::Array(kid_pairs),
                "Limits" => Object::Array(vec![
                    Object::string_literal(kids[0].0),
                    Object::string_literal(kids[kids.len() - 1].0),
                ]),
            });
            let mut root_dict = dictionary! {
                "Kids" => Object::Array(vec![Object::Reference(kid_id)]),
                "Limits" => Object::Array(vec![
                    Object::string_literal(kids[0].0),
                    Object::string_literal(kids[kids.len() - 1].0),
                ]),
            };
            if !root_pairs.is_empty() {
                root_dict.set("Names", Object::Array(root_pairs));
            }
            root_dict
        };

        // Keep a sibling name-tree category to prove it survives writes.
        let catalog = doc.catalog_mut().unwrap();
        catalog.set(
            "Names",
            dictionary! {
                "EmbeddedFiles" => embedded_files,
                "JavaScript" => dictionary! {
                    "Names" => Object::Array(vec![]),
                },
            },
        );

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    #[test]
    fn add_attachment_creates_tree_and_round_trips() {
        let plain = pdf_with_pages(&["base"]);
        let out = add_attachment(
            &plain,
            &spec("notes.txt", b"hello attachment", Some("a note")),
            &EngineLimits::default(),
        )
        .unwrap();

        let infos = list_attachments(&out, &EngineLimits::default()).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name, "notes.txt");
        assert_eq!(infos[0].description.as_deref(), Some("a note"));
        assert_eq!(infos[0].size, 16);
        assert_eq!(infos[0].created, None);
        assert_eq!(infos[0].modified, None);

        let bytes = extract_attachment(&out, "notes.txt", &EngineLimits::default()).unwrap();
        assert_eq!(bytes, b"hello attachment");
    }

    #[test]
    fn list_and_extract_walk_flat_and_kids() {
        let pdf =
            pdf_with_attachment_tree(&[("flat.txt", b"flat-bytes")], &[("kid.bin", b"kid-bytes")]);
        let infos = list_attachments(&pdf, &EngineLimits::default()).unwrap();
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].name, "flat.txt");
        assert_eq!(infos[0].size, 10);
        assert_eq!(infos[0].created.as_deref(), Some("D:20240102030405Z"));
        assert_eq!(infos[0].modified.as_deref(), Some("D:20240203040506Z"));
        assert_eq!(infos[1].name, "kid.bin");
        assert_eq!(infos[1].description.as_deref(), Some("desc for kid.bin"));

        assert_eq!(
            extract_attachment(&pdf, "flat.txt", &EngineLimits::default()).unwrap(),
            b"flat-bytes"
        );
        assert_eq!(
            extract_attachment(&pdf, "kid.bin", &EngineLimits::default()).unwrap(),
            b"kid-bytes"
        );
    }

    #[test]
    fn remove_attachment_preserves_other_entries_and_categories() {
        let pdf = pdf_with_attachment_tree(&[("keep.txt", b"keep"), ("drop.txt", b"drop")], &[]);
        let out = remove_attachment(&pdf, "drop.txt", &EngineLimits::default()).unwrap();

        let infos = list_attachments(&out, &EngineLimits::default()).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name, "keep.txt");
        assert_eq!(
            extract_attachment(&out, "keep.txt", &EngineLimits::default()).unwrap(),
            b"keep"
        );

        let doc = Document::load_mem(&out).unwrap();
        let catalog = doc.catalog().unwrap();
        let names = catalog.get(b"Names").unwrap();
        let (_, names_dict) = doc.dereference(names).unwrap();
        let names_dict = names_dict.as_dict().unwrap();
        assert!(names_dict.has(b"JavaScript"));
    }

    #[test]
    fn add_replaces_existing_name_and_validates_names() {
        let plain = pdf_with_pages(&["base"]);
        let first = add_attachment(
            &plain,
            &spec("same.txt", b"one", None),
            &EngineLimits::default(),
        )
        .unwrap();
        let second = add_attachment(
            &first,
            &spec("same.txt", b"two", Some("replacement")),
            &EngineLimits::default(),
        )
        .unwrap();

        let infos = list_attachments(&second, &EngineLimits::default()).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].description.as_deref(), Some("replacement"));
        assert_eq!(
            extract_attachment(&second, "same.txt", &EngineLimits::default()).unwrap(),
            b"two"
        );

        for bad in ["", "bad/name.txt", "bad\nname.txt", "bad\u{7}name.txt"] {
            assert!(matches!(
                add_attachment(&plain, &spec(bad, b"x", None), &EngineLimits::default()),
                Err(PdfError::InvalidInput(_))
            ));
        }
    }

    #[test]
    fn missing_attachment_and_garbage_input_error() {
        let plain = pdf_with_pages(&["base"]);
        assert!(matches!(
            extract_attachment(&plain, "missing.txt", &EngineLimits::default()),
            Err(PdfError::InvalidInput(_))
        ));
        assert!(matches!(
            list_attachments(b"junk", &EngineLimits::default()),
            Err(PdfError::InvalidPdf(_))
        ));
    }

    #[test]
    fn transforms_reject_encrypted_input() {
        let plain = pdf_with_pages(&["base"]);
        let encrypted = encrypt(
            &plain,
            "pw",
            None,
            PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();

        assert!(matches!(
            add_attachment(
                &encrypted,
                &spec("a.txt", b"x", None),
                &EngineLimits::default()
            ),
            Err(PdfError::Encrypted)
        ));
        assert!(matches!(
            remove_attachment(&encrypted, "a.txt", &EngineLimits::default()),
            Err(PdfError::Encrypted)
        ));
        assert!(matches!(
            extract_attachment(&encrypted, "a.txt", &EngineLimits::default()),
            Err(PdfError::Encrypted)
        ));
        assert!(
            list_attachments(&encrypted, &EngineLimits::default())
                .unwrap()
                .is_empty()
        );
    }
}
