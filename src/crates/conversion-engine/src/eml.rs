//! Minimal RFC 5322 / MIME (RFC 2045/2046) message parser for
//! `.eml` → text extraction.
//!
//! This is deliberately *not* a full mail parser — no `mail-parser`/
//! `mailparse`/`lettre` dependency exists in this crate, so the
//! smallest honest subset is implemented here:
//!
//! - Header block split from the body on the first blank line
//!   (`CRLF CRLF` or `LF LF`); folded (continuation) header lines are
//!   unfolded per RFC 5322 §2.2.3.
//! - `multipart/mixed`, `multipart/alternative` (and other
//!   `multipart/*`) bodies are walked recursively, bounded by a depth
//!   and part-count limit.
//! - `Content-Transfer-Encoding` `base64` (via the `base64` crate),
//!   `quoted-printable` (hand-rolled, soft breaks + `=XX`), and
//!   `7bit`/`8bit`/`binary`/absent (pass-through).
//! - `text/*` parts are charset-decoded: UTF-8/US-ASCII (lossy),
//!   ISO-8859-1 and Windows-1252 get real decoders; anything else is
//!   decoded as UTF-8-lossy with a note — never silently.
//! - `text/html` bodies are stripped to plain text with a tiny
//!   tag-stripper (script/style removed, block-level tags become
//!   newlines, common entities decoded). It is *not* an HTML engine.
//! - Leaf parts that are attachments (`Content-Disposition:
//!   attachment`, or any non-`text/*` type) are collected by name into
//!   [`EmlMessage::attachments`] — the PDF writer cannot embed them,
//!   so they are surfaced as warnings, never silently dropped.
//!
//! Limitations (documented, not hidden): encoded-word (`=?UTF-8?B?...?=`)
//! header decoding is NOT performed — header values are returned raw
//! (folded whitespace normalized). `message/rfc822` nested messages
//! are treated as attachments. Malformed input returns `Err` with a
//! user-safe reason; callers map it to `ConvertError::InvalidInput`.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;

/// Maximum MIME nesting depth walked (defends against pathological
/// `multipart` recursion).
const MAX_DEPTH: usize = 10;
/// Maximum total leaf parts examined across the whole message.
const MAX_PARTS: usize = 512;
/// Maximum single part body size decoded (64 MiB) — bounds memory on
/// hostile base64/QP bombs inside the engine's 256 MiB input cap.
const MAX_PART_BYTES: usize = 64 * 1024 * 1024;

/// A parsed `.eml` message: headers of interest, extracted body text
/// and a list of attachment descriptions.
#[derive(Debug, Clone, Default)]
pub struct EmlMessage {
    /// `From:` header value (unfolded), if present.
    pub from: Option<String>,
    /// `To:` header value, if present.
    pub to: Option<String>,
    /// `Cc:` header value, if present.
    pub cc: Option<String>,
    /// `Subject:` header value, if present.
    pub subject: Option<String>,
    /// `Date:` header value, if present.
    pub date: Option<String>,
    /// Plain-text body extracted from the message (preferred
    /// `text/plain`; `text/html` parts are tag-stripped). Empty when
    /// the message carries no renderable text part.
    pub body_text: String,
    /// Human-readable attachment descriptions (`"report.pdf"`,
    /// `"inline image (image/png)"`, ...). Never contains file
    /// contents.
    pub attachments: Vec<String>,
    /// Non-fatal parse caveats (unsupported charset, missing
    /// multipart boundary, ...). Surfaced as conversion warnings.
    pub notes: Vec<String>,
}

