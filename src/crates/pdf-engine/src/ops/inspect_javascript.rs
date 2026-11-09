//! Read-only inventory of JavaScript actions hidden in a PDF.
//!
//! This is the inspection counterpart to [`super::sanitize`]: where
//! sanitize strips active content, this op reports *where* JavaScript
//! lives without modifying anything. It walks the same structural
//! carriers sanitize cleans:
//!
//! | Location | PDF structure |
//! |---|---|
//! | Document open | catalog `/OpenAction` |
//! | Document additional actions | catalog `/AA` (`/WC`, `/WS`, `/DS`, ...) |
//! | Document name tree | catalog `/Names` → `/JavaScript` name tree |
//! | Per-page | page `/AA` |
//! | Per-annotation | page `/Annots` → each annot's `/AA` and `/A` |
//! | Outline/bookmarks | outline items' `/A` (walked via `/First`/`/Next`) |
//!
//! Only actions whose `/S` subtype is `/JavaScript` are reported —
//! other action types (`/URI`, `/GoTo`, ...) are not JavaScript. The
//! `/JS` payload is decoded with [`lopdf::decode_text_string`] and
//! truncated to 300 characters; when the string cannot be decoded
//! (encrypted input, malformed string) the snippet is left empty
//! rather than failing the scan, so encrypted documents are still
//! inventoried as far as lopdf can read them.
//!
//! Honest limits: like sanitize, this covers spec-conformant
//! locations only — JavaScript cannot hide inside page content
//! streams, but name-tree entries nested under other `/Names`
//! categories, `/AcroForm` XFA packets, and JavaScript referenced
//! indirectly through `/Named` actions are out of scope. Nothing is
//! removed or executed; this is a structural inventory only.

use lopdf::{Document, Object, decode_text_string};

use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::load_pdf;

/// Maximum length of the reported JavaScript snippet.
const MAX_SNIPPET_CHARS: usize = 300;

/// One JavaScript action found in the document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsInfo {
    /// Human-readable location of the action, e.g.
    /// `"catalog /OpenAction"`, `"page 2 /Annots[0] /A"`,
    /// `"names /JavaScript 'foo'"`, `"outline 'Title' /A"`.
    pub location: String,
    /// Registered name for name-tree entries (`/Names/JavaScript`),
    /// `None` for action-dictionary occurrences.
    pub name: Option<String>,
    /// The action's `/S` subtype as a string (always `"JavaScript"`
    /// for entries this op reports; kept for future extensibility).
    pub action_type: String,
    /// First 300 characters of the decoded `/JS` payload, or empty
    /// when it could not be decoded.
    pub snippet: String,
}

/// Scan `input` for JavaScript actions; returns a read-only
/// [`JsInfo`] per occurrence.
///
/// Unlike transforming ops this tolerates encrypted documents —
/// structure is still reported, with undecodable strings surfaced as
/// empty snippets. Input-side limits (`max_input_bytes`, `max_pages`)
/// are enforced by [`load_pdf`].
pub fn inspect_javascript(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<JsInfo>> {
    let doc = load_pdf(input, limits)?;
    let mut out = Vec::new();

    scan_catalog(&doc, &mut out);
    scan_name_tree(&doc, &mut out);
    scan_pages(&doc, &mut out);
    scan_outline(&doc, &mut out);

    Ok(out)
}

/// Resolve one level of indirection: `Object::Reference` → the target
/// object via `Document::dereference`. Non-references pass through.
/// Returns `None` for dangling references.
fn resolve<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a Object> {
    match obj {
        Object::Reference(_) => doc.dereference(obj).ok().map(|(_, o)| o),
        _ => Some(obj),
    }
}

/// Resolve `obj` to a dictionary if possible.
fn as_dict<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a lopdf::Dictionary> {
    resolve(doc, obj)?.as_dict().ok()
}

/// Decode a PDF string object to a `String`. Returns `None` for
/// non-strings and undecodable payloads (e.g. encrypted bytes).
fn decode_string(doc: &Document, obj: &Object) -> Option<String> {
    let resolved = resolve(doc, obj)?;
    decode_text_string(resolved).ok()
}

/// Truncate `s` to [`MAX_SNIPPET_CHARS`] characters (char-boundary
/// safe).
fn truncate_snippet(s: &str) -> String {
    s.chars().take(MAX_SNIPPET_CHARS).collect()
}

/// If `action_obj` (resolved) is a JavaScript action dictionary, push
/// a [`JsInfo`] with the given location and optional name.
fn report_action(
    doc: &Document,
    action_obj: &Object,
    location: &str,
    name: Option<String>,
    out: &mut Vec<JsInfo>,
) {
    let Some(dict) = as_dict(doc, action_obj) else {
        return;
    };
    let is_js = dict
        .get(b"S")
        .ok()
        .and_then(|s| resolve(doc, s))
        .and_then(|o| o.as_name().ok())
        == Some(b"JavaScript");
    if !is_js {
        return;
    }
    let snippet = dict
        .get(b"JS")
        .ok()
        .and_then(|js| decode_string(doc, js))
        .map(|s| truncate_snippet(&s))
        .unwrap_or_default();
    out.push(JsInfo {
        location: location.to_string(),
        name,
        action_type: "JavaScript".to_string(),
        snippet,
    });
}

/// Iterate every action inside an `/A`-style value: a single action
/// dictionary, an array of action dictionaries, or a reference to
/// either. Each action is reported at `location` with an index suffix
/// when it comes from an array.
fn report_actions(
    doc: &Document,
    value: &Object,
    location: &str,
    name: Option<String>,
    out: &mut Vec<JsInfo>,
) {
    let resolved = match resolve(doc, value) {
        Some(o) => o,
        None => return,
    };
    match resolved {
        Object::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                report_action(doc, item, &format!("{location}[{i}]"), name.clone(), out);
            }
        }
        _ => report_action(doc, resolved, location, name, out),
    }
}

