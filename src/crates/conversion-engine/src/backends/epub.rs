//! EPUB → PDF backend (pure Rust, no external tools).
//!
//! An EPUB is a ZIP archive. This backend:
//!
//! 1. Reads the archive through the shared
//!    [`ZipArchive`](crate::crates::conversion_engine::archive::ZipArchive)
//!    (stored + raw-deflate members, CRC-verified).
//! 2. Reads `META-INF/container.xml` → OPF package document →
//!    `manifest` + `spine` (document order).
//! 3. For each spine item that is XHTML/HTML, extracts text through the
//!    shared [`parse_html`]/[`html_render_lines`] pipeline used by the
//!    `html` → `pdf` path, and assembles every spine document into one
//!    PDF. Each spine document starts on a fresh page (via
//!    [`build_pdf`]'s `page_breaks`).
//!
//! Deliberate limitations (all surfaced in [`ConvertOutput::warnings`]):
//! - Images, CSS, scripts, fonts and other embedded media are **not**
//!   rendered; referenced `<img>` files are listed by name in warnings.
//! - Only text structure (headings, paragraphs, lists, preformatted
//!   text) is preserved — same fidelity as `html` → `pdf`.
//! - OPF/spine XML is parsed with a minimal tag scanner, not a full XML
//!   parser; pathological-but-legal XML may degrade to warnings/errors.
//! - ZIP member names are matched as UTF-8 strings (EPUB mandates UTF-8
//!   names); non-UTF-8 names are matched lossily.
//!
//! Zip-bomb guards: per-member uncompressed cap checked *before*
//! decompression, an aggregate inflated-byte budget, a spine-item cap,
//! and a per-document XHTML byte cap.

use std::collections::{BTreeSet, HashMap};

use super::rust::{
    MARGIN, build_pdf, decode_entities, html_render_lines, option_warnings, page_dimensions,
    parse_html,
};
use crate::crates::conversion_engine::archive::ZipArchive;
use crate::crates::conversion_engine::backend::{BackendAvailability, ConversionBackend};
use crate::crates::conversion_engine::error::{ConvertError, ConvertResult};
use crate::crates::conversion_engine::model::{ConvertOutput, ConvertRequest, Format};

/// Maximum central-directory entries accepted. Real EPUBs have tens to
/// low hundreds of members; 4096 is generous.
const MAX_ZIP_ENTRIES: usize = 4096;
/// Maximum spine itemrefs processed.
const MAX_SPINE_ITEMS: usize = 512;
/// Per-member uncompressed size cap (64 MiB), checked before inflate.
const MAX_ENTRY_UNCOMPRESSED: u64 = 64 * 1024 * 1024;
/// Total bytes the backend will decompress across all member reads.
const MAX_TOTAL_UNCOMPRESSED: u64 = 256 * 1024 * 1024;
/// Per-spine-document XHTML byte cap before parsing (16 MiB); larger
/// documents are truncated with a warning.
const MAX_XHTML_BYTES: usize = 16 * 1024 * 1024;
/// How many dropped-image names are listed in warnings before
/// collapsing to a count.
const MAX_LISTED_IMAGES: usize = 10;
/// Fallback PDF title when neither content nor blocks yield one.
const FALLBACK_TITLE: &str = "EPUB document";

/// EPUB → PDF backend. Holds no state; always available.
#[derive(Debug, Default, Clone, Copy)]
pub struct EpubBackend;

impl EpubBackend {
    /// Create a new backend instance.
    pub fn new() -> Self {
        Self
    }
}

