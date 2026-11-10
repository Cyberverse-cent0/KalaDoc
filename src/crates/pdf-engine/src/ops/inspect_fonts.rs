//! Read-only inventory of the fonts a PDF's pages reference.
//!
//! Each page's effective `/Resources` (inherited from ancestor `/Pages`
//! nodes when the page does not define its own) is scanned for a
//! `/Font` dictionary. Every entry is dereferenced and reported once —
//! fonts shared between pages are merged into a single [`FontInfo`]
//! whose `pages` list records every page that references them. Dedup
//! is by resolved object id; font dictionaries embedded directly in a
//! resource dictionary (no object id) are deduplicated by their
//! descriptive tuple instead.
//!
//! One level of **Form XObject** resources is also walked: each
//! `/XObject` entry that resolves to a stream with `/Subtype /Form`
//! contributes the fonts in its own `/Resources` to the same page.
//! Nested Form XObjects (a Form inside a Form) are not descended
//! into.
//!
//! For each font the op reports:
//!
//! - `name` — `/BaseFont` (including any `AAAAAA+` subset prefix, kept
//!   verbatim), falling back to the resource name (`/F1`, ...) when
//!   `/BaseFont` is absent.
//! - `subtype` — `/Subtype` (`/Type1`, `/TrueType`, `/Type0`, ...),
//!   empty string when absent.
//! - `encoding` — `/Encoding` rendered as a name (`"WinAnsiEncoding"`)
//!   or, for dictionary encodings, the `/BaseEncoding` name (or
//!   `"Differences"` when only a differences array is present).
//! - `embedded` — `true` when a `/FontDescriptor` (on the font itself
//!   or on any `/DescendantFonts` descendant) carries `/FontFile`,
//!   `/FontFile2` or `/FontFile3`.
//!
//! Like [`super::info::inspect`], encrypted documents are tolerated:
//! names and dictionaries remain readable, so the inventory is still
//! produced for structure lopdf can parse. Nothing is decrypted,
//! executed or modified.

use std::collections::{HashMap, HashSet};

use lopdf::{Document, Object, ObjectId};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::load_pdf;

/// Cap on `/Parent` climbing when resolving inherited `/Resources`.
const MAX_INHERIT_DEPTH: usize = 64;
/// Safety cap on distinct fonts reported per document — the scan
/// truncates rather than fails on pathological files.
const MAX_FONTS: usize = 4096;
/// Safety cap on Form XObject resource lookups across the document.
const MAX_FORM_XOBJECTS: usize = 8192;
/// Cap on `/DescendantFonts` recursion when checking for embedded
/// font programs.
const MAX_DESCENDANT_DEPTH: usize = 16;

/// A font referenced by one or more pages' resource dictionaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FontInfo {
    /// `/BaseFont` name (subset prefix kept), or the resource name
    /// when `/BaseFont` is absent.
    pub name: String,
    /// `/Subtype` name (`"Type1"`, `"TrueType"`, `"Type0"`, ...);
    /// empty string when the font dictionary does not declare one.
    pub subtype: String,
    /// `/Encoding` as a display string — a name like
    /// `"WinAnsiEncoding"`, a `/BaseEncoding` name for dictionary
    /// encodings, or `"Differences"`. `None` when absent or
    /// undecodable (e.g. encrypted strings).
    pub encoding: Option<String>,
    /// Whether a font program stream (`/FontFile`, `/FontFile2` or
    /// `/FontFile3`) is declared on the font's descriptor or on any
    /// descendant font's descriptor.
    pub embedded: bool,
    /// Sorted, deduplicated 1-based page numbers whose resources
    /// (directly or via a Form XObject) reference this font.
    pub pages: Vec<u32>,
}

/// Dedup key for a font dictionary: the resolved object id when the
/// font is an indirect object, or its descriptive tuple when it is a
/// direct dictionary (which has no identity of its own).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FontKey {
    Id(ObjectId),
    Inline(String, String, Option<String>),
}