/// Parse `input` as an RFC 5322 message. Returns `Err(reason)` for
/// input that does not look like a mail message at all (no header
/// block, no `Name:` header lines).
pub fn parse(input: &[u8]) -> Result<EmlMessage, String> {
    if input.is_empty() {
        return Err("input is empty".to_string());
    }

    // Split header block from body on the first blank line. Both CRLF
    // and bare-LF line endings are accepted (broken MUAs exist).
    let (header_bytes, body_bytes) = split_header_body(input);
    let headers = parse_headers(header_bytes)?;
    if headers.is_empty() {
        return Err("no RFC 5322 header fields found".to_string());
    }

    let mut msg = EmlMessage {
        from: header_value(&headers, "from"),
        to: header_value(&headers, "to"),
        cc: header_value(&headers, "cc"),
        subject: header_value(&headers, "subject"),
        date: header_value(&headers, "date"),
        ..EmlMessage::default()
    };

    let mut walker = Walker {
        attachments: Vec::new(),
        notes: Vec::new(),
        parts_seen: 0,
        plain_parts: Vec::new(),
        html_parts: Vec::new(),
    };
    walker.walk_part(&headers, body_bytes, 0);

    msg.attachments = walker.attachments;
    msg.notes = walker.notes;

    // Body selection: prefer text/plain parts (document order, joined
    // with a blank line); fall back to tag-stripped text/html.
    if !walker.plain_parts.is_empty() {
        msg.body_text = walker.plain_parts.join("\n\n");
    } else if !walker.html_parts.is_empty() {
        msg.body_text = walker
            .html_parts
            .iter()
            .map(|h| html_to_text(h))
            .collect::<Vec<_>>()
            .join("\n\n");
        if !walker.html_parts.is_empty() {
            msg.notes.push(
                "message had no text/plain part; text/html was stripped to plain text".to_string(),
            );
        }
    }
    if msg.body_text.trim().is_empty() && msg.attachments.is_empty() {
        msg.notes
            .push("message contains no extractable text body".to_string());
    }
    Ok(msg)
}

/// Render the message as the plain-text document the PDF writer
/// consumes: a header block (From/To/Cc/Subject/Date), blank line,
/// then the body, then an attachments listing when present.
pub fn render_text(msg: &EmlMessage) -> String {
    let mut out = String::new();
    if let Some(v) = &msg.from {
        out.push_str(&format!("From: {v}\n"));
    }
    if let Some(v) = &msg.to {
        out.push_str(&format!("To: {v}\n"));
    }
    if let Some(v) = &msg.cc {
        out.push_str(&format!("Cc: {v}\n"));
    }
    if let Some(v) = &msg.subject {
        out.push_str(&format!("Subject: {v}\n"));
    }
    if let Some(v) = &msg.date {
        out.push_str(&format!("Date: {v}\n"));
    }
    out.push('\n');
    out.push_str(msg.body_text.trim_end());
    out.push('\n');
    if !msg.attachments.is_empty() {
        out.push_str("\nAttachments (not embedded in PDF):\n");
        for a in &msg.attachments {
            out.push_str(&format!("- {a}\n"));
        }
    }
    out
}

/// One `(name, value)` header pair, name lowercased.
type Headers = Vec<(String, String)>;

/// Split raw input into `(header_bytes, body_bytes)` at the first
/// blank line. If no blank line exists the whole input is treated as
/// the header block (bodyless message is legal).
fn split_header_body(input: &[u8]) -> (&[u8], &[u8]) {
    // Search for "\n\n" or "\r\n\r\n"; the latter is covered by the
    // former only if we also accept "\n\r\n"... simplest: scan for a
    // line that is empty (only an optional '\r').
    let mut pos = 0usize;
    while pos < input.len() {
        // Find end of this line.
        let line_start = pos;
        let mut i = pos;
        while i < input.len() && input[i] != b'\n' {
            i += 1;
        }
        let line_end = i; // index of '\n' or EOF
        // Strip trailing '\r' for the emptiness check.
        let content_end = if line_end > line_start && input[line_end - 1] == b'\r' {
            line_end - 1
        } else {
            line_end
        };
        if content_end == line_start {
            // Empty line: body starts after the '\n' (or at EOF).
            let body_start = if i < input.len() { i + 1 } else { i };
            return (&input[..line_start], &input[body_start..]);
        }
        pos = if i < input.len() { i + 1 } else { i };
        if i >= input.len() {
            break;
        }
    }
    (input, &[])
}