impl ConversionBackend for EpubBackend {
    fn name(&self) -> &'static str {
        "epub-rust"
    }

    fn availability(&self) -> BackendAvailability {
        // Pure in-process code — nothing to probe.
        BackendAvailability::Available { version: None }
    }

    fn supports(&self, from: Format, to: Format) -> bool {
        matches!((from, to), (Format::Epub, Format::Pdf))
    }

    fn convert(&self, req: &ConvertRequest) -> ConvertResult<ConvertOutput> {
        if req.input_format == req.output_format {
            return Err(ConvertError::InvalidInput {
                reason: "input and output formats are identical".to_string(),
            });
        }
        if !self.supports(req.input_format, req.output_format) {
            return Err(ConvertError::UnsupportedConversion {
                from: req.input_format,
                to: req.output_format,
            });
        }

        let mut warnings = option_warnings(req);
        warnings.push(
            "EPUB→PDF fallback: text structure (headings, paragraphs, lists, \
             preformatted text) is preserved; CSS, fonts, scripts and embedded \
             media are not rendered"
                .to_string(),
        );

        let archive = ZipArchive::open(&req.input)?;
        if archive.len() > MAX_ZIP_ENTRIES {
            return Err(ConvertError::TooLarge {
                limit: format!("{MAX_ZIP_ENTRIES} archive members"),
                actual: archive.len() as u64,
            });
        }
        let mut total_out: u64 = 0;

        // Spec sanity check: the first member should be `mimetype`
        // containing "application/epub+zip". Warn rather than fail —
        // many tools produce EPUBs that violate this harmlessly.
        match read_member(&archive, "mimetype", &mut total_out)? {
            Some(bytes) => {
                let mime = String::from_utf8_lossy(&bytes);
                if !mime.trim().eq_ignore_ascii_case("application/epub+zip") {
                    warnings.push(
                        "mimetype member does not contain \"application/epub+zip\"; \
                         treating input as EPUB anyway"
                            .to_string(),
                    );
                }
            }
            None => warnings
                .push("archive has no mimetype member; treating input as EPUB anyway".to_string()),
        }

        // container.xml → OPF package document.
        let container = read_member(&archive, "META-INF/container.xml", &mut total_out)?
            .ok_or_else(|| ConvertError::InvalidInput {
                reason: "META-INF/container.xml not found; not an EPUB archive".to_string(),
            })?;
        let container_xml = String::from_utf8_lossy(&container);
        let opf_name =
            container_rootfile(&container_xml).ok_or_else(|| ConvertError::InvalidInput {
                reason: "container.xml has no <rootfile> element".to_string(),
            })?;
        let opf_bytes = read_member(&archive, &opf_name, &mut total_out)?.ok_or_else(|| {
            ConvertError::InvalidInput {
                reason: "OPF package document referenced by container.xml is missing".to_string(),
            }
        })?;
        let opf_dir = parent_dir_str(&opf_name);
        let opf_xml = String::from_utf8_lossy(&opf_bytes);
        let (manifest, spine) = parse_opf(&opf_xml);
        if spine.is_empty() {
            return Err(ConvertError::InvalidInput {
                reason: "EPUB spine is empty; nothing to convert".to_string(),
            });
        }
        if spine.len() > MAX_SPINE_ITEMS {
            return Err(ConvertError::TooLarge {
                limit: format!("{MAX_SPINE_ITEMS} spine items"),
                actual: spine.len() as u64,
            });
        }

        let (page_w, page_h) = page_dimensions(req, &mut warnings);
        let text_width = page_w - 2.0 * MARGIN;

        let mut all_lines = Vec::new();
        let mut page_breaks = BTreeSet::new();
        let mut dropped_images: Vec<String> = Vec::new();
        let mut image_count = 0usize;
        let mut saw_media = false;
        let mut saw_script_style = false;
        let mut processed = 0usize;
        let mut skipped = 0usize;
        let mut title_source = String::new();
        let mut first_block_text = String::new();

        for idref in &spine {
            let Some(item) = manifest.get(idref) else {
                skipped += 1;
                continue;
            };
            if !is_html_media_type(&item.media_type) {
                skipped += 1;
                continue;
            }
            let member = resolve_member(&opf_dir, &item.href);
            let Some(mut bytes) = read_member(&archive, &member, &mut total_out)? else {
                skipped += 1;
                warnings.push(format!(
                    "spine item \"{member}\" not found in archive; skipped"
                ));
                continue;
            };

            if bytes.len() > MAX_XHTML_BYTES {
                warnings.push(format!(
                    "spine item \"{member}\" exceeds {} MiB; truncated before parsing",
                    MAX_XHTML_BYTES / (1024 * 1024)
                ));
                bytes.truncate(MAX_XHTML_BYTES);
            }
            let html = String::from_utf8_lossy(&bytes);
            if html.contains('\u{FFFD}') {
                warnings.push(format!(
                    "spine item \"{member}\" is not valid UTF-8; invalid sequences were replaced"
                ));
            }

            let doc = parse_html(&html);
            collect_image_names(&html, &mut dropped_images, &mut image_count);
            saw_media |= doc.saw_media;
            saw_script_style |= doc.saw_script_style;
            if doc.blocks.is_empty() {
                // Nothing renderable (e.g. an image-only page) — do
                // not emit a blank page or a page break for it.
                skipped += 1;
                continue;
            }

            if processed > 0 {
                // First render line of this spine doc starts a new page.
                page_breaks.insert(all_lines.len());
            }
            let lines = html_render_lines(&doc.blocks, text_width);
            if title_source.is_empty() && !doc.title.trim().is_empty() {
                title_source = doc.title.trim().to_string();
            }
            if first_block_text.is_empty() {
                first_block_text = doc
                    .blocks
                    .iter()
                    .map(|b| super::rust::collapse_ws(&b.text))
                    .find(|t| !t.is_empty())
                    .unwrap_or_default();
            }
            all_lines.extend(lines);
            processed += 1;
        }

        if skipped > 0 {
            warnings.push(format!(
                "{skipped} spine item(s) were skipped (missing, non-HTML media type, or no text content)"
            ));
        }
        if saw_script_style {
            warnings.push("script/style elements in spine documents were dropped".to_string());
        }
        if image_count > 0 || saw_media {
            let mut msg = format!(
                "{image_count} image element(s) in the EPUB were not embedded (this backend renders text only)"
            );
            if !dropped_images.is_empty() {
                msg.push_str(&format!(": {}", dropped_images.join(", ")));
                if image_count > dropped_images.len() {
                    msg.push_str(&format!(" and {} more", image_count - dropped_images.len()));
                }
            } else {
                msg.push_str(" (image/media elements present)");
            }
            warnings.push(msg);
        }
        if processed == 0 {
            warnings.push(
                "no spine document produced renderable text; output will be a blank page"
                    .to_string(),
            );
        }

        if title_source.is_empty() {
            title_source = if first_block_text.is_empty() {
                FALLBACK_TITLE.to_string()
            } else {
                first_block_text
            };
        }

        let bytes = build_pdf(
            &all_lines,
            page_w,
            page_h,
            &title_source,
            &mut warnings,
            &page_breaks,
        )?;

        Ok(ConvertOutput {
            bytes,
            output_format: Format::Pdf,
            backend: self.name().to_string(),
            warnings,
        })
    }
}