/// Inventory every font referenced by `input`'s pages.
///
/// Input-side limits (`max_input_bytes`, `max_pages`) are enforced by
/// [`load_pdf`]. Encrypted documents are tolerated like
/// [`super::info::inspect`]: structure is still reported.
pub fn inspect_fonts(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<FontInfo>> {
    let doc = load_pdf(input, limits)?;
    let pages = doc.get_pages();

    let mut fonts: Vec<FontInfo> = Vec::new();
    let mut index_of: HashMap<FontKey, usize> = HashMap::new();
    let mut form_xobject_visits = 0usize;

    for (page_number, page_id) in &pages {
        let Some(resources) = inherited_dict(&doc, *page_id, b"Resources") else {
            continue;
        };
        collect_resource_fonts(&doc, resources, *page_number, &mut fonts, &mut index_of);
        collect_form_xobject_fonts(
            &doc,
            resources,
            *page_number,
            &mut fonts,
            &mut index_of,
            &mut form_xobject_visits,
        );
    }

    for font in &mut fonts {
        font.pages.sort_unstable();
        font.pages.dedup();
    }
    Ok(fonts)
}

/// Scan one `/Resources` dictionary's `/Font` sub-dictionary,
/// recording each font against `page_number`.
fn collect_resource_fonts(
    doc: &Document,
    resources: &lopdf::Dictionary,
    page_number: u32,
    fonts: &mut Vec<FontInfo>,
    index_of: &mut HashMap<FontKey, usize>,
) {
    let Ok(fonts_obj) = resources.get_deref(b"Font", doc) else {
        return;
    };
    let Ok(font_dict) = fonts_obj.as_dict() else {
        return;
    };
    for (resource_name, font_obj) in font_dict.iter() {
        let Ok((object_id, object)) = doc.dereference(font_obj) else {
            continue;
        };
        let Ok(dict) = object.as_dict() else {
            continue;
        };
        let info = font_info(doc, dict, resource_name);
        let key = match object_id {
            Some(id) => FontKey::Id(id),
            None => FontKey::Inline(
                info.name.clone(),
                info.subtype.clone(),
                info.encoding.clone(),
            ),
        };
        match index_of.get(&key) {
            Some(&idx) => {
                let pages = &mut fonts[idx].pages;
                if pages.last() != Some(&page_number) {
                    pages.push(page_number);
                }
            }
            None => {
                if fonts.len() >= MAX_FONTS {
                    continue;
                }
                index_of.insert(key, fonts.len());
                fonts.push(FontInfo {
                    pages: vec![page_number],
                    ..info
                });
            }
        }
    }
}

/// Scan a `/Resources` dictionary's `/XObject` sub-dictionary for Form
/// XObjects and collect the fonts in each Form's own `/Resources`.
/// One level only — Forms nested inside Forms are not descended.
fn collect_form_xobject_fonts(
    doc: &Document,
    resources: &lopdf::Dictionary,
    page_number: u32,
    fonts: &mut Vec<FontInfo>,
    index_of: &mut HashMap<FontKey, usize>,
    visits: &mut usize,
) {
    let Ok(xobjects_obj) = resources.get_deref(b"XObject", doc) else {
        return;
    };
    let Ok(xobjects) = xobjects_obj.as_dict() else {
        return;
    };
    for (_, xobject) in xobjects.iter() {
        if *visits >= MAX_FORM_XOBJECTS {
            return;
        }
        *visits += 1;
        let Ok((_, object)) = doc.dereference(xobject) else {
            continue;
        };
        let Ok(stream) = object.as_stream() else {
            continue;
        };
        let is_form = stream
            .dict
            .get(b"Subtype")
            .ok()
            .and_then(|o| doc.dereference(o).ok())
            .and_then(|(_, o)| o.as_name().ok())
            .is_some_and(|name| name == b"Form");
        if !is_form {
            continue;
        }
        let Ok(form_resources) = stream.dict.get_deref(b"Resources", doc) else {
            continue;
        };
        let Ok(form_resources) = form_resources.as_dict() else {
            continue;
        };
        collect_resource_fonts(doc, form_resources, page_number, fonts, index_of);
    }
}

/// Build a [`FontInfo`] (without `pages`) from a dereferenced font
/// dictionary and its resource name.
fn font_info(doc: &Document, dict: &lopdf::Dictionary, resource_name: &[u8]) -> FontInfo {
    let name = dict
        .get(b"BaseFont")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_name().ok())
        .map(|n| String::from_utf8_lossy(n).into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| String::from_utf8_lossy(resource_name).into_owned());

    let subtype = dict
        .get(b"Subtype")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_name().ok())
        .map(|n| String::from_utf8_lossy(n).into_owned())
        .unwrap_or_default();

    let encoding = dict
        .get(b"Encoding")
        .ok()
        .and_then(|o| encoding_string(doc, o));

    let embedded = font_embedded(doc, dict, 0, &mut HashSet::new());

    FontInfo {
        name,
        subtype,
        encoding,
        embedded,
        pages: Vec::new(),
    }
}