/// Parse and unfold the header block. Returns `Err` when no
/// `Name: value` header field is present at all.
fn parse_headers(header_bytes: &[u8]) -> Result<Headers, String> {
    let text = String::from_utf8_lossy(header_bytes);
    let mut headers: Headers = Vec::new();
    let mut pending: Option<(String, String)> = None;

    for raw_line in text.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        // Folded continuation: starts with SP/HT and we have a
        // pending header — append with a single space (RFC 5322
        // unfolding replaces CRLF+WSP with the WSP itself; collapsing
        // to one space is the common practical normalization).
        if (line.starts_with(' ') || line.starts_with('\t')) && pending.is_some() {
            let (_, value) = pending.as_mut().unwrap();
            value.push(' ');
            value.push_str(line.trim());
            continue;
        }
        if let Some(pair) = pending.take() {
            headers.push(pair);
        }
        // Header field: "Name: value". Name = printable ASCII minus
        // colon; keep it permissive but require a colon.
        if let Some(colon) = line.find(':') {
            let name = line[..colon].trim();
            if !name.is_empty()
                && name.chars().all(|c| {
                    c.is_ascii_alphanumeric()
                        || matches!(
                            c,
                            '!' | '#'
                                | '$'
                                | '%'
                                | '&'
                                | '\''
                                | '*'
                                | '+'
                                | '-'
                                | '/'
                                | '='
                                | '?'
                                | '^'
                                | '_'
                                | '`'
                                | '{'
                                | '|'
                                | '}'
                                | '~'
                        )
                })
            {
                let value = line[colon + 1..].trim().to_string();
                pending = Some((name.to_ascii_lowercase(), value));
                continue;
            }
        }
        // Non-header line inside the header block (e.g. a stray
        // preamble): ignore rather than fail — real-world .eml files
        // are messy. `headers.is_empty()` check upstream catches the
        // totally-malformed case.
    }
    if let Some(pair) = pending.take() {
        headers.push(pair);
    }
    if headers.is_empty() {
        return Err("no RFC 5322 header fields found".to_string());
    }
    Ok(headers)
}

