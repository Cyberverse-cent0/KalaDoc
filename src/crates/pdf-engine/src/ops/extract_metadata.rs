//! Rich document metadata extraction.
//!
//! [`inspect`](super::info::inspect) returns a small summary; this op
//! digs deeper into everything a PDF can carry about itself:
//!
//! - The trailer `/Info` dictionary (title, author, subject, keywords,
//!   creator, producer) plus `/CreationDate`/`/ModDate`, parsed from
//!   the PDF date grammar `D:YYYYMMDDHHmmSSOHH'mm'` into a normalized
//!   ISO-8601-ish string ([`PdfDate`]).
//! - The catalog `/Metadata` XMP stream — a *basic* extraction of the
//!   common `dc:`/`pdf:`/`xmp:` fields via tag search, plus the raw
//!   (capped) packet text. This is not a full XMP/RDF parser; unusual
//!   but valid RDF layouts may be missed.
//! - `/PageLabels` number trees, rendered per page (decimal, Roman,
//!   or alphabetic styles with `/P` prefixes).
//! - `/Names` → `/EmbeddedFiles` entry names.
//! - `/AcroForm` `/Fields`, flattened to dotted field names.
//! - Top-level outline (`/Outlines`) item titles.
//! - First page's `/MediaBox` in points (inherited from `/Pages`
//!   ancestors when necessary).
//!
//! Like `inspect`, encrypted documents are tolerated: structure-level
//! fields (page count, encryption flag, size, page geometry) are still
//! reported, but every string-bearing section is skipped because
//! encrypted strings cannot be decoded without a password.

use std::collections::HashSet;

use lopdf::{Document, Object, decode_text_string};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::{doc_is_encrypted, load_pdf};

/// A PDF date string with its normalized form.
///
/// `raw` is the original string as found in the document (the `D:`
/// prefix kept). `parsed` is `"YYYY-MM-DDTHH:MM:SS"` plus a timezone
/// suffix when the source carried one (`Z`, `+HH:MM`, `-HH:MM`);
/// `None` when the string did not match the expected grammar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PdfDate {
    /// Original date string from the document.
    pub raw: String,
    /// Normalized `YYYY-MM-DDTHH:MM:SS[Z|±HH:MM]` form, if parseable.
    pub parsed: Option<String>,
}

/// Basic fields lifted from the catalog `/Metadata` XMP stream.
///
/// Only the common Dublin Core / PDF / XMP properties are extracted;
/// this is a tag-level scan, not a conforming XMP parser.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct XmpMetadata {
    /// `dc:title` (first `rdf:li` when present).
    pub title: Option<String>,
    /// `dc:creator` entries.
    pub creators: Vec<String>,
    /// `dc:description` (first `rdf:li` when present).
    pub description: Option<String>,
    /// `dc:subject` entries joined with `"; "`.
    pub subject: Option<String>,
    /// `pdf:Keywords`.
    pub keywords: Option<String>,
    /// `pdf:Producer`.
    pub producer: Option<String>,
    /// `xmp:CreatorTool`.
    pub creator_tool: Option<String>,
    /// `xmp:CreateDate`.
    pub create_date: Option<String>,
    /// `xmp:ModifyDate`.
    pub modify_date: Option<String>,
}

/// Rich metadata for a PDF document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PdfMetadata {
    /// Number of pages in the document's page tree.
    pub page_count: u32,
    /// Whether the trailer carries an `/Encrypt` entry. When `true`,
    /// every string-bearing field below is empty/`None`.
    pub encrypted: bool,
    /// Size of the input buffer in bytes.
    pub file_size_bytes: u64,
    /// `/Title` from the document information dictionary.
    pub title: Option<String>,
    /// `/Author`.
    pub author: Option<String>,
    /// `/Subject`.
    pub subject: Option<String>,
    /// `/Keywords` (raw string as stored).
    pub keywords: Option<String>,
    /// `/Creator`.
    pub creator: Option<String>,
    /// `/Producer`.
    pub producer: Option<String>,
    /// `/CreationDate`, parsed when it matches the PDF date grammar.
    pub creation_date: Option<PdfDate>,
    /// `/ModDate`, parsed when it matches the PDF date grammar.
    pub modification_date: Option<PdfDate>,
    /// Basic fields extracted from the XMP metadata stream.
    pub xmp: Option<XmpMetadata>,
    /// The raw XMP packet as UTF-8-lossy text, capped at 1 MiB
    /// characters — kept alongside the parsed fields for fields this
    /// op does not model.
    pub xmp_raw: Option<String>,
    /// `page_labels[i]` is the label for 1-based page `i + 1`, or
    /// `None` when no `/PageLabels` entry covers that page.
    pub page_labels: Vec<Option<String>>,
    /// File names from the catalog `/Names` → `/EmbeddedFiles` name tree.
    pub embedded_files: Vec<String>,
    /// Flattened AcroForm field names (dotted paths like `form1.name`).
    pub form_fields: Vec<String>,
    /// Titles of top-level outline items, in document order.
    pub outline_titles: Vec<String>,
    /// First page's `/MediaBox` as `[x0, y0, x1, y1]` in points,
    /// inheriting from ancestor `/Pages` nodes when necessary.
    pub page_size_pts: Option<[f64; 4]>,
}