/// Render an `/Encoding` value as a display string: a name object
/// yields the name itself; a dictionary yields its `/BaseEncoding`
/// name, or `"Differences"` when only a differences array is present.
fn encoding_string(doc: &Document, value: &Object) -> Option<String> {
    let (_, object) = doc.dereference(value).ok()?;
    match object {
        Object::Name(name) => Some(String::from_utf8_lossy(name).into_owned()),
        Object::Dictionary(dict) => dict
            .get(b"BaseEncoding")
            .ok()
            .and_then(|o| doc.dereference(o).ok())
            .and_then(|(_, o)| o.as_name().ok())
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .or_else(|| {
                dict.get(b"Differences")
                    .ok()
                    .map(|_| "Differences".to_string())
            }),
        _ => None,
    }
}

/// Whether the font dictionary (or any descendant) declares an
/// embedded font program via `/FontDescriptor` → `/FontFile`,
/// `/FontFile2` or `/FontFile3`. Descendant traversal is bounded by
/// [`MAX_DESCENDANT_DEPTH`] and a visited set so cyclic
/// `/DescendantFonts` chains cannot loop.
fn font_embedded(
    doc: &Document,
    dict: &lopdf::Dictionary,
    depth: usize,
    visited: &mut HashSet<ObjectId>,
) -> bool {
    if let Ok(descriptor_obj) = dict.get_deref(b"FontDescriptor", doc)
        && let Ok(descriptor) = descriptor_obj.as_dict()
        && [
            b"FontFile".as_slice(),
            b"FontFile2".as_slice(),
            b"FontFile3".as_slice(),
        ]
        .iter()
        // A font program is a stream object; a bare key (e.g. `null`)
        // does not make the font embedded.
        .any(|key| {
            descriptor
                .get(key)
                .ok()
                .and_then(|o| doc.dereference(o).ok())
                .is_some_and(|(_, o)| o.as_stream().is_ok())
        })
    {
        return true;
    }

    if depth >= MAX_DESCENDANT_DEPTH {
        return false;
    }
    let Ok(descendants_obj) = dict.get_deref(b"DescendantFonts", doc) else {
        return false;
    };
    let Ok(descendants) = descendants_obj.as_array() else {
        return false;
    };
    for descendant in descendants {
        let Ok((id, object)) = doc.dereference(descendant) else {
            continue;
        };
        // Skip already-visited indirect objects to bound cyclic graphs.
        if let Some(id) = id
            && !visited.insert(id)
        {
            continue;
        }
        let Ok(child) = object.as_dict() else {
            continue;
        };
        if font_embedded(doc, child, depth + 1, visited) {
            return true;
        }
    }
    false
}

