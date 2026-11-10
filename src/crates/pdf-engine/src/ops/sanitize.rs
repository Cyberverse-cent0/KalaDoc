//! Strip active content from a PDF: anything that can execute
//! JavaScript, launch external applications or documents, submit data,
//! or pull in embedded/rich-media payloads when a viewer opens it.
//!
//! This is a structural rewrite over `lopdf` dictionaries — every
//! dictionary and stream dictionary in the document is visited and the
//! keys below are removed wherever they appear:
//!
//! | Removed | What it is |
//! |---|---|
//! | `/OpenAction` | Catalog-level action run when the document opens |
//! | `/AA` | Additional-actions dictionary (page-open, widget events, ...) |
//! | `/JS`, `/JavaScript` | JavaScript action payloads / name-tree entries |
//!
//! On top of that, any `/A` (action) entry whose `/S` subtype is in
//! [`DANGEROUS_ACTIONS`] is removed — this covers link/Widget
//! annotations on pages and outline (bookmark) items, whether the action
//! dictionary is inline or an indirect reference. The strip list is
//! deliberately conservative: only subtypes that can reach outside the
//! document or execute code are removed; benign internal actions such
//! as `/GoTo` are preserved (see the test `sanitize_keeps_benign_actions`).
//!
//! The `/Names` dictionary gets two extra keys removed —
//! `/JavaScript` and `/EmbeddedFiles` (see [`NAMES_STRIP_KEYS`]) —
//! whether it is a direct dictionary on the catalog or an indirect
//! object. Removing the name-tree entries orphans the embedded file
//! streams and JavaScript payloads they pointed at: the objects remain
//! in the file as unreferenced data, unreachable from any viewer
//! navigation, which is what sanitize guarantees. True byte-level
//! removal would require object-graph garbage collection, which this
//! op does not attempt.
//!
//! Honest limits: this removes *declared* active content. It does not
//! inspect content streams for inline JavaScript actions (PDF
//! JavaScript only exists in name trees and action dictionaries, so
//! coverage is complete for spec-conformant files), does not strip
//! `/AcroForm` XFA packets, `/RichMedia` annotation payloads referenced
//! from page resources, or embedded GoToR targets' content. Encrypted
//! input is rejected — rewriting ciphertext would corrupt the file.

use std::collections::HashSet;

use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::{load_pdf, reject_encrypted, save_pdf};

/// `/S` action subtypes that are stripped wherever they appear as the
/// value of an `/A` entry.
///
/// `URI` and `Launch` reach outside the document; `SubmitForm` and
/// `ImportData` move document data out; `JavaScript`, `Named` (which can
/// resolve to a JavaScript action), `RichMedia`, `Sound`, `Movie` and
/// `Rendition` are active or externally-referencing media; `GoToR` and
/// `GoToE` open other documents/files.
const DANGEROUS_ACTIONS: &[&[u8]] = &[
    b"Launch",
    b"URI",
    b"SubmitForm",
    b"ImportData",
    b"JavaScript",
    b"Named",
    b"RichMedia",
    b"Sound",
    b"Movie",
    b"Rendition",
    b"GoToR",
    b"GoToE",
];

/// Dictionary keys removed unconditionally from every dictionary in the
/// document: catalog/page open actions, additional-actions tables, and
/// inline JavaScript payloads.
const STRIP_KEYS: &[&[u8]] = &[b"OpenAction", b"AA", b"JS", b"JavaScript"];

/// Keys removed from the document `/Names` dictionary: the JavaScript
/// name tree, the embedded-file name tree, and the rich-media name
/// tree. (`/EmbeddedFiles` and `/RichMedia` are not in [`STRIP_KEYS`]
/// because those key names are only meaningful under `/Names`; removing
/// them everywhere could hit unrelated uses.)
const NAMES_STRIP_KEYS: &[&[u8]] = &[b"JavaScript", b"EmbeddedFiles", b"RichMedia"];