/// Read a member by normalized archive-relative name, enforcing the
/// per-entry and aggregate decompression budgets *before* the bytes
/// are inflated. `Ok(None)` when no such member exists.
fn read_member(
    archive: &ZipArchive,
    name: &str,
    total_out: &mut u64,
) -> ConvertResult<Option<Vec<u8>>> {
    let wanted = norm_member_name(name);
    let Some(index) = find_member(archive, &wanted) else {
        return Ok(None);
    };
    let entry = &archive.entries()[index];
    if entry.uncompressed_size > MAX_ENTRY_UNCOMPRESSED {
        return Err(ConvertError::TooLarge {
            limit: format!("{MAX_ENTRY_UNCOMPRESSED} bytes per archive member"),
            actual: entry.uncompressed_size,
        });
    }
    if total_out.saturating_add(entry.uncompressed_size) > MAX_TOTAL_UNCOMPRESSED {
        return Err(ConvertError::TooLarge {
            limit: format!("{MAX_TOTAL_UNCOMPRESSED} total uncompressed bytes"),
            actual: total_out.saturating_add(entry.uncompressed_size),
        });
    }
    let bytes = archive.read_entry(index)?;
    *total_out += bytes.len() as u64;
    Ok(Some(bytes))
}

// ---------------------------------------------------------------------------
// EPUB structure parsing
// ---------------------------------------------------------------------------