/// Resolve an inherited dictionary entry (`/Resources`, ...) starting
/// at `page_id` and climbing `/Parent` links. Same algorithm as
/// `extract_images`' copy — returns the first dictionary found, or
/// `None` when no ancestor provides it.
fn inherited_dict<'a>(
    doc: &'a Document,
    page_id: ObjectId,
    key: &[u8],
) -> Option<&'a lopdf::Dictionary> {
    let mut current_id = Some(page_id);
    let mut direct: Option<&'a Object> = None;
    for _ in 0..MAX_INHERIT_DEPTH {
        let dict: &lopdf::Dictionary = match (current_id, direct) {
            (Some(id), _) => doc.get_dictionary(id).ok()?,
            (None, Some(obj)) => obj.as_dict().ok()?,
            (None, None) => return None,
        };
        if let Ok(obj) = dict.get_deref(key, doc)
            && let Ok(found) = obj.as_dict()
        {
            return Some(found);
        }
        let parent = dict.get(b"Parent").ok()?;
        let (parent_id, parent_obj) = doc.dereference(parent).ok()?;
        match parent_id {
            Some(id) => {
                current_id = Some(id);
                direct = None;
            }
            None => {
                current_id = None;
                direct = Some(parent_obj);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;
    use lopdf::{Document, Stream, dictionary};

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    /// The base fixture's font: `/Courier` Type1, no encoding, not
    /// embedded, referenced by every page.
    #[test]
    fn lists_shared_font_with_all_pages() {
        let pdf = pdf_with_pages(&["a", "b", "c"]);
        let fonts = inspect_fonts(&pdf, &limits()).unwrap();
        assert_eq!(fonts.len(), 1);
        let f = &fonts[0];
        assert_eq!(f.name, "Courier");
        assert_eq!(f.subtype, "Type1");
        assert_eq!(f.encoding, None);
        assert!(!f.embedded);
        assert_eq!(f.pages, vec![1, 2, 3]);
    }

    /// Encoding names and `/FontDescriptor` `/FontFile2` mark the
    /// font embedded.
    #[test]
    fn detects_encoding_and_embedded_descriptor() {
        let mut doc = Document::load_mem(&pdf_with_pages(&["x"])).unwrap();
        let font_id = doc
            .get_pages()
            .values()
            .next()
            .and_then(|page_id| {
                let page = doc.get_dictionary(*page_id).ok()?;
                let res = page.get_deref(b"Resources", &doc).ok()?;
                let res = res.as_dict().ok()?;
                let font = res.get_deref(b"Font", &doc).ok()?;
                let font = font.as_dict().ok()?;
                font.get(b"F1").ok()?.as_reference().ok()
            })
            .unwrap();

        // The font program must be an indirect stream object — a
        // stream nested in a dictionary is not valid PDF.
        let font_file_id = doc.add_object(Stream::new(dictionary! {}, vec![1u8, 2, 3]));
        let descriptor_id = doc.add_object(dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => "Courier",
            "Flags" => 4i64,
            "FontBBox" => vec![0.into(), 0.into(), 0.into(), 0.into()],
            "ItalicAngle" => 0,
            "Ascent" => 0,
            "Descent" => 0,
            "CapHeight" => 0,
            "StemV" => 0,
            "FontFile2" => Object::Reference(font_file_id),
        });
        {
            let font = doc.get_dictionary_mut(font_id).unwrap();
            font.set("Encoding", "WinAnsiEncoding");
            font.set("FontDescriptor", Object::Reference(descriptor_id));
        }

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        let fonts = inspect_fonts(&buf, &limits()).unwrap();
        assert_eq!(fonts.len(), 1);
        assert_eq!(fonts[0].encoding.as_deref(), Some("WinAnsiEncoding"));
        assert!(fonts[0].embedded);
    }

    /// A Type0 font embeds via a `/DescendantFonts` descendant's
    /// `/FontFile3`.
    #[test]
    fn detects_embedded_via_descendant_fonts() {
        let mut doc = Document::load_mem(&pdf_with_pages(&["x"])).unwrap();

        // Indirect stream object, as a real font program would be.
        let font_file_id = doc.add_object(Stream::new(dictionary! {}, vec![9u8]));
        let descendant_descriptor = doc.add_object(dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => "CIDFont",
            "Flags" => 4i64,
            "FontBBox" => vec![0.into(), 0.into(), 0.into(), 0.into()],
            "ItalicAngle" => 0,
            "Ascent" => 0,
            "Descent" => 0,
            "CapHeight" => 0,
            "StemV" => 0,
            "FontFile3" => Object::Reference(font_file_id),
        });
        let descendant_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "CIDFontType2",
            "BaseFont" => "AAAAAA+CIDFont",
            "CIDSystemInfo" => dictionary! {
                "Registry" => Object::string_literal("Adobe"),
                "Ordering" => Object::string_literal("Identity"),
                "Supplement" => 0,
            },
            "FontDescriptor" => Object::Reference(descendant_descriptor),
        });
        let type0_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type0",
            "BaseFont" => "AAAAAA+CIDFont",
            "Encoding" => "Identity-H",
            "DescendantFonts" => vec![Object::Reference(descendant_id)],
        });

        // Point the page's F1 resource at the Type0 font.
        let (_, page_id) = doc
            .get_pages()
            .iter()
            .next()
            .map(|(n, i)| (*n, *i))
            .unwrap();
        let res_id = {
            let page = doc.get_dictionary(page_id).unwrap();
            page.get(b"Resources").unwrap().as_reference().unwrap()
        };
        doc.get_dictionary_mut(res_id)
            .unwrap()
            .set("Font", dictionary! { "F1" => type0_id });

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        let fonts = inspect_fonts(&buf, &limits()).unwrap();
        assert_eq!(fonts.len(), 1);
        let f = &fonts[0];
        assert_eq!(f.name, "AAAAAA+CIDFont");
        assert_eq!(f.subtype, "Type0");
        assert_eq!(f.encoding.as_deref(), Some("Identity-H"));
        assert!(f.embedded);
        assert_eq!(f.pages, vec![1]);
    }

    /// Fonts inside a Form XObject's own `/Resources` are attributed
    /// to the page that draws the Form.
    #[test]
    fn collects_fonts_from_form_xobjects() {
        let mut doc = Document::load_mem(&pdf_with_pages(&["x"])).unwrap();

        let form_font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let form_resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F2" => form_font_id },
        });
        let form_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
                "Resources" => Object::Reference(form_resources_id),
            },
            b"q Q".to_vec(),
        ));

        let (_, page_id) = doc
            .get_pages()
            .iter()
            .next()
            .map(|(n, i)| (*n, *i))
            .unwrap();
        let res_id = {
            let page = doc.get_dictionary(page_id).unwrap();
            page.get(b"Resources").unwrap().as_reference().unwrap()
        };
        {
            let res = doc.get_dictionary_mut(res_id).unwrap();
            res.set("XObject", dictionary! { "Fm1" => form_id });
        }

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        let fonts = inspect_fonts(&buf, &limits()).unwrap();
        let names: Vec<&str> = fonts.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"Courier"), "{names:?}");
        let form_font = fonts.iter().find(|f| f.name == "Helvetica").unwrap();
        assert_eq!(form_font.subtype, "Type1");
        assert_eq!(form_font.pages, vec![1]);
    }

    /// `/Resources` removed from the page is still found via the
    /// `/Pages` ancestor.
    #[test]
    fn inherits_resources_from_pages_node() {
        let mut doc = Document::load_mem(&pdf_with_pages(&["x"])).unwrap();
        let (_, page_id) = doc
            .get_pages()
            .iter()
            .next()
            .map(|(n, i)| (*n, *i))
            .unwrap();
        doc.get_dictionary_mut(page_id)
            .unwrap()
            .remove(b"Resources");

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        let fonts = inspect_fonts(&buf, &limits()).unwrap();
        assert_eq!(fonts.len(), 1);
        assert_eq!(fonts[0].name, "Courier");
        assert_eq!(fonts[0].pages, vec![1]);
    }

    /// Non-font XObjects and non-dictionary font entries are skipped
    /// without failing.
    #[test]
    fn tolerates_malformed_entries() {
        let mut doc = Document::load_mem(&pdf_with_pages(&["x"])).unwrap();
        let (_, page_id) = doc
            .get_pages()
            .iter()
            .next()
            .map(|(n, i)| (*n, *i))
            .unwrap();
        let res_id = {
            let page = doc.get_dictionary(page_id).unwrap();
            page.get(b"Resources").unwrap().as_reference().unwrap()
        };
        let new_font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Courier",
        });
        let image_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => 1i64,
                "Height" => 1i64,
                "BitsPerComponent" => 8i64,
                "ColorSpace" => "DeviceGray",
            },
            vec![0u8],
        ));
        {
            let res = doc.get_dictionary_mut(res_id).unwrap();
            res.set(
                "Font",
                dictionary! {
                    "F1" => new_font_id,
                    "Bad" => Object::Null,
                    "Worse" => Object::string_literal("not a font"),
                },
            );
            // An image XObject must not be mistaken for a Form.
            res.set("XObject", dictionary! { "Im1" => image_id });
        }

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        let fonts = inspect_fonts(&buf, &limits()).unwrap();
        assert_eq!(fonts.len(), 1);
        assert_eq!(fonts[0].name, "Courier");
    }

    #[test]
    fn rejects_garbage() {
        assert!(inspect_fonts(b"junk", &limits()).is_err());
    }
}
