//! De-duplicate identical image XObjects across a document's resources.
//!
//! Pages (and Form XObjects one level deep) expose images through
//! `/Resources` → `/XObject` dictionaries whose entries map a resource
//! *name* to a stream object. Two entries — on the same page or on
//! different pages — can point at two distinct stream objects that
//! carry byte-identical content (common after merges, copies or
//! repeated stamps). This op finds those duplicates and rewrites every
//! duplicate entry to reference the *first* object's id, so all names
//! keep working while the extra stream objects become unreferenced.
//!
//! How it works:
//!
//! 1. **Scan** — for every page, the effective `/Resources` (own or
//!    inherited) `/XObject` dictionary is walked, plus the `/XObject`
//!    dictionaries inside Form XObjects referenced from page-level
//!    resources (one level deep — forms nested inside forms are not
//!    descended). Each entry that dereferences to a stream with
//!    `/Subtype /Image` is hashed over its **raw stored (still
//!    filtered) content bytes** with `DefaultHasher`. Two images are
//!    duplicates only when their stored bytes are identical — decoded
//!    pixel equality is *not* attempted, and images stored under
//!    different filters are not merged.
//! 2. **Rewrite** — for each hash group, the first-seen object id is
//!    canonical; every other resource entry pointing at a different
//!    object with the same hash is set to `Reference(canonical)`.
//!    Resource names are preserved — `/Im1` and `/Im2` both still
//!    work, they just resolve to one object.
//! 3. **Delete** — duplicate objects left with no remaining references
//!    anywhere in the object graph (or trailer) are removed from
//!    `doc.objects`, the same mechanism `prune_objects` uses. A
//!    duplicate still referenced from a place this op does not scan
//!    (annotation appearance streams, patterns, unvisited resource
//!    dictionaries, ...) is kept and reported in `warnings`.
//!
//! Honest scope notes:
//!
//! - Only **indirect** image streams participate: an image stored as a
//!   direct (non-reference) stream inside a dictionary has no object
//!   identity to share and is skipped.
//! - `/XObject` dictionaries that are neither a dictionary nor a
//!   reference are skipped with a warning.
//! - `images_found` counts *distinct indirect image objects* found;
//!   `duplicate_groups` counts content-hash groups that contained more
//!   than one object; `bytes_saved_estimate` sums the stored content
//!   bytes of the objects actually deleted (dictionary and xref
//!   overhead is extra, so treat it as a lower-bound estimate).
//! - Encrypted documents are rejected.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use lopdf::{Dictionary, Document, Object, ObjectId};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::{load_pdf, reject_encrypted, save_pdf};
use super::watermark::resources_object;

/// Result of [`dedup_images`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DedupReport {
    /// Distinct indirect image XObject stream objects discovered in the
    /// scanned resource dictionaries (page `/Resources` plus Form
    /// XObjects one level deep).
    pub images_found: usize,
    /// Number of distinct stream-content hashes shared by more than
    /// one image object — i.e. the number of duplicate groups found.
    pub duplicate_groups: usize,
    /// Duplicate stream objects actually removed from the document.
    /// Objects still referenced from unscanned locations are kept and
    /// reported via `warnings` instead.
    pub objects_removed: usize,
    /// Sum of the stored (still-filtered) content bytes of the removed
    /// objects — a lower-bound estimate of the bytes saved; object
    /// dictionaries and xref entries add a little more.
    pub bytes_saved_estimate: usize,
    /// Non-fatal issues: duplicate objects kept because references
    /// remain outside the scanned dictionaries, or malformed
    /// `/XObject` entries that were skipped.
    pub warnings: Vec<String>,
}

/// Where a `/XObject` dictionary lives, so entries inside it can be
/// rewritten after the scan phase.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum XDictSite {
    /// The `/XObject` value is itself an indirect dictionary object.
    Indirect(ObjectId),
    /// The `/XObject` value is a *direct* dictionary nested in the
    /// dictionary reached from object `obj` by following `keys` (then
    /// the `XObject` key itself). `obj` is a page dictionary, a
    /// `/Resources` dictionary, or — when `via_stream` is set — a Form
    /// XObject stream whose dictionary holds `/Resources`.
    Parent {
        obj: ObjectId,
        keys: Vec<Vec<u8>>,
        via_stream: bool,
    },
}

/// One image resource entry that is a candidate for de-duplication.
struct Candidate {
    site: XDictSite,
    /// Resource name (without leading slash) inside the `/XObject` dict.
    name: Vec<u8>,
    /// Resolved object id of the image stream.
    id: ObjectId,
    /// Hash of the raw stored stream content.
    hash: u64,
    /// Stored content length, for the bytes-saved estimate.
    content_len: usize,
}