/// Scan every value of an `/AA` additional-actions dictionary at
/// `location`: each key (`/WC`, `/WS`, `/E`, ...) maps to an action
/// or an array of actions.
fn report_aa_table(doc: &Document, owner: &Object, location: &str, out: &mut Vec<JsInfo>) {
    let Some(aa) = as_dict(doc, owner).and_then(|d| d.get(b"AA").ok()) else {
        return;
    };
    let Some(aa_dict) = as_dict(doc, aa) else {
        return;
    };
    for (key, value) in aa_dict.iter() {
        let key_str = String::from_utf8_lossy(key);
        report_actions(doc, value, &format!("{location} /AA /{key_str}"), None, out);
    }
}

/// Catalog-level carriers: `/OpenAction`, `/AA`.
fn scan_catalog(doc: &Document, out: &mut Vec<JsInfo>) {
    let Ok(root) = doc.trailer.get(b"Root").and_then(Object::as_reference) else {
        return;
    };
    let Ok(catalog_obj) = doc.get_object(root) else {
        return;
    };

    if let Ok(open_action) = catalog_obj.as_dict().and_then(|d| d.get(b"OpenAction")) {
        report_actions(doc, open_action, "catalog /OpenAction", None, out);
    }
    report_aa_table(doc, catalog_obj, "catalog", out);
}

/// Walk a name-tree dictionary's `/Names` array (and `/Kids` limits —
/// the JavaScript name tree is almost always a flat leaf). Reports
/// each name→action binding where the value is a JavaScript action.
fn scan_names_array(doc: &Document, tree: &Object, out: &mut Vec<JsInfo>) {
    let Some(tree_dict) = as_dict(doc, tree) else {
        return;
    };
    let Some(names) = tree_dict.get(b"Names").ok().and_then(|o| resolve(doc, o)) else {
        return;
    };
    let Some(items) = names.as_array().ok() else {
        return;
    };
    // /Names is a flat [key, value, key, value, ...] array.
    for pair in items.chunks_exact(2) {
        let (key_obj, value_obj) = (&pair[0], &pair[1]);
        let name = decode_string(doc, key_obj);
        // Only inspect values that look like actions; cheap pre-filter
        // keeps benign name→destination pairs out of the report path.
        if let Some(dict) = as_dict(doc, value_obj)
            && dict.get(b"S").is_ok()
        {
            let location = match &name {
                Some(n) => format!("names /JavaScript '{n}'"),
                None => "names /JavaScript".to_string(),
            };
            report_action(doc, value_obj, &location, name, out);
        }
    }
}