/// Remove active content from `input`; returns the sanitized PDF.
///
/// Every dictionary reachable as an indirect object — plus dictionaries
/// nested directly inside them (inline action dicts, annots arrays,
/// outline items, the catalog, the `/Names` tree) — is cleaned. See the
/// module docs for the exact strip list.
///
/// Errors: [`PdfError::Encrypted`] for encrypted input,
/// [`PdfError::InvalidPdf`] for unparseable input, limit errors as
/// usual.
pub fn sanitize(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    // Pass 1 (immutable): find indirect objects that *are* dangerous
    // action dictionaries, so pass 2 can remove `/A` entries that merely
    // reference them.
    let mut dangerous_ids: HashSet<ObjectId> = HashSet::new();
    for (id, obj) in &doc.objects {
        let dict = match obj {
            Object::Dictionary(d) => Some(d),
            Object::Stream(s) => Some(&s.dict),
            _ => None,
        };
        if let Some(dict) = dict
            && is_dangerous_action(dict)
        {
            dangerous_ids.insert(*id);
        }
    }

    // Pass 2 (mutable): clean every object. References are not followed
    // here — each indirect object cleans itself in its own iteration,
    // and inline dictionaries are reached by recursion.
    for (_, obj) in doc.objects.iter_mut() {
        match obj {
            Object::Dictionary(dict) => clean_dict(dict, &dangerous_ids),
            Object::Stream(stream) => clean_dict(&mut stream.dict, &dangerous_ids),
            _ => {}
        }
    }

    // The /Names dictionary: strip the JavaScript and embedded-file
    // name trees. /Names may be a direct dictionary on the catalog or
    // an indirect object. (A direct dict was already visited by the
    // generic pass, but "EmbeddedFiles" is deliberately not a
    // STRIP_KEYS member, so it still needs this targeted removal.)
    strip_names_tree(&mut doc);

    save_pdf(&mut doc)
}

/// Whether `dict` is an action dictionary (`/S` present) whose subtype
/// is in [`DANGEROUS_ACTIONS`].
fn is_dangerous_action(dict: &Dictionary) -> bool {
    dict.get(b"S")
        .ok()
        .and_then(|s| s.as_name().ok())
        .is_some_and(|name| DANGEROUS_ACTIONS.contains(&name))
}

/// Remove [`STRIP_KEYS`] and dangerous `/A` entries from `dict`, then
/// recurse into every nested dictionary/array so inline structures
/// (annotation arrays, outline item chains, action dictionaries) are
/// cleaned too.
fn clean_dict(dict: &mut Dictionary, dangerous_ids: &HashSet<ObjectId>) {
    for key in STRIP_KEYS {
        dict.remove(key);
    }

    // /A may be an inline action dict or a reference to one (collected
    // in pass 1). Anything else (missing, malformed) is left alone.
    let dangerous_a = match dict.get(b"A") {
        Ok(Object::Dictionary(action)) => is_dangerous_action(action),
        Ok(Object::Reference(id)) => dangerous_ids.contains(id),
        _ => false,
    };
    if dangerous_a {
        dict.remove(b"A");
    }

    for (_, value) in dict.iter_mut() {
        clean_value(value, dangerous_ids);
    }
}

fn clean_value(obj: &mut Object, dangerous_ids: &HashSet<ObjectId>) {
    match obj {
        Object::Dictionary(dict) => clean_dict(dict, dangerous_ids),
        Object::Array(items) => {
            for item in items {
                clean_value(item, dangerous_ids);
            }
        }
        Object::Stream(stream) => clean_dict(&mut stream.dict, dangerous_ids),
        _ => {}
    }
}