/// Safety caps for pathological documents — metadata sections are
/// best-effort, so hitting a cap truncates rather than fails.
const MAX_LABEL_SEGMENTS: usize = 1024;
const MAX_NAME_ENTRIES: usize = 1024;
const MAX_FORM_FIELDS: usize = 4096;
const MAX_OUTLINE_ITEMS: usize = 1024;
const MAX_FIELD_DEPTH: usize = 32;
const MAX_INHERIT_DEPTH: usize = 64;
const MAX_XMP_BYTES: usize = 8 * 1024 * 1024;
const MAX_XMP_CHARS: usize = 1024 * 1024;

/// Extract rich [`PdfMetadata`] from `input`.
///
/// Encrypted input is tolerated (like [`super::info::inspect`]): the
/// document still loads and structural fields are reported, but
/// string-bearing sections are skipped since encrypted strings cannot
/// be decoded.
pub fn extract_metadata(input: &[u8], limits: &EngineLimits) -> PdfResult<PdfMetadata> {
    let doc = load_pdf(input, limits)?;
    let encrypted = doc_is_encrypted(&doc);
    let page_count = doc.get_pages().len() as u32;

    let pages = doc.get_pages();
    let first_page_id = pages.values().next().copied();

    if encrypted {
        return Ok(PdfMetadata {
            page_count,
            encrypted,
            file_size_bytes: input.len() as u64,
            title: None,
            author: None,
            subject: None,
            keywords: None,
            creator: None,
            producer: None,
            creation_date: None,
            modification_date: None,
            xmp: None,
            xmp_raw: None,
            page_labels: vec![None; page_count as usize],
            embedded_files: Vec::new(),
            form_fields: Vec::new(),
            outline_titles: Vec::new(),
            page_size_pts: first_page_id.and_then(|id| page_mediabox(&doc, id)),
        });
    }

    let info = doc.trailer.get(b"Info").ok().and_then(|info| {
        let (_, object) = doc.dereference(info).ok()?;
        object.as_dict().ok()
    });

    let catalog = doc.catalog().ok();
    let xmp_raw = catalog.and_then(|c| xmp_packet(&doc, c));

    Ok(PdfMetadata {
        page_count,
        encrypted,
        file_size_bytes: input.len() as u64,
        title: info.and_then(|d| info_string(&doc, d, b"Title")),
        author: info.and_then(|d| info_string(&doc, d, b"Author")),
        subject: info.and_then(|d| info_string(&doc, d, b"Subject")),
        keywords: info.and_then(|d| info_string(&doc, d, b"Keywords")),
        creator: info.and_then(|d| info_string(&doc, d, b"Creator")),
        producer: info.and_then(|d| info_string(&doc, d, b"Producer")),
        creation_date: info
            .and_then(|d| d.get(b"CreationDate").ok())
            .and_then(|o| decode_text_string(o).ok())
            .map(|raw| PdfDate {
                parsed: parse_pdf_date(&raw),
                raw,
            }),
        modification_date: info
            .and_then(|d| d.get(b"ModDate").ok())
            .and_then(|o| decode_text_string(o).ok())
            .map(|raw| PdfDate {
                parsed: parse_pdf_date(&raw),
                raw,
            }),
        xmp: xmp_raw.as_deref().and_then(parse_xmp),
        xmp_raw,
        page_labels: catalog
            .map(|c| collect_page_labels(&doc, c, page_count))
            .unwrap_or_else(|| vec![None; page_count as usize]),
        embedded_files: catalog
            .map(|c| collect_embedded_file_names(&doc, c))
            .unwrap_or_default(),
        form_fields: catalog
            .map(|c| collect_form_fields(&doc, c))
            .unwrap_or_default(),
        outline_titles: catalog
            .map(|c| collect_outline_titles(&doc, c))
            .unwrap_or_default(),
        page_size_pts: first_page_id.and_then(|id| page_mediabox(&doc, id)),
    })
}