/// Catalog `/Names` → `/JavaScript` name tree.
fn scan_name_tree(doc: &Document, out: &mut Vec<JsInfo>) {
    let Ok(root) = doc.trailer.get(b"Root").and_then(Object::as_reference) else {
        return;
    };
    let Ok(catalog_obj) = doc.get_object(root) else {
        return;
    };
    let Ok(names_obj) = catalog_obj
        .as_dict()
        .and_then(|d| d.get(b"Names"))
        .map(|o| o.clone())
    else {
        return;
    };
    let Some(names_dict) = as_dict(doc, &names_obj) else {
        return;
    };
    let Ok(js_tree) = names_dict.get(b"JavaScript") else {
        return;
    };
    scan_names_array(doc, js_tree, out);
}

/// Every page's `/AA` plus each annotation's `/AA` and `/A`.
fn scan_pages(doc: &Document, out: &mut Vec<JsInfo>) {
    let pages = doc.get_pages();
    for (page_num, page_id) in pages {
        let Ok(page_obj) = doc.get_object(page_id) else {
            continue;
        };
        report_aa_table(doc, page_obj, &format!("page {page_num}"), out);

        let Some(annots) = page_obj
            .as_dict()
            .ok()
            .and_then(|d| d.get(b"Annots").ok())
            .and_then(|o| resolve(doc, o))
            .and_then(|o| o.as_array().ok())
        else {
            continue;
        };
        for (i, annot_ref) in annots.iter().enumerate() {
            let Some(annot_obj) = resolve(doc, annot_ref) else {
                continue;
            };
            let base = format!("page {page_num} /Annots[{i}]");
            report_aa_table(doc, annot_obj, &base, out);
            if let Some(dict) = as_dict(doc, annot_obj)
                && let Ok(action) = dict.get(b"A")
            {
                report_actions(doc, action, &format!("{base} /A"), None, out);
            }
        }
    }
}