/// First value for `name` (lowercase), unfolded value trimmed.
fn header_value(headers: &Headers, name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Case-insensitive parameter lookup on a `Content-Type`-style
/// header value: `type/subtype; param=value; ...`.
fn content_type_param(value: &str, param: &str) -> Option<String> {
    for piece in value.split(';').skip(1) {
        let piece = piece.trim();
        if let Some((k, v)) = piece.split_once('=') {
            if k.trim().eq_ignore_ascii_case(param) {
                let v = v.trim().trim_matches('"');
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Media type (lowercased, no parameters) of a `Content-Type` value,
/// defaulting to `text/plain` per RFC 2045 when absent.
fn media_type(headers: &Headers, default: &str) -> String {
    header_value(headers, "content-type")
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Accumulator for the MIME tree walk.
struct Walker {
    attachments: Vec<String>,
    notes: Vec<String>,
    parts_seen: usize,
    /// Extracted `text/plain` bodies, in document order.
    plain_parts: Vec<String>,
    /// Extracted `text/html` bodies, in document order.
    html_parts: Vec<String>,
}

impl Walker {
    /// Process one MIME part: `headers` are the part's own header
    /// block, `body` its raw (still transfer-encoded) body bytes.
    fn walk_part(&mut self, headers: &Headers, body: &[u8], depth: usize) {
        self.parts_seen += 1;
        if self.parts_seen > MAX_PARTS {
            self.notes
                .push("message has too many MIME parts; remaining parts skipped".to_string());
            return;
        }
        if depth > MAX_DEPTH {
            self.notes
                .push("MIME nesting too deep; remaining parts skipped".to_string());
            return;
        }

        let ctype = media_type(headers, "text/plain");
        let disposition = header_value(headers, "content-disposition")
            .map(|v| {
                v.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase()
            })
            .unwrap_or_default();

        if ctype.starts_with("multipart/") {
            self.walk_multipart(headers, body, depth);
            return;
        }

        // message/rfc822 nested messages are treated as opaque
        // attachments — we do not recurse into forwarded messages.
        let is_attachment =
            disposition == "attachment" || ctype == "message/rfc822" || !ctype.starts_with("text/");
        if is_attachment {
            self.attachments.push(attachment_label(headers, &ctype));
            return;
        }

        // Text leaf: decode transfer encoding, then charset.
        if body.len() > MAX_PART_BYTES {
            self.notes.push(format!(
                "a {ctype} part exceeds the 64 MiB per-part limit and was skipped"
            ));
            return;
        }
        let decoded = match decode_transfer(body, &transfer_encoding(headers)) {
            Ok(d) => d,
            Err(note) => {
                self.notes.push(note);
                return;
            }
        };
        let charset = header_value(headers, "content-type")
            .and_then(|v| content_type_param(&v, "charset"))
            .unwrap_or_else(|| "utf-8".to_string());
        let (text, note) = decode_text(&decoded, &charset);
        if let Some(n) = note {
            self.notes.push(n);
        }
        if ctype == "text/html" || ctype == "application/xhtml+xml" {
            self.html_parts.push(text);
        } else {
            // text/plain and any other text/* subtype.
            self.plain_parts.push(text);
        }
    }

    /// Split a `multipart/*` body on its boundary and recurse.
    fn walk_multipart(&mut self, headers: &Headers, body: &[u8], depth: usize) {
        let ctype_value = header_value(headers, "content-type").unwrap_or_default();
        let boundary = content_type_param(&ctype_value, "boundary");
        let Some(boundary) = boundary.filter(|b| !b.is_empty()) else {
            self.notes.push(
                "multipart MIME part is missing a boundary parameter; its body was skipped"
                    .to_string(),
            );
            return;
        };
        for segment in split_multipart(body, boundary.as_bytes()) {
            let (sub_headers, sub_body) = split_part(segment);
            self.walk_part(&sub_headers, sub_body, depth + 1);
        }
    }
}

/// `Content-Transfer-Encoding` value lowercased (`""` when absent).
fn transfer_encoding(headers: &Headers) -> String {
    header_value(headers, "content-transfer-encoding")
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default()
}

/// Split one MIME body segment into `(headers, body)`: the part's own
/// header block plus its (still encoded) body bytes.
fn split_part(segment: &[u8]) -> (Headers, &[u8]) {
    let (h, b) = split_header_body(segment);
    let headers = parse_headers(h).unwrap_or_default();
    (headers, b)
}

/// Human-readable label for an attachment part: filename when
/// declared (`Content-Disposition` `filename=` or `Content-Type`
/// `name=`), else a type description.
fn attachment_label(headers: &Headers, ctype: &str) -> String {
    let disp = header_value(headers, "content-disposition").unwrap_or_default();
    if let Some(name) = content_type_param(&disp, "filename") {
        return sanitize_label(&name);
    }
    let ct = header_value(headers, "content-type").unwrap_or_default();
    if let Some(name) = content_type_param(&ct, "name") {
        return sanitize_label(&name);
    }
    format!("unnamed attachment ({ctype})")
}

/// Keep attachment labels single-line and bounded — they come from
/// untrusted input and end up in warnings/PDF text.
fn sanitize_label(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(200)
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "unnamed attachment".to_string()
    } else {
        cleaned.to_string()
    }
}

/// Apply `Content-Transfer-Encoding` decoding. Returns decoded bytes
/// or an error string describing the problem (surfaced as a note).
fn decode_transfer(body: &[u8], encoding: &str) -> Result<Vec<u8>, String> {
    match encoding {
        "" | "7bit" | "8bit" | "binary" => Ok(body.to_vec()),
        "base64" => {
            // MIME base64 may contain whitespace; strip it.
            let compact: Vec<u8> = body
                .iter()
                .copied()
                .filter(|b| !b.is_ascii_whitespace())
                .collect();
            B64.decode(&compact)
                .map_err(|_| "base64 attachment/body part could not be decoded".to_string())
        }
        "quoted-printable" => Ok(decode_quoted_printable(body)),
        other => Err(format!(
            "unsupported Content-Transfer-Encoding \"{other}\"; part skipped"
        )),
    }
}

/// Minimal quoted-printable decoder: `=XX` hex escapes and soft line
/// breaks (`=` at end of line). Malformed escapes pass through as
/// literal text rather than failing.
fn decode_quoted_printable(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0usize;
    while i < input.len() {
        match input[i] {
            b'=' if i + 1 < input.len() => {
                let next = input[i + 1];
                if next == b'\r' || next == b'\n' {
                    // Soft break: skip "=" + CRLF (or "=" + LF, or
                    // "=" + CR at EOF).
                    i += 2;
                    if next == b'\r' && i < input.len() && input[i] == b'\n' {
                        i += 1;
                    }
                } else if i + 2 < input.len()
                    && input[i + 1].is_ascii_hexdigit()
                    && input[i + 2].is_ascii_hexdigit()
                {
                    let hi = (input[i + 1] as char).to_digit(16).unwrap();
                    let lo = (input[i + 2] as char).to_digit(16).unwrap();
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                } else {
                    // Malformed escape: emit '=' literally.
                    out.push(b'=');
                    i += 1;
                }
            }
            b'=' => {
                // Trailing '=' at EOF.
                out.push(b'=');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// Decode bytes to text for a charset. Returns `(text, note)` —
/// `note` is `Some` when the charset was unrecognized and UTF-8
/// lossy decoding was used as a fallback.
fn decode_text(bytes: &[u8], charset: &str) -> (String, Option<String>) {
    let cs = charset.trim().trim_matches('"').to_ascii_lowercase();
    match cs.as_str() {
        "" | "utf-8" | "utf8" | "us-ascii" | "ascii" => {
            let text = String::from_utf8_lossy(bytes).into_owned();
            (text, None)
        }
        "iso-8859-1" | "latin1" | "latin-1" | "iso8859-1" | "iso_8859-1" | "l1" => {
            // ISO-8859-1 maps bytes 1:1 onto Unicode code points.
            (bytes.iter().map(|&b| b as char).collect(), None)
        }
        "windows-1252" | "cp1252" | "win-1252" | "windows1252" => (decode_cp1252(bytes), None),
        other => (
            String::from_utf8_lossy(bytes).into_owned(),
            Some(format!(
                "charset \"{other}\" is not supported; part was decoded as UTF-8 (lossy)"
            )),
        ),
    }
}

/// Windows-1252 → String. Bytes 0x00–0x7F and 0xA0–0xFF map to the
/// same code points; 0x80–0x9F use the cp1252 punctuation table.
fn decode_cp1252(bytes: &[u8]) -> String {
    const CP1252_HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', // 80-87
        'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž', '\u{8F}', // 88-8F
        '\u{90}', '‘', '’', '“', '”', '•', '–', '—', // 90-97
        '˜', '™', 'š', '›', 'œ', '\u{9D}', 'ž', 'Ÿ', // 98-9F
    ];
    bytes
        .iter()
        .map(|&b| match b {
            0x80..=0x9F => CP1252_HIGH[(b - 0x80) as usize],
            _ => b as char,
        })
        .collect()
}

/// Split a `multipart/*` body into its part segments. Boundary
/// delimiters are `--boundary` at a line start, optionally followed
/// by transport padding and CRLF; the closing `--boundary--`
/// terminates the list. Preamble/epilogue are skipped.
fn split_multipart<'a>(body: &'a [u8], boundary: &[u8]) -> Vec<&'a [u8]> {
    let mut marker = Vec::with_capacity(boundary.len() + 2);
    marker.extend_from_slice(b"--");
    marker.extend_from_slice(boundary);

    // Find delimiter line starts: position 0 or right after '\n',
    // followed by the marker, optional SP/TAB padding, then CRLF/LF
    // or "--" (closing).
    let mut segments = Vec::new();
    let mut part_start: Option<usize> = None;
    let mut pos = 0usize;
    while pos + marker.len() <= body.len() {
        // Candidate: marker at line start.
        let at_line_start = pos == 0 || body[pos - 1] == b'\n';
        if !at_line_start || &body[pos..pos + marker.len()] != marker.as_slice() {
            pos += 1;
            continue;
        }
        // Validate what follows the marker.
        let mut j = pos + marker.len();
        while j < body.len() && (body[j] == b' ' || body[j] == b'\t') {
            j += 1;
        }
        let closing = j + 1 < body.len() && body[j] == b'-' && body[j + 1] == b'-';
        let eol =
            if j < body.len() && body[j] == b'\r' && j + 1 < body.len() && body[j + 1] == b'\n' {
                j + 2
            } else if j < body.len() && body[j] == b'\n' {
                j + 1
            } else if j >= body.len() {
                j
            } else {
                // Not a real boundary (marker prefix of a longer token).
                pos += 1;
                continue;
            };
        if closing {
            // Closing delimiter: emit pending part, stop.
            if let Some(start) = part_start.take() {
                segments.push(trim_trailing_eol(&body[start..pos]));
            }
            return segments;
        }
        if let Some(start) = part_start.take() {
            segments.push(trim_trailing_eol(&body[start..pos]));
        }
        part_start = Some(eol);
        pos = eol;
    }
    // No closing delimiter found: emit whatever part is open (be
    // lenient — truncated MIME is common) but note nothing here;
    // callers see whatever parsed.
    if let Some(start) = part_start {
        segments.push(trim_trailing_eol(&body[start..]));
    }
    segments
}

/// Strip one trailing line break (CRLF or LF) from a segment — the
/// delimiter's own line ending belongs to the boundary, not the part.
fn trim_trailing_eol(mut segment: &[u8]) -> &[u8] {
    if segment.ends_with(b"\r\n") {
        segment = &segment[..segment.len() - 2];
    } else if segment.ends_with(b"\n") {
        segment = &segment[..segment.len() - 1];
    }
    segment
}

/// Very small HTML → text stripper. NOT an HTML parser: removes
/// `<script>`/`<style>` content, turns block-level closings/`<br>`
/// into newlines, drops all remaining tags, decodes the five common
/// entities plus numeric `&#NN;`/`&#xNN;`. Enough for typical
/// `text/html` email bodies.
fn html_to_text(html: &str) -> String {
    let mut s = html.to_string();
    // Remove script/style blocks (case-insensitive, non-greedy,
    // dotall-ish via manual scan since regex isn't a dep).
    for tag in ["script", "style"] {
        s = strip_tag_blocks(&s, tag);
    }
    // Block-level boundaries → newline.
    let lower = s.to_ascii_lowercase();
    // Work on the lowered copy for tag detection but copy bytes from
    // the original to preserve case in text. '<'/'>' are ASCII and
    // cannot appear inside UTF-8 multibyte sequences, so byte-level
    // scanning is safe; the result is lossy-decoded at the end.
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Find '>' — a '>' inside a quoted attr value is rare in
            // email HTML; accept the limitation.
            if let Some(gt) = s[i..].find('>') {
                let tag_inner = &lower[i + 1..i + gt];
                let name_end = tag_inner
                    .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
                    .unwrap_or(tag_inner.len());
                let name = &tag_inner[..name_end];
                let closing = name.starts_with('/');
                let base = name.trim_start_matches('/');
                let is_block = matches!(
                    base,
                    "br" | "p"
                        | "div"
                        | "li"
                        | "tr"
                        | "table"
                        | "ul"
                        | "ol"
                        | "blockquote"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                        | "hr"
                        | "pre"
                );
                if is_block && (closing || base == "br" || base == "hr") {
                    out.push(b'\n');
                } else if closing
                    && matches!(
                        base,
                        "p" | "div"
                            | "li"
                            | "tr"
                            | "h1"
                            | "h2"
                            | "h3"
                            | "h4"
                            | "h5"
                            | "h6"
                            | "blockquote"
                            | "pre"
                    )
                {
                    out.push(b'\n');
                }
                i += gt + 1;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    decode_entities(&String::from_utf8_lossy(&out))
}

/// Remove `<tag>...</tag>` blocks (script/style) case-insensitively.
fn strip_tag_blocks(s: &str, tag: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    let mut lower_rest = lower.as_str();
    loop {
        let Some(start) = lower_rest.find(&open) else {
            out.push_str(rest);
            break;
        };
        // Ensure it's a real tag open (next char is '>', ws, or '/')
        let after = start + open.len();
        let boundary_ok = lower_rest[after..]
            .chars()
            .next()
            .map(|c| c == '>' || c == '/' || c.is_whitespace())
            .unwrap_or(false);
        if !boundary_ok {
            // Not our tag (e.g. <scriptx>): copy through the '<'.
            out.push_str(&rest[..start + 1]);
            let skip = start + 1;
            rest = &rest[skip..];
            lower_rest = &lower_rest[skip..];
            continue;
        }
        out.push_str(&rest[..start]);
        let Some(close_pos) = lower_rest[after..].find(&close) else {
            // Unterminated: drop the rest.
            break;
        };
        let consumed = after + close_pos + close.len();
        rest = &rest[consumed..];
        lower_rest = &lower_rest[consumed..];
    }
    out
}

/// Decode common HTML entities in text: `&amp; &lt; &gt; &quot;
/// &apos; &nbsp;` and numeric `&#NN;` / `&#xHH;`. Unknown entities
/// pass through unchanged.
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let semi = tail.find(';');
        match semi {
            Some(semi_pos) if semi_pos <= 10 => {
                let entity = &tail[1..semi_pos];
                let decoded = match entity {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" | "#39" => Some('\''),
                    "nbsp" => Some(' '),
                    _ => {
                        if let Some(num) = entity
                            .strip_prefix("#x")
                            .or_else(|| entity.strip_prefix("#X"))
                        {
                            u32::from_str_radix(num, 16).ok().and_then(char::from_u32)
                        } else if let Some(num) = entity.strip_prefix('#') {
                            num.parse::<u32>().ok().and_then(char::from_u32)
                        } else {
                            None
                        }
                    }
                };
                match decoded {
                    Some(c) => {
                        out.push(c);
                        rest = &tail[semi_pos + 1..];
                    }
                    None => {
                        out.push('&');
                        rest = &tail[1..];
                    }
                }
            }
            _ => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_plain_message() {
        let eml = b"From: Alice <alice@example.com>\r\n\
                   To: Bob <bob@example.com>\r\n\
                   Subject: Hello there\r\n\
                   Date: Mon, 01 Jan 2024 10:00:00 +0000\r\n\
                   \r\n\
                   Hi Bob,\r\n\
                   this is the body.\r\n";
        let msg = parse(eml).unwrap();
        assert_eq!(msg.from.as_deref(), Some("Alice <alice@example.com>"));
        assert_eq!(msg.to.as_deref(), Some("Bob <bob@example.com>"));
        assert_eq!(msg.subject.as_deref(), Some("Hello there"));
        assert!(msg.body_text.contains("this is the body."));
        assert!(msg.attachments.is_empty());
    }

    #[test]
    fn unfolds_folded_headers() {
        let eml = b"From: Alice\r\n <alice@example.com>\r\n\r\nBody\r\n";
        let msg = parse(eml).unwrap();
        assert_eq!(msg.from.as_deref(), Some("Alice <alice@example.com>"));
    }

    #[test]
    fn multipart_alternative_prefers_plain() {
        let eml = b"From: a@b.c\r\n\
                   Content-Type: multipart/alternative; boundary=\"B1\"\r\n\
                   \r\n\
                   --B1\r\n\
                   Content-Type: text/plain; charset=utf-8\r\n\
                   \r\n\
                   plain version\r\n\
                   --B1\r\n\
                   Content-Type: text/html; charset=utf-8\r\n\
                   \r\n\
                   <p>html <b>version</b></p>\r\n\
                   --B1--\r\n";
        let msg = parse(eml).unwrap();
        assert!(msg.body_text.contains("plain version"));
        assert!(!msg.body_text.contains("html version"));
    }

    #[test]
    fn multipart_mixed_extracts_attachment_names() {
        let eml = b"From: a@b.c\r\n\
                   Content-Type: multipart/mixed; boundary=\"MIX\"\r\n\
                   \r\n\
                   --MIX\r\n\
                   Content-Type: text/plain\r\n\
                   \r\n\
                   See attached.\r\n\
                   --MIX\r\n\
                   Content-Type: application/pdf\r\n\
                   Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
                   Content-Transfer-Encoding: base64\r\n\
                   \r\n\
                   JVBERi0xLjQK\r\n\
                   --MIX--\r\n";
        let msg = parse(eml).unwrap();
        assert!(msg.body_text.contains("See attached."));
        assert_eq!(msg.attachments, vec!["report.pdf".to_string()]);
    }

    #[test]
    fn base64_and_quoted_printable_decoded() {
        let eml = b"From: a@b.c\r\n\
                   Content-Type: text/plain; charset=utf-8\r\n\
                   Content-Transfer-Encoding: base64\r\n\
                   \r\n\
                   aGVsbG8gd29ybGQ=\r\n";
        let msg = parse(eml).unwrap();
        assert!(msg.body_text.contains("hello world"));

        let eml_qp = b"From: a@b.c\r\n\
                      Content-Type: text/plain\r\n\
                      Content-Transfer-Encoding: quoted-printable\r\n\
                      \r\n\
                      caf=C3=A9=20na=C3=AFve=20=\r\n\
                      next line\r\n";
        let msg_qp = parse(eml_qp).unwrap();
        assert!(msg_qp.body_text.contains("café naïve"));
        assert!(msg_qp.body_text.contains("next line"));
    }

    #[test]
    fn html_fallback_stripped() {
        let eml = b"From: a@b.c\r\n\
                   Content-Type: text/html\r\n\
                   \r\n\
                   <html><body><p>Hello <b>there</b></p><p>Second &amp; last</p></body></html>\r\n";
        let msg = parse(eml).unwrap();
        assert!(msg.body_text.contains("Hello there"));
        assert!(msg.body_text.contains("Second & last"));
        assert!(!msg.body_text.contains('<'));
        assert!(msg.notes.iter().any(|n| n.contains("text/html")));
    }

    #[test]
    fn garbage_input_is_error() {
        assert!(parse(b"not an email at all\njust text\n").is_err());
        assert!(parse(b"").is_err());
    }

    #[test]
    fn windows_1252_charset_decoded() {
        // "café" in cp1252: 63 61 66 E9
        let eml = b"From: a@b.c\r\n\
                   Content-Type: text/plain; charset=windows-1252\r\n\
                   \r\n\
                   caf\xE9\r\n";
        let msg = parse(eml).unwrap();
        assert!(msg.body_text.contains("café"));
    }

    #[test]
    fn render_text_includes_headers_and_attachments() {
        let msg = EmlMessage {
            from: Some("a@b.c".to_string()),
            subject: Some("Hi".to_string()),
            body_text: "body".to_string(),
            attachments: vec!["x.bin".to_string()],
            ..EmlMessage::default()
        };
        let text = render_text(&msg);
        assert!(text.contains("From: a@b.c"));
        assert!(text.contains("Subject: Hi"));
        assert!(text.contains("- x.bin"));
    }
}