fn info_string(doc: &Document, dict: &lopdf::Dictionary, key: &[u8]) -> Option<String> {
    // `/Info` entries may be indirect references in unusual files.
    let object = doc.dereference(dict.get(key).ok()?).ok()?.1;
    decode_text_string(object).ok()
}

// ---------------------------------------------------------------------------
// PDF date strings
// ---------------------------------------------------------------------------

/// Parse a PDF date string into `YYYY-MM-DDTHH:MM:SS` plus an optional
/// timezone suffix.
///
/// Grammar (ISO 32000): `D:YYYYMMDDHHmmSSOHH'mm'` where every field
/// after the year is optional and `O` is `Z`, `+` or `-`. Fields
/// missing from the source default to `01` (day) or `00` (time).
/// Returns `None` when the year is absent, non-numeric, or out of
/// range — the raw string is still kept by the caller.
pub(crate) fn parse_pdf_date(raw: &str) -> Option<String> {
    let s = raw.trim();
    let s = s.strip_prefix("D:").or_else(|| s.strip_prefix("d:"))?;
    let bytes = s.as_bytes();
    if bytes.len() < 4 {
        return None;
    }
    let digits = |range: std::ops::Range<usize>| -> Option<u32> {
        let slice = bytes.get(range)?;
        if !slice.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(slice).ok()?.parse().ok()
    };

    let year = digits(0..4)?;
    if !(1..=9999).contains(&year) {
        return None;
    }
    // Optional fields are only consumed when fully present.
    let month = if bytes.len() >= 6 { digits(4..6)? } else { 1 };
    let day = if bytes.len() >= 8 { digits(6..8)? } else { 1 };
    let hour = if bytes.len() >= 10 { digits(8..10)? } else { 0 };
    let minute = if bytes.len() >= 12 {
        digits(10..12)?
    } else {
        0
    };
    let second = if bytes.len() >= 14 {
        digits(12..14)?
    } else {
        0
    };
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    let mut out = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}");

    // Timezone: O HH'mm' — the apostrophe after HH is part of the spec.
    if bytes.len() > 14 {
        match bytes[14] {
            b'Z' | b'z' => out.push('Z'),
            sign @ (b'+' | b'-') => {
                let tz_h = digits(15..17);
                let tz_m = if bytes.len() >= 21 {
                    digits(18..20)
                } else {
                    Some(0)
                };
                if let (Some(h), Some(m)) = (tz_h, tz_m)
                    && h <= 23
                    && m <= 59
                {
                    out.push(sign as char);
                    out.push_str(&format!("{h:02}:{m:02}"));
                }
            }
            _ => {}
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// XMP metadata stream
// ---------------------------------------------------------------------------

/// Decompress the catalog `/Metadata` stream and return its text,
/// capped at [`MAX_XMP_CHARS`] characters.
fn xmp_packet(doc: &Document, catalog: &lopdf::Dictionary) -> Option<String> {
    let object = catalog.get(b"Metadata").ok()?;
    let (_, object) = doc.dereference(object).ok()?;
    let stream = object.as_stream().ok()?;
    let bytes = stream
        .decompressed_content_with_limit(MAX_XMP_BYTES)
        .unwrap_or_else(|_| stream.content.clone());
    let text = String::from_utf8_lossy(&bytes);
    let trimmed = text.trim_start();
    if !trimmed.starts_with('<') {
        return None;
    }
    Some(text.chars().take(MAX_XMP_CHARS).collect())
}

fn parse_xmp(xml: &str) -> Option<XmpMetadata> {
    let parsed = XmpMetadata {
        title: xmp_first_text(xml, "dc:title"),
        creators: xmp_all_items(xml, "dc:creator"),
        description: xmp_first_text(xml, "dc:description"),
        subject: {
            let items = xmp_all_items(xml, "dc:subject");
            if items.is_empty() {
                None
            } else {
                Some(items.join("; "))
            }
        },
        keywords: xmp_plain_text(xml, "pdf:Keywords"),
        producer: xmp_plain_text(xml, "pdf:Producer"),
        creator_tool: xmp_plain_text(xml, "xmp:CreatorTool"),
        create_date: xmp_plain_text(xml, "xmp:CreateDate"),
        modify_date: xmp_plain_text(xml, "xmp:ModifyDate"),
    };
    // An XMP packet with none of the modeled fields is still useful
    // raw — report `None` here rather than an all-empty struct.
    let empty = parsed.title.is_none()
        && parsed.creators.is_empty()
        && parsed.description.is_none()
        && parsed.subject.is_none()
        && parsed.keywords.is_none()
        && parsed.producer.is_none()
        && parsed.creator_tool.is_none()
        && parsed.create_date.is_none()
        && parsed.modify_date.is_none();
    (!empty).then_some(parsed)
}

/// Inner XML of the first `<tag>...</tag>` element.
fn xml_inner<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}");
    let start = xml.find(&open)?;
    let after_open = &xml[start + open.len()..];
    let inner_start = after_open.find('>')? + 1;
    let inner = &after_open[inner_start..];
    let close = format!("</{tag}>");
    let inner_end = inner.find(&close)?;
    Some(&inner[..inner_end])
}