/// Mutable scan state shared across pages and Form XObjects.
struct Scan {
    /// Distinct image object ids discovered.
    found_ids: HashSet<ObjectId>,
    /// Form XObject ids awaiting a one-level-deep resource scan.
    form_ids: Vec<ObjectId>,
    /// Forms already queued (an XObject dict may name one form twice).
    seen_forms: HashSet<ObjectId>,
    /// Per-entry dedup candidates in deterministic scan order.
    candidates: Vec<Candidate>,
    warnings: Vec<String>,
}

/// De-duplicate identical image XObjects in `input`; returns the
/// rewritten PDF and a [`DedupReport`]. Encrypted input is rejected.
pub fn dedup_images(input: &[u8], limits: &EngineLimits) -> PdfResult<(Vec<u8>, DedupReport)> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let mut scan = Scan {
        found_ids: HashSet::new(),
        form_ids: Vec::new(),
        seen_forms: HashSet::new(),
        candidates: Vec::new(),
        warnings: Vec::new(),
    };

    // Phase 1a: every page's effective /Resources.
    let pages_map = doc.get_pages();
    let mut page_numbers: Vec<u32> = pages_map.keys().copied().collect();
    page_numbers.sort_unstable();
    for page_number in page_numbers {
        let Some(&page_id) = pages_map.get(&page_number) else {
            continue;
        };
        let Some(resources) = resources_object(&doc, page_id) else {
            continue;
        };
        // Where the /XObject dict lives if it turns out to be a direct
        // dictionary: inside the resources object when /Resources is
        // indirect, else inside the page dict under /Resources.
        let direct_site = match &resources {
            Object::Reference(rid) => XDictSite::Parent {
                obj: *rid,
                keys: Vec::new(),
                via_stream: false,
            },
            Object::Dictionary(_) => XDictSite::Parent {
                obj: page_id,
                keys: vec![b"Resources".to_vec()],
                via_stream: false,
            },
            _ => continue,
        };
        scan.scan_resources(&doc, &resources, &direct_site, true);
    }

    // Phase 1b: Form XObjects referenced from page-level resources —
    // one level deep only; forms inside forms are not descended.
    let form_ids = std::mem::take(&mut scan.form_ids);
    for form_id in form_ids {
        let Ok(object) = doc.get_object(form_id) else {
            continue;
        };
        let Ok(stream) = object.as_stream() else {
            continue;
        };
        let Ok(resources) = stream.dict.get(b"Resources").cloned() else {
            continue;
        };
        let direct_site = match &resources {
            Object::Reference(rid) => XDictSite::Parent {
                obj: *rid,
                keys: Vec::new(),
                via_stream: false,
            },
            Object::Dictionary(_) => XDictSite::Parent {
                obj: form_id,
                keys: vec![b"Resources".to_vec()],
                via_stream: true,
            },
            _ => continue,
        };
        scan.scan_resources(&doc, &resources, &direct_site, false);
    }

    // Phase 2: pick a canonical object per content hash and plan the
    // reference rewrites. Scan order is deterministic (page order, then
    // dictionary order, then form discovery order), so the "first"
    // object is stable.
    let mut canonical: HashMap<u64, ObjectId> = HashMap::new();
    let mut edits: HashMap<XDictSite, Vec<(Vec<u8>, ObjectId)>> = HashMap::new();
    let mut dup_ids: Vec<ObjectId> = Vec::new();
    let mut dup_seen: HashSet<ObjectId> = HashSet::new();
    let mut dup_bytes: HashMap<ObjectId, usize> = HashMap::new();
    let mut dup_group_hashes: HashSet<u64> = HashSet::new();

    for cand in &scan.candidates {
        match canonical.get(&cand.hash) {
            Some(&canon) if canon != cand.id => {
                edits
                    .entry(cand.site.clone())
                    .or_default()
                    .push((cand.name.clone(), canon));
                if dup_seen.insert(cand.id) {
                    dup_ids.push(cand.id);
                    dup_bytes.insert(cand.id, cand.content_len);
                }
                dup_group_hashes.insert(cand.hash);
            }
            _ => {
                canonical.entry(cand.hash).or_insert(cand.id);
            }
        }
    }

    // Phase 3: rewrite the resource entries so every duplicate name
    // resolves to the canonical object.
    for (site, pairs) in &edits {
        match xobject_dict_mut(&mut doc, site) {
            Some(dict) => {
                for (name, target) in pairs {
                    dict.set(name.clone(), Object::Reference(*target));
                }
            }
            None => scan.warnings.push(format!(
                "could not locate XObject dictionary at {site:?}; \
                 {count} duplicate reference(s) left unchanged",
                count = pairs.len()
            )),
        }
    }

    // Phase 4: collect every object id still referenced anywhere
    // (objects + trailer), then delete only the duplicates nothing
    // references anymore.
    let mut referenced: HashSet<ObjectId> = HashSet::new();
    for (_, object) in doc.objects.iter() {
        collect_references(object, &mut referenced);
    }
    for (_, value) in doc.trailer.iter() {
        collect_references(value, &mut referenced);
    }

    let mut objects_removed = 0usize;
    let mut bytes_saved_estimate = 0usize;
    for id in dup_ids {
        if referenced.contains(&id) {
            scan.warnings.push(format!(
                "duplicate image object {id:?} is still referenced outside \
                 the scanned resource dictionaries; left in place"
            ));
            continue;
        }
        if doc.objects.remove(&id).is_some() {
            objects_removed += 1;
            bytes_saved_estimate += dup_bytes.get(&id).copied().unwrap_or(0);
        }
    }

    let images_found = scan.found_ids.len();
    let duplicate_groups = dup_group_hashes.len();
    let bytes = save_pdf(&mut doc)?;
    Ok((
        bytes,
        DedupReport {
            images_found,
            duplicate_groups,
            objects_removed,
            bytes_saved_estimate,
            warnings: scan.warnings,
        },
    ))
}