/// Strip [`NAMES_STRIP_KEYS`] from the catalog's `/Names` dictionary,
/// handling both the direct-dictionary and indirect-reference forms.
fn strip_names_tree(doc: &mut Document) {
    let Ok(root_id) = doc.trailer.get(b"Root").and_then(Object::as_reference) else {
        return;
    };

    // Inspect first (immutable) to learn the /Names shape, then mutate —
    // the catalog borrow and the names-object borrow cannot overlap.
    enum NamesShape {
        Direct,
        Indirect(ObjectId),
        Other,
    }
    let shape = doc
        .get_dictionary(root_id)
        .ok()
        .and_then(|catalog| catalog.get(b"Names").ok())
        .map(|names| match names {
            Object::Dictionary(_) => NamesShape::Direct,
            Object::Reference(id) => NamesShape::Indirect(*id),
            _ => NamesShape::Other,
        })
        .unwrap_or(NamesShape::Other);

    match shape {
        NamesShape::Direct => {
            if let Ok(catalog) = doc.get_dictionary_mut(root_id)
                && let Ok(Object::Dictionary(names)) = catalog.get_mut(b"Names")
            {
                for key in NAMES_STRIP_KEYS {
                    names.remove(key);
                }
            }
        }
        NamesShape::Indirect(id) => {
            if let Ok(names) = doc.get_dictionary_mut(id) {
                for key in NAMES_STRIP_KEYS {
                    names.remove(key);
                }
            }
        }
        NamesShape::Other => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::error::PdfError;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::dictionary;

    /// Build a PDF with the usual active-content carriers injected:
    /// catalog `/OpenAction` + `/AA` + `/Names` (JavaScript and
    /// EmbeddedFiles trees), a page annotation with a `/URI` action and
    /// an `/AA` table, and an outline item with a `/JavaScript` action.
    fn pdf_with_active_content() -> Vec<u8> {
        let mut doc = Document::load_mem(&pdf_with_pages(&["click me"])).unwrap();

        let js_action = || {
            dictionary! {
                "S" => "JavaScript",
                "JS" => Object::string_literal("app.alert('xss')"),
            }
        };

        let js_payload_id = doc.add_object(js_action());

        // Catalog: OpenAction, AA, Names.
        let root_id = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        {
            let catalog = doc.get_dictionary_mut(root_id).unwrap();
            catalog.set("OpenAction", js_action());
            catalog.set(
                "AA",
                dictionary! { "WC" => dictionary! { "S" => "JavaScript", "JS" => Object::string_literal("app.alert('wc')") } },
            );
            catalog.set(
                "Names",
                dictionary! {
                    "JavaScript" => dictionary! {
                        "Names" => Object::Array(vec![
                            Object::string_literal("a"),
                            Object::Reference(js_payload_id),
                        ]),
                    },
                    "EmbeddedFiles" => dictionary! {
                        "Names" => Object::Array(vec![]),
                    },
                },
            );
        }

        // Page annotation with a dangerous /A and an /AA table.
        let (_, page_id) = doc
            .get_pages()
            .iter()
            .next()
            .map(|(n, i)| (*n, *i))
            .unwrap();
        let annot_id = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Link",
            "Rect" => vec![0.into(), 0.into(), 10.into(), 10.into()],
            "A" => dictionary! {
                "S" => "URI",
                "URI" => Object::string_literal("https://evil.example"),
            },
            "AA" => dictionary! {
                "E" => js_action(),
            },
        });
        {
            let page = doc.get_dictionary_mut(page_id).unwrap();
            page.set("Annots", Object::Array(vec![Object::Reference(annot_id)]));
        }

        // Outline item with a JavaScript action.
        let outline_id = doc.add_object(dictionary! {
            "Title" => Object::string_literal("bad bookmark"),
            "Parent" => Object::Null,
            "A" => js_action(),
        });
        {
            let catalog = doc.get_dictionary_mut(root_id).unwrap();
            catalog.set(
                "Outlines",
                dictionary! {
                    "Type" => "Outlines",
                    "First" => Object::Reference(outline_id),
                    "Last" => Object::Reference(outline_id),
                    "Count" => 1,
                },
            );
        }

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    #[test]
    fn sanitize_strips_active_content() {
        let dirty = pdf_with_active_content();
        let clean = sanitize(&dirty, &EngineLimits::default()).unwrap();

        let doc = Document::load_mem(&clean).unwrap();
        let root_id = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        let catalog = doc.get_dictionary(root_id).unwrap();

        assert!(catalog.get(b"OpenAction").is_err(), "OpenAction removed");
        assert!(catalog.get(b"AA").is_err(), "catalog AA removed");

        let names = catalog.get(b"Names").unwrap().as_dict().unwrap();
        assert!(names.get(b"JavaScript").is_err(), "JS name tree removed");
        assert!(
            names.get(b"EmbeddedFiles").is_err(),
            "embedded files removed"
        );

        // Page annotation: /A (URI) and /AA gone.
        let (_, page_id) = doc
            .get_pages()
            .iter()
            .next()
            .map(|(n, i)| (*n, *i))
            .unwrap();
        let page = doc.get_dictionary(page_id).unwrap();
        let annots = page.get(b"Annots").unwrap().as_array().unwrap();
        let annot_id = annots[0].as_reference().unwrap();
        let annot = doc.get_dictionary(annot_id).unwrap();
        assert!(annot.get(b"A").is_err(), "URI action removed");
        assert!(annot.get(b"AA").is_err(), "annot AA removed");
        assert!(annot.get(b"Subtype").is_ok(), "harmless keys kept");

        // Outline item action removed.
        let outlines = catalog.get(b"Outlines").unwrap().as_dict().unwrap();
        let first_id = outlines.get(b"First").unwrap().as_reference().unwrap();
        let item = doc.get_dictionary(first_id).unwrap();
        assert!(item.get(b"A").is_err(), "outline JS action removed");
        assert!(item.get(b"Title").is_ok(), "outline title kept");
    }

    #[test]
    fn sanitize_keeps_benign_actions() {
        // A /GoTo action (internal page link) must survive.
        let mut doc = Document::load_mem(&pdf_with_pages(&["a", "b"])).unwrap();
        let js_like = dictionary! {
            "S" => "GoTo",
            "D" => vec![1.into(), "Fit".into()],
        };
        let (_, page_id) = doc
            .get_pages()
            .iter()
            .next()
            .map(|(n, i)| (*n, *i))
            .unwrap();
        let annot_id = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Link",
            "Rect" => vec![0.into(), 0.into(), 10.into(), 10.into()],
            "A" => js_like,
        });
        doc.get_dictionary_mut(page_id)
            .unwrap()
            .set("Annots", Object::Array(vec![Object::Reference(annot_id)]));
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();

        let clean = sanitize(&buf, &EngineLimits::default()).unwrap();
        let doc = Document::load_mem(&clean).unwrap();
        let (_, page_id) = doc
            .get_pages()
            .iter()
            .next()
            .map(|(n, i)| (*n, *i))
            .unwrap();
        let page = doc.get_dictionary(page_id).unwrap();
        let annots = page.get(b"Annots").unwrap().as_array().unwrap();
        let annot = doc
            .get_dictionary(annots[0].as_reference().unwrap())
            .unwrap();
        assert!(annot.get(b"A").is_ok(), "GoTo action preserved");
    }

    #[test]
    fn sanitize_rejects_garbage_and_encrypted() {
        assert!(matches!(
            sanitize(b"junk", &EngineLimits::default()),
            Err(PdfError::InvalidPdf(_))
        ));
        let plain = pdf_with_pages(&["x"]);
        let encrypted = crate::crates::pdf_engine::ops::encrypt::encrypt(
            &plain,
            "pw",
            None,
            crate::crates::pdf_engine::ops::encrypt::PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        assert!(matches!(
            sanitize(&encrypted, &EngineLimits::default()),
            Err(PdfError::Encrypted)
        ));
    }
}