/// Strip XML tags and decode the five predefined entities.
fn xml_text(fragment: &str) -> String {
    let mut out = String::with_capacity(fragment.len());
    let mut rest = fragment;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        match rest[lt..].find('>') {
            Some(gt) => rest = &rest[lt + gt + 1..],
            None => break,
        }
    }
    out.push_str(rest);
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
        .trim()
        .to_string()
}

/// Text of the first `<rdf:li>` inside `inner`, or the stripped inner
/// text when there are no `rdf:li` items.
fn first_item_text(inner: &str) -> Option<String> {
    if let Some(li) = xml_inner(inner, "rdf:li") {
        let text = xml_text(li);
        if !text.is_empty() {
            return Some(text);
        }
    }
    let text = xml_text(inner);
    (!text.is_empty()).then_some(text)
}

fn xmp_first_text(xml: &str, tag: &str) -> Option<String> {
    xml_inner(xml, tag).and_then(first_item_text)
}

/// All `<rdf:li>` texts under `tag`; a single non-li value yields one
/// entry. Returns an empty vec when the element is absent.
fn xmp_all_items(xml: &str, tag: &str) -> Vec<String> {
    let Some(inner) = xml_inner(xml, tag) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    let mut rest = inner;
    while let Some(start) = rest.find("<rdf:li") {
        let after = &rest[start..];
        let Some(gt) = after.find('>') else { break };
        let content = &after[gt + 1..];
        let Some(end) = content.find("</rdf:li>") else {
            break;
        };
        let text = xml_text(&content[..end]);
        if !text.is_empty() {
            items.push(text);
        }
        rest = &content[end + "</rdf:li>".len()..];
    }
    if items.is_empty()
        && let Some(text) = first_item_text(inner)
    {
        items.push(text);
    }
    items
}

/// Plain (non-rdf) element text.
fn xmp_plain_text(xml: &str, tag: &str) -> Option<String> {
    let text = xml_text(xml_inner(xml, tag)?);
    (!text.is_empty()).then_some(text)
}

// ---------------------------------------------------------------------------
// Page labels
// ---------------------------------------------------------------------------

/// One `/Nums` entry of a page-label number tree, resolved to concrete
/// values so no document references are retained.
struct LabelSegment {
    /// 1-based first page the segment covers.
    start_page: u32,
    /// `/P` prefix string.
    prefix: String,
    /// `/S` style: `D`, `R`, `r`, `A`, `a` (default `D`).
    style: u8,
    /// `/St` start number (default 1).
    start_num: i64,
}

fn collect_page_labels(
    doc: &Document,
    catalog: &lopdf::Dictionary,
    page_count: u32,
) -> Vec<Option<String>> {
    let mut out = vec![None; page_count as usize];
    let Ok(labels_obj) = catalog.get(b"PageLabels") else {
        return out;
    };
    let Ok((_, labels)) = doc.dereference(labels_obj) else {
        return out;
    };
    let Ok(node) = labels.as_dict() else {
        return out;
    };

    let mut segments = Vec::new();
    collect_label_segments(doc, node, &mut segments);
    if segments.is_empty() {
        return out;
    }
    segments.sort_by_key(|s| s.start_page);

    for (index, slot) in out.iter_mut().enumerate() {
        let page = index as u32 + 1;
        // Last segment whose start is <= this page wins.
        let segment = segments.iter().take_while(|s| s.start_page <= page).last();
        if let Some(segment) = segment {
            let n = segment.start_num + i64::from(page - segment.start_page);
            *slot = Some(format!(
                "{}{}",
                segment.prefix,
                format_label_number(n, segment.style)
            ));
        }
    }
    out
}