impl Scan {
    /// Scan one `/Resources` value (a cloned [`Object`]: direct
    /// dictionary or reference) for image XObjects. When
    /// `descend_forms` is set, Form XObject entries are queued for the
    /// one-level-deep pass; `direct_site` locates the `/XObject`
    /// dictionary when it is stored directly inside the resources
    /// dictionary.
    fn scan_resources(
        &mut self,
        doc: &Document,
        resources: &Object,
        direct_site: &XDictSite,
        descend_forms: bool,
    ) {
        let res_dict = match resources {
            Object::Reference(id) => match doc.get_dictionary(*id) {
                Ok(d) => d,
                Err(_) => return,
            },
            Object::Dictionary(d) => d,
            _ => return,
        };
        let Ok(xobj) = res_dict.get(b"XObject") else {
            return;
        };
        // Clone the /XObject value so the local `xobj` can be matched
        // against while `doc` borrows stay independent of `self`.
        let xobj = xobj.clone();
        let (site, xdict): (XDictSite, &Dictionary) = match &xobj {
            Object::Reference(xid) => match doc.get_dictionary(*xid) {
                Ok(d) => (XDictSite::Indirect(*xid), d),
                Err(_) => return,
            },
            Object::Dictionary(d) => (direct_site.clone(), d),
            _ => {
                self.warnings.push(
                    "/XObject resource is neither a dictionary nor a reference; skipped"
                        .to_string(),
                );
                return;
            }
        };
        for (name, value) in xdict.iter() {
            let Ok((oid, target)) = doc.dereference(value) else {
                continue;
            };
            let Ok(stream) = target.as_stream() else {
                continue;
            };
            if is_image_stream(doc, &stream.dict) {
                // Direct (non-reference) image streams have no object
                // id to share — they cannot participate in dedup.
                let Some(id) = oid else { continue };
                self.found_ids.insert(id);
                self.candidates.push(Candidate {
                    site: site.clone(),
                    name: name.clone(),
                    id,
                    hash: hash_bytes(&stream.content),
                    content_len: stream.content.len(),
                });
            } else if descend_forms && is_form_stream(doc, &stream.dict) {
                if let Some(id) = oid
                    && self.seen_forms.insert(id)
                {
                    self.form_ids.push(id);
                }
            }
        }
    }
}

/// Borrow the `/XObject` dictionary a [`XDictSite`] points at,
/// mutably. Returns `None` when the path no longer resolves (the
/// document changed shape between scan and rewrite — reported as a
/// warning by the caller).
fn xobject_dict_mut<'a>(doc: &'a mut Document, site: &XDictSite) -> Option<&'a mut Dictionary> {
    match site {
        XDictSite::Indirect(id) => doc.get_object_mut(*id).ok()?.as_dict_mut().ok(),
        XDictSite::Parent {
            obj,
            keys,
            via_stream,
        } => {
            let object = doc.get_object_mut(*obj).ok()?;
            let mut dict: &mut Dictionary = if *via_stream {
                &mut object.as_stream_mut().ok()?.dict
            } else {
                object.as_dict_mut().ok()?
            };
            for key in keys {
                dict = dict.get_mut(key).ok()?.as_dict_mut().ok()?;
            }
            dict.get_mut(b"XObject").ok()?.as_dict_mut().ok()
        }
    }
}