/// One `<item>` from the OPF manifest.
struct OpfItem {
    href: String,
    media_type: String,
}

/// Extract the OPF path from `META-INF/container.xml`: the `full-path`
/// attribute of the first `<rootfile>` element, percent-decoded and
/// normalized to archive-member form.
fn container_rootfile(container_xml: &str) -> Option<String> {
    for tag in scan_xml_tags(container_xml) {
        if tag.name == "rootfile" && !tag.closing {
            if let Some(path) = tag.attr("full-path") {
                let decoded = percent_decode(path.as_bytes());
                return Some(norm_member_name(&String::from_utf8_lossy(&decoded)));
            }
        }
    }
    None
}

/// Parse the OPF package document into `(manifest, spine)`.
///
/// `manifest`: item `id` → [`OpfItem`]. `spine`: `idref`s in document
/// order. A minimal tag scan — not a validating XML parser.
fn parse_opf(opf_xml: &str) -> (HashMap<String, OpfItem>, Vec<String>) {
    let mut manifest = HashMap::new();
    let mut spine = Vec::new();
    let mut in_manifest = false;
    let mut in_spine = false;

    for tag in scan_xml_tags(opf_xml) {
        match tag.name.as_str() {
            "manifest" => in_manifest = !tag.closing,
            "spine" => in_spine = !tag.closing,
            "item" if in_manifest && !tag.closing => {
                let id = tag.attr("id").unwrap_or_default().to_string();
                let href = tag.attr("href").unwrap_or_default().to_string();
                let media_type = tag
                    .attr("media-type")
                    .or_else(|| tag.attr("type"))
                    .unwrap_or_default()
                    .to_string();
                if !id.is_empty() && !href.is_empty() {
                    manifest.insert(id, OpfItem { href, media_type });
                }
            }
            "itemref" if in_spine && !tag.closing => {
                if let Some(idref) = tag.attr("idref") {
                    if !idref.is_empty() {
                        spine.push(idref.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    (manifest, spine)
}

/// Whether an OPF `media-type` denotes an HTML/XHTML content document.
fn is_html_media_type(media_type: &str) -> bool {
    media_type.to_ascii_lowercase().contains("html")
}

/// Directory portion of an archive-member path (everything up to the
/// last `/`), used to resolve relative OPF hrefs.
fn parent_dir_str(member: &str) -> String {
    match member.rfind('/') {
        Some(i) => member[..i].to_string(),
        None => String::new(),
    }
}

/// Resolve an OPF `href` against the OPF's directory, producing a
/// normalized archive-member name.
fn resolve_member(opf_dir: &str, href: &str) -> String {
    // Strip any fragment, then percent-decode.
    let no_frag = href.split('#').next().unwrap_or(href);
    let decoded = percent_decode(no_frag.as_bytes());
    let decoded = String::from_utf8_lossy(&decoded);
    let joined = if decoded.starts_with('/') {
        decoded.into_owned()
    } else if opf_dir.is_empty() {
        decoded.into_owned()
    } else {
        format!("{opf_dir}/{decoded}")
    };
    norm_member_name(&joined)
}

/// Normalize an archive-member path: `\` → `/`, drop `.` segments,
/// resolve `..` by popping (clamped at root), collapse repeated `/`,
/// strip leading `/`.
fn norm_member_name(name: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in name.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

/// Find the index of the archive member whose normalized name equals
/// `wanted` (already normalized). Skips directory entries.
fn find_member(archive: &ZipArchive, wanted: &str) -> Option<usize> {
    archive
        .entries()
        .iter()
        .position(|e| !e.is_dir && norm_member_name(&e.name) == wanted)
}

/// Percent-decode `%XX` sequences; other bytes pass through unchanged.
fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' && i + 2 < input.len() {
            let h = hex_val(input[i + 1]);
            let l = hex_val(input[i + 2]);
            if let (Some(h), Some(l)) = (h, l) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Collect `<img src>` references from XHTML source for the
/// dropped-media warning. `seen` accumulates display names (capped);
/// `count` tracks the true total.
fn collect_image_names(html: &str, seen: &mut Vec<String>, count: &mut usize) {
    for tag in scan_xml_tags(html) {
        if tag.name == "img" {
            *count += 1;
            if let Some(src) = tag.attr("src") {
                if seen.len() < MAX_LISTED_IMAGES && !src.trim().is_empty() {
                    // Show the basename-ish tail; full relative paths add
                    // noise without helping the user find the file.
                    let tail = src.rsplit('/').next().unwrap_or(src);
                    let tail = tail.rsplit('\\').next().unwrap_or(tail);
                    let name: String = tail.chars().take(80).collect();
                    if !seen.iter().any(|s| s == &name) {
                        seen.push(name);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Minimal XML tag scanner (container.xml / OPF / img-src harvesting)
// ---------------------------------------------------------------------------

/// One scanned XML/HTML tag.
struct XmlTag {
    /// Local tag name, ASCII-lowercased (`opf:item` → `item`).
    name: String,
    /// Whether this is a closing tag (`</name>`).
    closing: bool,
    /// `(local-name, entity-decoded value)` attribute pairs.
    attrs: Vec<(String, String)>,
}

impl XmlTag {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(n, _)| n.as_str() == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Scan `s` for XML-ish tags. Skips comments, doctypes and processing
/// instructions. Attribute values may contain `>` (quote-aware end
/// detection). Not a validating parser — malformed input yields
/// best-effort tags.
fn scan_xml_tags(s: &str) -> Vec<XmlTag> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &s[i..];
        if rest.starts_with("<!--") {
            match rest.find("-->") {
                Some(end) => i += end + 3,
                None => break,
            }
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            // Skip to the next '>' (doctype/PIs rarely contain quoted '>').
            match rest.find('>') {
                Some(end) => i += end + 1,
                None => break,
            }
            continue;
        }
        // Find quote-aware tag end.
        let mut quote: Option<u8> = None;
        let mut j = i + 1;
        let mut gt = None;
        while j < bytes.len() {
            let b = bytes[j];
            match quote {
                Some(q) => {
                    if b == q {
                        quote = None;
                    }
                }
                None => match b {
                    b'"' | b'\'' => quote = Some(b),
                    b'>' => {
                        gt = Some(j);
                        break;
                    }
                    _ => {}
                },
            }
            j += 1;
        }
        let Some(gt) = gt else { break };
        let inner = s[i + 1..gt].trim();
        i = gt + 1;

        let (closing, body) = match inner.strip_prefix('/') {
            Some(r) => (true, r.trim_start()),
            None => (false, inner),
        };
        // Tag name: up to whitespace, '/', or end. May contain ':' and
        // digits (h1, opf:item); keep the local part after ':'.
        let name_end = body
            .find(|c: char| c.is_ascii_whitespace() || c == '/')
            .unwrap_or(body.len());
        let raw_name = &body[..name_end];
        if raw_name.is_empty() {
            continue;
        }
        let local = raw_name.rsplit(':').next().unwrap_or(raw_name);
        if local.is_empty()
            || !local
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
        {
            continue;
        }
        let attrs = parse_attrs(&body[name_end..]);
        out.push(XmlTag {
            name: local.to_ascii_lowercase(),
            closing,
            attrs,
        });
    }
    out
}

/// Parse `name="value"` / `name='value'` / `name=value` pairs from the
/// attribute region of a tag (the text between the tag name and `>`).
fn parse_attrs(region: &str) -> Vec<(String, String)> {
    let bytes = region.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i;
        while i < bytes.len()
            && !(bytes[i].is_ascii_whitespace()
                || bytes[i] == b'='
                || bytes[i] == b'/'
                || bytes[i] == b'>')
        {
            i += 1;
        }
        if i == start {
            i += 1;
            continue;
        }
        let name = &region[start..i];
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let q = bytes[i];
                i += 1;
                let vs = i;
                while i < bytes.len() && bytes[i] != q {
                    i += 1;
                }
                value = region[vs..i].to_string();
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                let vs = i;
                while i < bytes.len()
                    && !bytes[i].is_ascii_whitespace()
                    && bytes[i] != b'/'
                    && bytes[i] != b'>'
                {
                    i += 1;
                }
                value = region[vs..i].to_string();
            }
        }
        let local = name.rsplit(':').next().unwrap_or(name);
        out.push((local.to_ascii_lowercase(), decode_entities(&value)));
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::conversion_engine::ConversionEngine;
    use crate::crates::conversion_engine::model::ConvertOptions;
    use lopdf::Document;
    use std::sync::Arc;

    // -- test helpers -------------------------------------------------------

    /// Build a minimal ZIP (stored members only) from `(name, bytes)`.
    /// CRC via the shared archive module's `crc32`.
    fn zip_store(files: &[(&str, &[u8])]) -> Vec<u8> {
        use crate::crates::conversion_engine::archive::crc32;
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in files {
            let name_b = name.as_bytes();
            let crc = crc32(data);
            let offset = out.len() as u32;
            out.extend_from_slice(&0x0403_4B50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // stored
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name_b);
            out.extend_from_slice(data);

            central.extend_from_slice(&0x0201_4B50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name_b);
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        let cd_size = central.len() as u32;
        out.extend_from_slice(&0x0605_4B50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// Bit writer for the fixed-Huffman deflate test encoder.
    struct BitWriter {
        out: Vec<u8>,
        bit: u32,
        cur: u8,
    }
    impl BitWriter {
        fn new() -> Self {
            Self {
                out: Vec::new(),
                bit: 0,
                cur: 0,
            }
        }
        fn bits(&mut self, v: u32, n: u32) {
            for i in 0..n {
                self.cur |= (((v >> i) & 1) as u8) << self.bit;
                self.bit += 1;
                if self.bit == 8 {
                    self.out.push(self.cur);
                    self.cur = 0;
                    self.bit = 0;
                }
            }
        }
        /// Write a Huffman code MSB-first (reverse into LSB packing).
        fn code(&mut self, code: u32, len: u32) {
            let mut rev = 0u32;
            for i in 0..len {
                rev |= ((code >> i) & 1) << (len - 1 - i);
            }
            self.bits(rev, len);
        }
        fn finish(mut self) -> Vec<u8> {
            if self.bit != 0 {
                self.out.push(self.cur);
            }
            self.out
        }
    }

    /// Fixed-Huffman deflate of `data` (literals only, one final block).
    fn fixed_deflate(data: &[u8]) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.bits(1, 1); // BFINAL
        w.bits(1, 2); // BTYPE = fixed
        for &b in data {
            let c = b as u32;
            let (code, len) = if c <= 143 {
                (0x30 + c, 8)
            } else {
                (0x190 + (c - 144), 9)
            };
            w.code(code, len);
        }
        w.code(0, 7); // end-of-block 256
        w.finish()
    }

    /// Like [`zip_store`] but marks every entry raw-deflate (method 8)
    /// with `comp` already deflated.
    fn zip_deflate(files: &[(&str, &[u8], &[u8])]) -> Vec<u8> {
        use crate::crates::conversion_engine::archive::crc32;
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, raw, comp) in files {
            let name_b = name.as_bytes();
            let crc = crc32(raw);
            let offset = out.len() as u32;
            out.extend_from_slice(&0x0403_4B50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&8u16.to_le_bytes()); // deflate
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name_b);
            out.extend_from_slice(comp);

            central.extend_from_slice(&0x0201_4B50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&8u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            central.extend_from_slice(&(raw.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name_b);
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        let cd_size = central.len() as u32;
        out.extend_from_slice(&0x0605_4B50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    const CONTAINER: &str = r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;

    const OPF: &str = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata><dc:title xmlns:dc="http://purl.org/dc/elements/1.1/">Test Book</dc:title></metadata>
  <manifest>
    <item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch2" href="chapter2.xhtml" media-type="application/xhtml+xml"/>
    <item id="css" href="style.css" media-type="text/css"/>
  </manifest>
  <spine><itemref idref="ch1"/><itemref idref="ch2"/></spine>
</package>"#;

    const CH1: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Chapter One</title></head>
<body><h1>First Chapter</h1><p>Hello <b>EPUB</b> world.</p><ul><li>one</li><li>two</li></ul></body></html>"#;

    const CH2: &str = r#"<html xmlns="http://www.w3.org/1999/xhtml"><body>
<h2>Second Chapter</h2><p>Goodbye &amp; thanks.</p><pre>pre  formatted</pre>
</body></html>"#;

    fn minimal_epub() -> Vec<u8> {
        zip_store(&[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", CONTAINER.as_bytes()),
            ("OEBPS/content.opf", OPF.as_bytes()),
            ("OEBPS/chapter1.xhtml", CH1.as_bytes()),
            ("OEBPS/chapter2.xhtml", CH2.as_bytes()),
            ("OEBPS/style.css", b"body{}"),
        ])
    }

    fn req(input: &[u8]) -> ConvertRequest {
        ConvertRequest {
            input_format: Format::Epub,
            output_format: Format::Pdf,
            input: input.to_vec(),
            options: ConvertOptions::default(),
        }
    }

    // -- EPUB parsing tests ---------------------------------------------------

    #[test]
    fn container_and_opf_parsing() {
        let tags = scan_xml_tags(CONTAINER);
        let rf = tags.iter().find(|t| t.name == "rootfile").unwrap();
        assert_eq!(rf.attr("full-path"), Some("OEBPS/content.opf"));
        assert_eq!(container_rootfile(CONTAINER).unwrap(), "OEBPS/content.opf");

        let (manifest, spine) = parse_opf(OPF);
        assert_eq!(spine, vec!["ch1".to_string(), "ch2".to_string()]);
        assert_eq!(manifest["ch1"].href, "chapter1.xhtml");
        assert_eq!(manifest["ch1"].media_type, "application/xhtml+xml");
        assert_eq!(manifest["css"].media_type, "text/css");
    }

    #[test]
    fn href_resolution() {
        let dir = "OEBPS";
        assert_eq!(
            resolve_member(dir, "chapter1.xhtml"),
            "OEBPS/chapter1.xhtml"
        );
        assert_eq!(
            resolve_member(dir, "sub/../chapter1.xhtml"),
            "OEBPS/chapter1.xhtml"
        );
        assert_eq!(
            resolve_member(dir, "my%20chapter.xhtml"),
            "OEBPS/my chapter.xhtml"
        );
        assert_eq!(resolve_member(dir, "/abs/path.xhtml"), "abs/path.xhtml");
        assert_eq!(
            resolve_member("OEBPS/text", "../images/pic.png"),
            "OEBPS/images/pic.png"
        );
    }

    #[test]
    fn norm_member_name_handles_separators_and_dots() {
        assert_eq!(norm_member_name("./OEBPS//ch1.xhtml"), "OEBPS/ch1.xhtml");
        assert_eq!(norm_member_name("OEBPS\\ch1.xhtml"), "OEBPS/ch1.xhtml");
        assert_eq!(norm_member_name("/OEBPS/ch1.xhtml"), "OEBPS/ch1.xhtml");
    }

    // -- backend tests -------------------------------------------------------

    #[test]
    fn name_availability_supports() {
        let b = EpubBackend::new();
        assert_eq!(b.name(), "epub-rust");
        assert_eq!(
            b.availability(),
            BackendAvailability::Available { version: None }
        );
        assert!(b.supports(Format::Epub, Format::Pdf));
        assert!(!b.supports(Format::Epub, Format::Txt));
        assert!(!b.supports(Format::Pdf, Format::Epub));
    }

    #[test]
    fn epub_to_pdf_produces_valid_pdf() {
        let b = EpubBackend::new();
        let out = b.convert(&req(&minimal_epub())).unwrap();
        assert_eq!(out.output_format, Format::Pdf);
        assert_eq!(out.backend, "epub-rust");
        assert!(out.bytes.starts_with(b"%PDF"), "missing %PDF header");
        Document::load_mem(&out.bytes).unwrap();
    }

    #[test]
    fn epub_two_spine_docs_start_on_separate_pages() {
        let b = EpubBackend::new();
        let out = b.convert(&req(&minimal_epub())).unwrap();
        let doc = Document::load_mem(&out.bytes).unwrap();
        // The page break between spine docs forces at least 2 pages.
        assert!(
            doc.get_pages().len() >= 2,
            "expected page break between spine docs, got {} pages",
            doc.get_pages().len()
        );
    }

    #[test]
    fn epub_deflate_entries_convert() {
        // Same book but every member deflated (fixed-Huffman streams).
        let files: Vec<(&str, Vec<u8>, Vec<u8>)> = [
            ("mimetype", b"application/epub+zip".to_vec()),
            ("META-INF/container.xml", CONTAINER.as_bytes().to_vec()),
            ("OEBPS/content.opf", OPF.as_bytes().to_vec()),
            ("OEBPS/chapter1.xhtml", CH1.as_bytes().to_vec()),
            ("OEBPS/chapter2.xhtml", CH2.as_bytes().to_vec()),
        ]
        .into_iter()
        .map(|(n, raw)| {
            let comp = fixed_deflate(&raw);
            (n, raw, comp)
        })
        .collect();
        let refs: Vec<(&str, &[u8], &[u8])> = files
            .iter()
            .map(|(n, r, c)| (*n, r.as_slice(), c.as_slice()))
            .collect();
        let epub = zip_deflate(&refs);
        let b = EpubBackend::new();
        let out = b.convert(&req(&epub)).unwrap();
        assert!(out.bytes.starts_with(b"%PDF"));
    }

    #[test]
    fn epub_engine_dispatch_end_to_end() {
        let engine = ConversionEngine::with_backends(vec![Arc::new(EpubBackend::new())]);
        let out = engine.convert(&req(&minimal_epub())).unwrap();
        assert_eq!(out.backend, "epub-rust");
        assert!(out.bytes.starts_with(b"%PDF"));
    }

    #[test]
    fn epub_garbage_is_invalid_input() {
        let b = EpubBackend::new();
        let err = b.convert(&req(b"definitely not a zip")).unwrap_err();
        assert!(matches!(err, ConvertError::InvalidInput { .. }));
    }

    #[test]
    fn epub_missing_spine_rejected() {
        let epub = zip_store(&[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", CONTAINER.as_bytes()),
            (
                "OEBPS/content.opf",
                b"<package><manifest/><spine/></package>",
            ),
        ]);
        let b = EpubBackend::new();
        let err = b.convert(&req(&epub)).unwrap_err();
        assert!(matches!(err, ConvertError::InvalidInput { .. }));
    }

    #[test]
    fn epub_missing_container_rejected() {
        let epub = zip_store(&[("mimetype", b"application/epub+zip")]);
        let b = EpubBackend::new();
        let err = b.convert(&req(&epub)).unwrap_err();
        assert!(matches!(err, ConvertError::InvalidInput { .. }));
    }

    #[test]
    fn epub_image_warns_not_silent() {
        let xhtml = r#"<html xmlns="http://www.w3.org/1999/xhtml"><body>
<h1>Pic</h1><p>before</p><img src="images/pic.png" alt="a pic"/>
<p>after</p></body></html>"#;
        let opf2 = r#"<package><manifest>
<item id="i1" href="img.xhtml" media-type="application/xhtml+xml"/>
</manifest><spine><itemref idref="i1"/></spine></package>"#;
        let epub = zip_store(&[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", CONTAINER.as_bytes()),
            ("OEBPS/content.opf", opf2.as_bytes()),
            ("OEBPS/img.xhtml", xhtml.as_bytes()),
        ]);
        let b = EpubBackend::new();
        let out = b.convert(&req(&epub)).unwrap();
        assert!(out.bytes.starts_with(b"%PDF"));
        assert!(
            out.warnings.iter().any(|w| w.contains("pic.png")),
            "expected dropped-image warning naming pic.png: {:?}",
            out.warnings
        );
    }
}