/// Walk a page-label number tree (`/Nums` arrays plus `/Kids` nodes),
/// appending resolved [`LabelSegment`]s.
fn collect_label_segments(doc: &Document, node: &lopdf::Dictionary, out: &mut Vec<LabelSegment>) {
    if out.len() >= MAX_LABEL_SEGMENTS {
        return;
    }

    if let Ok(nums) = node.get(b"Nums")
        && let Ok((_, nums)) = doc.dereference(nums)
        && let Ok(items) = nums.as_array()
    {
        // /Nums is [key1 val1 key2 val2 ...]; keys are 0-based page indices.
        for pair in items.chunks_exact(2) {
            if out.len() >= MAX_LABEL_SEGMENTS {
                break;
            }
            let Ok(key) = pair[0].as_i64() else { continue };
            if key < 0 {
                continue;
            }
            let Ok((_, label_obj)) = doc.dereference(&pair[1]) else {
                continue;
            };
            let Ok(label) = label_obj.as_dict() else {
                continue;
            };
            let prefix = label
                .get(b"P")
                .ok()
                .and_then(|o| decode_text_string(o).ok())
                .unwrap_or_default();
            let style = label
                .get(b"S")
                .ok()
                .and_then(|o| doc.dereference(o).ok())
                .and_then(|(_, o)| o.as_name().ok())
                .and_then(|n| n.first().copied())
                .unwrap_or(b'D');
            let start_num = label
                .get(b"St")
                .ok()
                .and_then(|o| doc.dereference(o).ok())
                .and_then(|(_, o)| o.as_i64().ok())
                .unwrap_or(1);
            out.push(LabelSegment {
                start_page: key as u32 + 1,
                prefix,
                style,
                start_num,
            });
        }
    }

    if let Ok(kids) = node.get(b"Kids")
        && let Ok((_, kids)) = doc.dereference(kids)
        && let Ok(items) = kids.as_array()
    {
        for kid in items {
            if let Ok((_, kid_obj)) = doc.dereference(kid)
                && let Ok(kid_dict) = kid_obj.as_dict()
            {
                collect_label_segments(doc, kid_dict, out);
            }
        }
    }
}

/// Render a label number in the given `/S` style.
fn format_label_number(n: i64, style: u8) -> String {
    match style {
        b'R' => to_roman(n.max(0) as u32).to_uppercase(),
        b'r' => to_roman(n.max(0) as u32).to_lowercase(),
        b'A' => to_alpha(n.max(0) as u64, true),
        b'a' => to_alpha(n.max(0) as u64, false),
        _ => n.to_string(),
    }
}

/// Roman numerals for 1..=3999; falls back to decimal outside that
/// range (labels beyond MMMCMXCIX are vanishingly rare).
fn to_roman(mut n: u32) -> String {
    if n == 0 || n > 3999 {
        return n.to_string();
    }
    const TABLE: &[(u32, &str)] = &[
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut out = String::new();
    for &(value, numeral) in TABLE {
        while n >= value {
            out.push_str(numeral);
            n -= value;
        }
    }
    out
}

/// Spreadsheet-style letters: 1 → "A", 26 → "Z", 27 → "AA".
fn to_alpha(mut n: u64, upper: bool) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let mut chars = Vec::new();
    while n > 0 {
        n -= 1;
        chars.push(b'A' + (n % 26) as u8);
        n /= 26;
    }
    chars.reverse();
    let s = String::from_utf8(chars).unwrap_or_default();
    if upper { s } else { s.to_lowercase() }
}

// ---------------------------------------------------------------------------
// Embedded file names
// ---------------------------------------------------------------------------