/// Whether a stream dictionary declares `/Subtype /Image`.
fn is_image_stream(doc: &Document, dict: &Dictionary) -> bool {
    subtype_name(doc, dict).is_some_and(|name| name == b"Image")
}

/// Whether a stream dictionary declares `/Subtype /Form`.
fn is_form_stream(doc: &Document, dict: &Dictionary) -> bool {
    subtype_name(doc, dict).is_some_and(|name| name == b"Form")
}

/// Dereferenced `/Subtype` name of a stream dictionary, if readable.
fn subtype_name<'a>(doc: &'a Document, dict: &'a Dictionary) -> Option<&'a [u8]> {
    dict.get(b"Subtype")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_name().ok())
}

/// Non-cryptographic content hash — collisions only cost a missed or
/// (theoretically) incorrect merge of byte-different streams, and
/// `DefaultHasher` is deterministic for a fixed key set.
fn hash_bytes(content: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

/// Insert every [`Object::Reference`] target reachable inside `object`
/// into `out` — dictionaries, arrays and stream dictionaries recurse.
fn collect_references(object: &Object, out: &mut HashSet<ObjectId>) {
    match object {
        Object::Reference(id) => {
            out.insert(*id);
        }
        Object::Array(items) => {
            for item in items {
                collect_references(item, out);
            }
        }
        Object::Dictionary(dict) => {
            for (_, value) in dict.iter() {
                collect_references(value, out);
            }
        }
        Object::Stream(stream) => {
            for (_, value) in stream.dict.iter() {
                collect_references(value, out);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::{Stream, dictionary};

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    /// An image stream dictionary + content shared by the fixtures.
    const IMG_BYTES: &[u8] = &[0u8, 255, 128, 64, 32, 16, 8, 4];

    fn image_stream_dict() -> lopdf::Dictionary {
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => 2i64,
            "Height" => 4i64,
            "BitsPerComponent" => 8i64,
            "ColorSpace" => "DeviceGray",
        }
    }

    /// Build a one-page PDF whose /XObject dict holds `Im1` and `Im2`
    /// pointing at two *distinct* stream objects with identical bytes.
    fn pdf_with_duplicate_images() -> Vec<u8> {
        pdf_with_xobject_entries(&[("Im1", IMG_BYTES), ("Im2", IMG_BYTES)])
    }

    /// Build a one-page PDF whose /XObject dict maps each `(name, bytes)`
    /// pair to its own image stream object.
    fn pdf_with_xobject_entries(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let mut xobjects = Dictionary::new();
        for (name, bytes) in entries {
            let id = doc.add_object(Stream::new(image_stream_dict(), bytes.to_vec()));
            xobjects.set(name.as_bytes(), id);
        }
        let resources_id = doc.add_object(dictionary! {
            "XObject" => xobjects,
        });
        let content = lopdf::content::Content {
            operations: vec![lopdf::content::Operation::new("q", vec![])],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "Contents" => content_id, "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1i64,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    /// Resolve the page's `/XObject` dictionary from a serialized PDF.
    fn xobject_dict(pdf: &[u8]) -> (Document, Dictionary) {
        let doc = Document::load_mem(pdf).unwrap();
        let pages = doc.get_pages();
        let page_id = *pages.values().next().unwrap();
        let resources = resources_object(&doc, page_id).unwrap();
        let res_dict = match &resources {
            Object::Reference(id) => doc.get_dictionary(*id).unwrap().clone(),
            Object::Dictionary(d) => d.clone(),
            _ => panic!("unexpected /Resources shape"),
        };
        let xobj = res_dict.get(b"XObject").unwrap().clone();
        let dict = match xobj {
            Object::Dictionary(d) => d,
            Object::Reference(id) => doc.get_dictionary(id).unwrap().clone(),
            _ => panic!("unexpected /XObject shape"),
        };
        (doc, dict)
    }

    #[test]
    fn merges_identical_images_under_different_names() {
        let pdf = pdf_with_duplicate_images();
        let (out, report) = dedup_images(&pdf, &limits()).unwrap();

        assert_eq!(report.images_found, 2);
        assert_eq!(report.duplicate_groups, 1);
        assert_eq!(report.objects_removed, 1);
        assert_eq!(report.bytes_saved_estimate, IMG_BYTES.len());
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);

        // Both names must still exist and now resolve to the same object.
        let (doc, xdict) = xobject_dict(&out);
        let im1 = xdict.get(b"Im1").unwrap().as_reference().unwrap();
        let im2 = xdict.get(b"Im2").unwrap().as_reference().unwrap();
        assert_eq!(im1, im2, "duplicate names should share one object");
        // The surviving object is still a valid image stream.
        let stream = doc.get_object(im1).unwrap().as_stream().unwrap();
        assert_eq!(stream.content, IMG_BYTES);
    }

    #[test]
    fn distinct_images_are_untouched() {
        let pdf = pdf_with_xobject_entries(&[("Im1", &[1u8, 2, 3]), ("Im2", &[9u8, 8, 7])]);
        let (out, report) = dedup_images(&pdf, &limits()).unwrap();
        assert_eq!(report.images_found, 2);
        assert_eq!(report.duplicate_groups, 0);
        assert_eq!(report.objects_removed, 0);
        assert_eq!(report.bytes_saved_estimate, 0);

        let (_, xdict) = xobject_dict(&out);
        let im1 = xdict.get(b"Im1").unwrap().as_reference().unwrap();
        let im2 = xdict.get(b"Im2").unwrap().as_reference().unwrap();
        assert_ne!(im1, im2);
    }

    #[test]
    fn dedupes_across_form_xobjects_one_level_deep() {
        // Page XObject: /Fm1 -> Form whose own /Resources /XObject holds
        // /A and /B pointing at identical image streams.
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();

        let img_a = doc.add_object(Stream::new(image_stream_dict(), IMG_BYTES.to_vec()));
        let img_b = doc.add_object(Stream::new(image_stream_dict(), IMG_BYTES.to_vec()));
        let form_resources = doc.add_object(dictionary! {
            "XObject" => dictionary! { "A" => img_a, "B" => img_b },
        });
        let form_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 10.into(), 10.into()],
                "Resources" => form_resources,
            },
            b"q Q".to_vec(),
        ));
        let page_resources = doc.add_object(dictionary! {
            "XObject" => dictionary! { "Fm1" => form_id },
        });
        let content_id = doc.add_object(Stream::new(dictionary! {}, b"q Q".to_vec()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id,
            "Contents" => content_id, "Resources" => page_resources,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1i64,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut pdf = Vec::new();
        doc.save_to(&mut pdf).unwrap();

        let (out, report) = dedup_images(&pdf, &limits()).unwrap();
        assert_eq!(report.images_found, 2);
        assert_eq!(report.duplicate_groups, 1);
        assert_eq!(report.objects_removed, 1);

        // Inside the form's resources, /A and /B now share one object.
        let doc = Document::load_mem(&out).unwrap();
        let form = doc.get_object(form_id).unwrap().as_stream().unwrap();
        let res = form.dict.get(b"Resources").unwrap();
        let res_dict = match res {
            Object::Reference(id) => doc.get_dictionary(*id).unwrap(),
            Object::Dictionary(d) => d,
            _ => panic!("unexpected form /Resources"),
        };
        let xobj = res_dict.get(b"XObject").unwrap();
        let xdict = match xobj {
            Object::Dictionary(d) => d.clone(),
            Object::Reference(id) => doc.get_dictionary(*id).unwrap().clone(),
            _ => panic!("unexpected form /XObject"),
        };
        assert_eq!(
            xdict.get(b"A").unwrap().as_reference().unwrap(),
            xdict.get(b"B").unwrap().as_reference().unwrap()
        );
    }

    #[test]
    fn imageless_document_is_noop() {
        let pdf = pdf_with_pages(&["a", "b"]);
        let (out, report) = dedup_images(&pdf, &limits()).unwrap();
        assert_eq!(report.images_found, 0);
        assert_eq!(report.duplicate_groups, 0);
        assert_eq!(report.objects_removed, 0);
        assert_eq!(report.bytes_saved_estimate, 0);
        assert!(Document::load_mem(&out).is_ok());
    }

    #[test]
    fn rejects_garbage_and_encrypted() {
        assert!(dedup_images(b"junk", &limits()).is_err());

        let pdf = pdf_with_duplicate_images();
        let encrypted = crate::crates::pdf_engine::ops::encrypt::encrypt(
            &pdf,
            "pw",
            None,
            crate::crates::pdf_engine::ops::encrypt::PdfPermissions::default(),
            &limits(),
        )
        .unwrap();
        assert!(matches!(
            dedup_images(&encrypted, &limits()),
            Err(crate::crates::pdf_engine::error::PdfError::Encrypted)
        ));
    }
}