/// Walk the outline item tree (`/Outlines` → `/First` → `/Next`/`/First`
/// children) and report each item's `/A` action. A visited-set guards
/// against cyclic `/Next` chains in hostile files.
fn scan_outline(doc: &Document, out: &mut Vec<JsInfo>) {
    let Ok(root) = doc.trailer.get(b"Root").and_then(Object::as_reference) else {
        return;
    };
    let Ok(catalog_obj) = doc.get_object(root) else {
        return;
    };
    let Ok(outlines) = catalog_obj.as_dict().and_then(|d| d.get(b"Outlines")) else {
        return;
    };
    let Some(first) = as_dict(doc, outlines)
        .and_then(|d| d.get(b"First").ok())
        .and_then(|o| resolve(doc, o))
        .and_then(|r| r.as_reference().ok())
    else {
        return;
    };

    let mut visited = std::collections::HashSet::new();
    let mut stack = vec![first];
    while let Some(id) = stack.pop() {
        if !visited.insert(id) {
            continue;
        }
        let Ok(item_obj) = doc.get_object(id) else {
            continue;
        };
        let Some(item_dict) = as_dict(doc, item_obj) else {
            continue;
        };

        let title = item_dict
            .get(b"Title")
            .ok()
            .and_then(|t| decode_string(doc, t))
            .unwrap_or_default();
        let base = format!("outline '{title}'");

        if let Ok(action) = item_dict.get(b"A") {
            report_actions(doc, action, &format!("{base} /A"), None, out);
        }

        // Push children then next sibling; order in the report is not
        // guaranteed to match document order, which is fine for an
        // inventory.
        if let Ok(child) = item_dict.get(b"First").and_then(|o| o.as_reference()) {
            stack.push(child);
        }
        if let Ok(next) = item_dict.get(b"Next").and_then(|o| o.as_reference()) {
            stack.push(next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::{Document, Object, dictionary};

    /// Build a PDF with JavaScript in every scanned location:
    /// catalog OpenAction + AA, Names/JavaScript tree, page AA, an
    /// annotation /A + /AA, and an outline item /A.
    fn pdf_with_javascript() -> Vec<u8> {
        let mut doc = Document::load_mem(&pdf_with_pages(&["click me"])).unwrap();

        let js = |code: &str| {
            dictionary! {
                "S" => "JavaScript",
                "JS" => Object::string_literal(code),
            }
        };

        let named_js_id = doc.add_object(js("app.alert('named')"));

        // Catalog: OpenAction, AA, Names.
        let root_id = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        {
            let catalog = doc.get_dictionary_mut(root_id).unwrap();
            catalog.set("OpenAction", js("app.alert('open')"));
            catalog.set("AA", dictionary! { "WC" => js("app.alert('wc')") });
            catalog.set(
                "Names",
                dictionary! {
                    "JavaScript" => dictionary! {
                        "Names" => Object::Array(vec![
                            Object::string_literal("a"),
                            Object::Reference(named_js_id),
                        ]),
                    },
                },
            );
        }

        // Page AA + annotation with /A and /AA.
        let (_, page_id) = doc
            .get_pages()
            .iter()
            .next()
            .map(|(n, i)| (*n, *i))
            .unwrap();
        {
            let page = doc.get_dictionary_mut(page_id).unwrap();
            page.set("AA", dictionary! { "O" => js("app.alert('page')") });
        }
        let annot_id = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Link",
            "Rect" => vec![0.into(), 0.into(), 10.into(), 10.into()],
            "A" => js("app.alert('annot')"),
            "AA" => dictionary! { "E" => js("app.alert('annotE')") },
        });
        doc.get_dictionary_mut(page_id)
            .unwrap()
            .set("Annots", Object::Array(vec![Object::Reference(annot_id)]));

        // Outline item with a JavaScript action.
        let outline_id = doc.add_object(dictionary! {
            "Title" => Object::string_literal("bad bookmark"),
            "Parent" => Object::Null,
            "A" => js("app.alert('outline')"),
        });
        doc.get_dictionary_mut(root_id).unwrap().set(
            "Outlines",
            dictionary! {
                "Type" => "Outlines",
                "First" => Object::Reference(outline_id),
                "Last" => Object::Reference(outline_id),
                "Count" => 1,
            },
        );

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    #[test]
    fn finds_javascript_everywhere() {
        let pdf = pdf_with_javascript();
        let infos = inspect_javascript(&pdf, &EngineLimits::default()).unwrap();

        let locations: Vec<&str> = infos.iter().map(|i| i.location.as_str()).collect();
        assert!(locations.contains(&"catalog /OpenAction"), "{locations:?}");
        assert!(
            locations.iter().any(|l| l.starts_with("catalog /AA /WC")),
            "{locations:?}"
        );
        assert!(
            locations.iter().any(|l| l.contains("/JavaScript 'a'")),
            "{locations:?}"
        );
        assert!(locations.iter().any(|l| l.starts_with("page 1 /AA /O")));
        assert!(locations.iter().any(|l| l.contains("/Annots[0] /A")));
        assert!(locations.iter().any(|l| l.contains("/Annots[0] /AA /E")));
        assert!(
            locations
                .iter()
                .any(|l| l.contains("outline 'bad bookmark' /A"))
        );

        // Named tree entry carries its registered name.
        let named = infos
            .iter()
            .find(|i| i.name.as_deref() == Some("a"))
            .unwrap();
        assert!(named.snippet.contains("named"));

        // Every report is a JavaScript action.
        assert!(infos.iter().all(|i| i.action_type == "JavaScript"));
    }

    #[test]
    fn clean_pdf_reports_nothing() {
        let pdf = pdf_with_pages(&["hello"]);
        let infos = inspect_javascript(&pdf, &EngineLimits::default()).unwrap();
        assert!(infos.is_empty());
    }

    #[test]
    fn benign_actions_not_reported() {
        // A /GoTo annotation action must not be reported.
        let mut doc = Document::load_mem(&pdf_with_pages(&["x"])).unwrap();
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
                "S" => "GoTo",
                "D" => vec![1.into(), "Fit".into()],
            },
        });
        doc.get_dictionary_mut(page_id)
            .unwrap()
            .set("Annots", Object::Array(vec![Object::Reference(annot_id)]));
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();

        assert!(
            inspect_javascript(&buf, &EngineLimits::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(inspect_javascript(b"junk", &EngineLimits::default()).is_err());
    }

    #[test]
    fn snippet_truncated() {
        let mut doc = Document::load_mem(&pdf_with_pages(&["x"])).unwrap();
        let long = "a".repeat(1000);
        let root_id = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        doc.get_dictionary_mut(root_id).unwrap().set(
            "OpenAction",
            dictionary! {
                "S" => "JavaScript",
                "JS" => Object::string_literal(long.clone()),
            },
        );
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();

        let infos = inspect_javascript(&buf, &EngineLimits::default()).unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].snippet.chars().count(), MAX_SNIPPET_CHARS);
    }
}