fn collect_embedded_file_names(doc: &Document, catalog: &lopdf::Dictionary) -> Vec<String> {
    let mut names = Vec::new();
    // Catalog → /Names → /EmbeddedFiles → /Names → [name filespec ...]
    let Some(names_dict) = entry_dict(doc, catalog, b"Names") else {
        return names;
    };
    let Some(ef_dict) = entry_dict(doc, names_dict, b"EmbeddedFiles") else {
        return names;
    };
    let Ok(names_obj) = ef_dict.get(b"Names") else {
        return names;
    };
    let Ok((_, names_obj)) = doc.dereference(names_obj) else {
        return names;
    };
    let Ok(items) = names_obj.as_array() else {
        return names;
    };

    for pair in items.chunks_exact(2) {
        if names.len() >= MAX_NAME_ENTRIES {
            break;
        }
        // The name-tree key is a text string; the value a file spec.
        let name = decode_text_string(&pair[0]).ok().or_else(|| {
            pair[0]
                .as_str()
                .ok()
                .map(|b| String::from_utf8_lossy(b).into_owned())
        });
        // Resolve the file specification and prefer /UF over /F.
        let resolved = doc.dereference(&pair[1]).ok().map(|(_, o)| o);
        let spec_name = resolved.and_then(|o| o.as_dict().ok()).and_then(|spec| {
            spec.get(b"UF")
                .or_else(|_| spec.get(b"F"))
                .ok()
                .and_then(|o| decode_text_string(o).ok())
        });
        if let Some(name) = name.or(spec_name) {
            names.push(name);
        }
    }
    names
}

// ---------------------------------------------------------------------------
// AcroForm fields
// ---------------------------------------------------------------------------

fn collect_form_fields(doc: &Document, catalog: &lopdf::Dictionary) -> Vec<String> {
    let mut out = Vec::new();
    let Some(acroform) = entry_dict(doc, catalog, b"AcroForm") else {
        return out;
    };
    let Ok(fields_obj) = acroform.get(b"Fields") else {
        return out;
    };
    let Ok((_, fields_obj)) = doc.dereference(fields_obj) else {
        return out;
    };
    let Ok(fields) = fields_obj.as_array() else {
        return out;
    };
    for field in fields {
        if out.len() >= MAX_FORM_FIELDS {
            break;
        }
        if let Ok((_, field_obj)) = doc.dereference(field)
            && let Ok(dict) = field_obj.as_dict()
        {
            collect_field_names(doc, dict, "", &mut out, 0);
        }
    }
    out
}

/// Recurse a field dictionary tree, accumulating `/T` partial names
/// into dotted paths. A node contributes its name when it is a real
/// field (`/FT` present) or a leaf carrying `/T`.
fn collect_field_names(
    doc: &Document,
    dict: &lopdf::Dictionary,
    prefix: &str,
    out: &mut Vec<String>,
    depth: usize,
) {
    if out.len() >= MAX_FORM_FIELDS || depth > MAX_FIELD_DEPTH {
        return;
    }

    let partial = dict
        .get(b"T")
        .ok()
        .and_then(|o| decode_text_string(o).ok())
        .unwrap_or_default();
    let name = if prefix.is_empty() {
        partial.clone()
    } else if partial.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}.{partial}")
    };

    let has_ft = dict.has(b"FT");
    let kids: Vec<Object> = dict
        .get(b"Kids")
        .ok()
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_array().ok())
        .map(|a| a.clone())
        .unwrap_or_default();

    // Record this node when it is a terminal field or a named leaf.
    if !name.is_empty() && (has_ft || kids.is_empty()) {
        out.push(name.clone());
    }

    for kid in &kids {
        if out.len() >= MAX_FORM_FIELDS {
            break;
        }
        if let Ok((_, kid_obj)) = doc.dereference(kid)
            && let Ok(kid_dict) = kid_obj.as_dict()
        {
            collect_field_names(doc, kid_dict, &name, out, depth + 1);
        }
    }
}

// ---------------------------------------------------------------------------
// Outline titles
// ---------------------------------------------------------------------------

fn collect_outline_titles(doc: &Document, catalog: &lopdf::Dictionary) -> Vec<String> {
    let mut out = Vec::new();
    let Some(outlines) = entry_dict(doc, catalog, b"Outlines") else {
        return out;
    };
    let Ok(first_obj) = outlines.get(b"First") else {
        return out;
    };
    let Ok((_, first)) = doc.dereference(first_obj) else {
        return out;
    };
    let Ok(mut item) = first.as_dict() else {
        return out;
    };

    // Walk the top-level /First → /Next chain. Cycle-guarded by count;
    // child items (an item's own /First) are intentionally not visited.
    let mut seen: HashSet<lopdf::ObjectId> = HashSet::new();
    loop {
        if out.len() >= MAX_OUTLINE_ITEMS {
            break;
        }
        if let Ok(title_obj) = item.get(b"Title")
            && let Ok((id_opt, title_obj)) = doc.dereference(title_obj)
        {
            if let Some(id) = id_opt {
                seen.insert(id);
            }
            if let Ok(title) = decode_text_string(title_obj)
                && !title.is_empty()
            {
                out.push(title);
            }
        }
        let Ok(next_obj) = item.get(b"Next") else {
            break;
        };
        let Ok((next_id, next)) = doc.dereference(next_obj) else {
            break;
        };
        if let Some(id) = next_id
            && !seen.insert(id)
        {
            break; // cycle
        }
        let Ok(next_dict) = next.as_dict() else {
            break;
        };
        item = next_dict;
    }
    out
}

// ---------------------------------------------------------------------------
// Page geometry
// ---------------------------------------------------------------------------

/// First page's `/MediaBox` as `[f64; 4]`, resolving `/Parent`
/// inheritance and numeric `Integer`/`Real` entries.
fn page_mediabox(doc: &Document, page_id: lopdf::ObjectId) -> Option<[f64; 4]> {
    let mut current_id = Some(page_id);
    let mut direct: Option<&Object> = None;
    for _ in 0..MAX_INHERIT_DEPTH {
        let dict = match (current_id, direct) {
            (Some(id), _) => doc.get_dictionary(id).ok()?,
            (None, Some(obj)) => obj.as_dict().ok()?,
            (None, None) => return None,
        };
        if let Ok(box_obj) = dict.get_deref(b"MediaBox", doc)
            && let Some(rect) = mediabox_array(box_obj)
        {
            return Some(rect);
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

/// Parse a `/MediaBox`-shaped array of four numbers into `[f64; 4]`.
fn mediabox_array(obj: &Object) -> Option<[f64; 4]> {
    let items = obj.as_array().ok()?;
    if items.len() != 4 {
        return None;
    }
    let mut out = [0.0f64; 4];
    for (slot, item) in out.iter_mut().zip(items.iter()) {
        *slot = match item {
            Object::Integer(v) => *v as f64,
            Object::Real(v) => *v as f64,
            _ => return None,
        };
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Small dereference helpers
// ---------------------------------------------------------------------------

/// Dereference `dict[key]` (following references) and return it as a
/// dictionary, or `None` when absent/mistyped.
fn entry_dict<'a>(
    doc: &'a Document,
    dict: &'a lopdf::Dictionary,
    key: &[u8],
) -> Option<&'a lopdf::Dictionary> {
    let object = dict.get(key).ok()?;
    let (_, object) = doc.dereference(object).ok()?;
    object.as_dict().ok()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::{pdf_with_pages, pdf_with_pages_and_title};
    use lopdf::{Document, Stream, dictionary};

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    #[test]
    fn reports_info_dictionary_fields() {
        let pdf = pdf_with_pages_and_title(&["hello", "world"], "My Title");
        let meta = extract_metadata(&pdf, &limits()).unwrap();
        assert_eq!(meta.page_count, 2);
        assert_eq!(meta.title.as_deref(), Some("My Title"));
        assert_eq!(meta.author.as_deref(), Some("KalaDoc Tests"));
        assert_eq!(meta.creator.as_deref(), Some("kaladoc-test-utils"));
        assert_eq!(meta.producer.as_deref(), Some("lopdf"));
        assert!(!meta.encrypted);
        assert_eq!(meta.file_size_bytes, pdf.len() as u64);
        assert_eq!(meta.page_size_pts, Some([0.0, 0.0, 595.0, 842.0]));
        assert!(meta.subject.is_none());
        assert!(meta.keywords.is_none());
        assert!(meta.creation_date.is_none());
        assert!(meta.modification_date.is_none());
        assert!(meta.xmp.is_none());
        assert!(meta.xmp_raw.is_none());
        assert!(meta.page_labels.iter().all(Option::is_none));
        assert!(meta.embedded_files.is_empty());
        assert!(meta.form_fields.is_empty());
        assert!(meta.outline_titles.is_empty());
    }

    #[test]
    fn default_title_and_single_page() {
        let pdf = pdf_with_pages(&["only"]);
        let meta = extract_metadata(&pdf, &limits()).unwrap();
        assert_eq!(meta.page_count, 1);
        assert_eq!(meta.title.as_deref(), Some("KalaDoc test document"));
    }

    #[test]
    fn rejects_oversized_input() {
        let pdf = pdf_with_pages(&["x"]);
        let tight = EngineLimits {
            max_input_bytes: Some(10),
            ..EngineLimits::default()
        };
        assert!(extract_metadata(&pdf, &tight).is_err());
    }

    #[test]
    fn pdf_date_parsing() {
        assert_eq!(
            parse_pdf_date("D:20240115143000Z"),
            Some("2024-01-15T14:30:00Z".to_string())
        );
        assert_eq!(
            parse_pdf_date("D:20240115"),
            Some("2024-01-15T00:00:00".to_string())
        );
        assert_eq!(
            parse_pdf_date("D:20240115143000+05'30'"),
            Some("2024-01-15T14:30:00+05:30".to_string())
        );
        // Year-only is legal: later fields default.
        assert_eq!(
            parse_pdf_date("D:2024"),
            Some("2024-01-01T00:00:00".to_string())
        );
        // Too-short year and out-of-range month are not.
        assert_eq!(parse_pdf_date("D:20"), None);
        assert_eq!(parse_pdf_date("D:202413"), None);
        assert_eq!(parse_pdf_date("garbage"), None);
        assert_eq!(
            parse_pdf_date("D:19991231235959-08'00'"),
            Some("1999-12-31T23:59:59-08:00".to_string())
        );
    }

    #[test]
    fn metadata_reads_dates_and_page_labels() {
        // Build a document with CreationDate/ModDate and a /PageLabels
        // number tree on top of the standard fixture shape.
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Courier",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let mut kids = Vec::new();
        for _ in 0..3 {
            let content = lopdf::content::Content {
                operations: vec![lopdf::content::Operation::new("BT", vec![])],
            };
            let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
            let page_id = doc.add_object(dictionary! {
                "Type" => "Page", "Parent" => pages_id,
                "Contents" => content_id, "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            });
            kids.push(Object::Reference(page_id));
        }
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => kids, "Count" => 3i64,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );

        // PageLabels: pages 1-2 decimal "P.", page 3 roman starting at 4.
        let labels_id = doc.add_object(dictionary! {
            "Nums" => vec![
                Object::Integer(0),
                Object::Dictionary(dictionary! {
                    "S" => "D", "St" => 1i64, "P" => Object::string_literal("P."),
                }),
                Object::Integer(2),
                Object::Dictionary(dictionary! {
                    "S" => "r", "St" => 4i64,
                }),
            ],
        });
        let info_id = doc.add_object(dictionary! {
            "Title" => Object::string_literal("Labelled"),
            "CreationDate" => Object::string_literal("D:20240301090000Z"),
            "ModDate" => Object::string_literal("D:20240302"),
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages_id, "PageLabels" => labels_id,
        });
        doc.trailer.set("Root", catalog_id);
        doc.trailer.set("Info", info_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();

        let meta = extract_metadata(&buf, &limits()).unwrap();
        assert_eq!(
            meta.creation_date.as_ref().unwrap().parsed.as_deref(),
            Some("2024-03-01T09:00:00Z")
        );
        assert_eq!(
            meta.modification_date.as_ref().unwrap().parsed.as_deref(),
            Some("2024-03-02T00:00:00")
        );
        assert_eq!(
            meta.page_labels,
            vec![
                Some("P.1".to_string()),
                Some("P.2".to_string()),
                Some("iv".to_string()), // start 4 → roman "iv"
            ]
        );
    }

    #[test]
    fn xmp_fields_extracted() {
        let xml = r#"<?xpacket?><x:xmpmeta xmlns:dc="http://purl.org/dc/elements/1.1/">
          <rdf:RDF><rdf:Description>
            <dc:title><rdf:Alt><rdf:li xml:lang="x-default">My &amp; Title</rdf:li></rdf:Alt></dc:title>
            <dc:creator><rdf:Seq><rdf:li>Alice</rdf:li><rdf:li>Bob</rdf:li></rdf:Seq></dc:creator>
            <pdf:Producer>ProducerX</pdf:Producer>
          </rdf:Description></rdf:RDF></x:xmpmeta>"#;
        assert_eq!(
            xmp_first_text(xml, "dc:title").as_deref(),
            Some("My & Title")
        );
        assert_eq!(xmp_all_items(xml, "dc:creator"), vec!["Alice", "Bob"]);
        assert_eq!(
            xmp_plain_text(xml, "pdf:Producer").as_deref(),
            Some("ProducerX")
        );
        assert!(xmp_first_text(xml, "dc:missing").is_none());
        let parsed = parse_xmp(xml).unwrap();
        assert_eq!(
            parsed.creators,
            vec!["Alice".to_string(), "Bob".to_string()]
        );
        assert_eq!(parsed.producer.as_deref(), Some("ProducerX"));
    }
}
