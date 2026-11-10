//! Signer-certificate extraction and best-effort X.509 parsing.
//!
//! [`extract_signer_certificates`] walks the document's signature
//! fields (the same `/AcroForm` `/Fields` tree and catalog `/Perms`
//! references [`verify_signatures`](super::verify_signature::verify_signatures)
//! inspects) and collects every DER-encoded certificate it can find:
//!
//! - **`/Cert` entries** in the signature value dictionary — a single
//!   string or an array of strings, each holding one or more
//!   DER-encoded certificates (concatenated `SEQUENCE`s are split).
//!   These strings may be literal or hexadecimal in the file; `lopdf`
//!   has already decoded both to raw bytes by the time we see them.
//! - **Certificates embedded in the CMS/PKCS#7 `/Contents` blob** —
//!   the normal case for `adbe.pkcs7.*` signatures. Extracted by
//!   shelling out to `openssl pkcs7 -print_certs` and base64-decoding
//!   the PEM blocks it emits. This step is *best-effort*: when
//!   `openssl` is absent from `PATH` the CMS-embedded certificates are
//!   skipped and only `/Cert` entries are returned (the result is
//!   never faked). [`parse_cert_with_openssl`] is the function that
//!   hard-fails with [`PdfError::MissingTool`] when `openssl` is needed
//!   but missing.
//!
//! [`parse_cert_with_openssl`] pipes a single DER certificate to
//! `openssl x509 -inform DER -noout -subject -issuer -serial -dates
//! -text` and parses the text output into a [`CertSummary`]. It is
//! deliberately best-effort:
//!
//! - `subject`/`issuer` are openssl's default one-line renderings —
//!   the exact DN formatting depends on the openssl version and
//!   `openssl.cnf`, so callers should not parse them further.
//! - `key_usage` is parsed from the `X509v3 Key Usage` extension text;
//!   `is_ca` from `CA:TRUE` in `X509v3 Basic Constraints`; both are
//!   `false`/empty when the extensions are absent.
//! - `self_signed` is the heuristic `subject == issuer` — it does not
//!   verify the signature, only that issuer and subject render
//!   identically.
//! - `not_before`/`not_after` are openssl's `notBefore=`/`notAfter=`
//!   strings verbatim (e.g. `"Oct  1 12:00:00 2025 GMT"`), not parsed
//!   timestamps.
//!
//! Encrypted input is tolerated like [`verify_signatures`]: field
//! dictionaries are not encrypted, but string *values* (`/Cert`,
//! `/Contents`) are ciphertext, so extraction simply finds no
//! parseable certificates.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use base64::Engine as _;
use lopdf::{Dictionary, Document, Object};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;
use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::load_pdf;
use super::raster::find_tool;
use super::verify_signature::is_signature_field;

/// The external tool certificate parsing and CMS extraction shell out to.
const OPENSSL_TOOL: &str = "openssl";

/// Maximum depth followed through field `/Kids` chains (mirrors
/// `verify_signature`).
const MAX_FIELD_DEPTH: usize = 32;

/// Total certificates returned by [`extract_signer_certificates`].
const MAX_CERTS: usize = 256;

/// Largest single `/Cert` or `/Contents` string considered, in bytes.
/// Signature blobs are normally a few KiB; this only stops pathological
/// inputs from being shipped to `openssl`.
const MAX_SOURCE_BLOB: usize = 16 * 1024 * 1024;

/// Largest single DER certificate accepted.
const MAX_SINGLE_CERT: usize = 4 * 1024 * 1024;

/// Cap on `openssl x509 -text` output kept for parsing.
const MAX_X509_OUTPUT: usize = 512 * 1024;

/// Cap on any single parsed text field (subject, issuer, usage entry).
const MAX_FIELD_LEN: usize = 1024;

/// One DER-encoded signer certificate found in the document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignerCert {
    /// Discovery order across all signature fields, 0-based.
    pub index: usize,
    /// Raw DER bytes of the certificate.
    pub der_bytes: Vec<u8>,
    /// `der_bytes` base64-encoded (standard alphabet, padded).
    pub der_b64: String,
    /// `der_bytes.len()` — duplicated for convenience.
    pub size: usize,
}

/// Best-effort summary of one X.509 certificate, parsed via `openssl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CertSummary {
    /// One-line subject DN as printed by `openssl x509 -subject`.
    pub subject: String,
    /// One-line issuer DN as printed by `openssl x509 -issuer`.
    pub issuer: String,
    /// Serial number as printed by `openssl x509 -serial`
    /// (openssl prints it in hex, possibly negative for >64-bit
    /// serials — kept verbatim).
    pub serial: String,
    /// `notBefore=` value verbatim, e.g. `"Oct  1 12:00:00 2025 GMT"`.
    pub not_before: Option<String>,
    /// `notAfter=` value verbatim.
    pub not_after: Option<String>,
    /// `X509v3 Key Usage` extension entries, comma-split and trimmed.
    /// Empty when the extension is absent or unparsable.
    pub key_usage: Vec<String>,
    /// `true` when the text contains `CA:TRUE` (Basic Constraints).
    pub is_ca: bool,
    /// Heuristic: `subject == issuer`. Does not verify the signature.
    pub self_signed: bool,
}

/// Extract every signer certificate embedded in `input`.
///
/// Returns certificates in discovery order: for each signature field,
/// `/Cert` entries first, then certificates embedded in the CMS
/// `/Contents` blob. Duplicate DER blobs are returned only once.
///
/// See the module docs for the `/Cert` vs CMS-embedded distinction and
/// the `openssl`-absent behavior.
pub fn extract_signer_certificates(
    input: &[u8],
    limits: &EngineLimits,
) -> PdfResult<Vec<SignerCert>> {
    let doc = load_pdf(input, limits)?;
    let sig_dicts = collect_signature_dicts(&doc);

    // CMS-embedded certificates need openssl; probe lazily — only when
    // at least one signature carries a usable /Contents blob. When the
    // tool is absent the CMS path is skipped entirely.
    let needs_cms = sig_dicts
        .iter()
        .any(|d| signature_contents(&doc, d).is_some());
    let tool = if needs_cms {
        find_tool(OPENSSL_TOOL).ok()
    } else {
        None
    };
    let workdir = if tool.is_some() {
        tempfile::tempdir().ok().map(|d| d.path().to_path_buf())
    } else {
        None
    };

    let mut certs: Vec<SignerCert> = Vec::new();
    let mut seen: HashSet<Vec<u8>> = HashSet::new();

    let push = |der: Vec<u8>, certs: &mut Vec<SignerCert>, seen: &mut HashSet<Vec<u8>>| {
        if der.is_empty() || der.len() > MAX_SINGLE_CERT || certs.len() >= MAX_CERTS {
            return;
        }
        if !seen.insert(der.clone()) {
            return;
        }
        certs.push(SignerCert {
            index: certs.len(),
            der_b64: base64::engine::general_purpose::STANDARD.encode(&der),
            size: der.len(),
            der_bytes: der,
        });
    };

    for (sig_index, dict) in sig_dicts.iter().enumerate() {
        if certs.len() >= MAX_CERTS {
            break;
        }

        // 1. Explicit /Cert entries (single string or array of strings).
        for blob in cert_strings(&doc, dict) {
            for der in split_der_certificates(&blob) {
                push(der.to_vec(), &mut certs, &mut seen);
            }
        }

        // 2. Certificates embedded in the CMS /Contents blob.
        if let (Some(tool), Some(dir)) = (tool.as_deref(), workdir.as_deref())
            && let Some(cms) = signature_contents(&doc, dict)
        {
            for der in cms_certificates(tool, &cms, dir, sig_index) {
                push(der, &mut certs, &mut seen);
            }
        }
    }

    Ok(certs)
}

/// Parse one DER-encoded X.509 certificate via `openssl x509`.
///
/// Requires `openssl` on `PATH`; a missing binary yields
/// [`PdfError::MissingTool`]. A non-zero exit (e.g. `der` is not a
/// certificate) yields [`PdfError::ExternalTool`]. See the module docs
/// for the honesty notes on the parsed fields.
pub fn parse_cert_with_openssl(der: &[u8]) -> PdfResult<CertSummary> {
    let tool = find_tool(OPENSSL_TOOL)?;
    let stdout = run_x509(&tool, der)?;
    Ok(parse_x509_output(&stdout))
}

// ---------------------------------------------------------------------
// Document walking
// ---------------------------------------------------------------------

/// Collect the signature *value* dictionaries (`/V`, or the field dict
/// itself for merged field/value dictionaries) of every signature
/// field, mirroring `verify_signature::collect_fields`.
fn collect_signature_dicts<'a>(doc: &'a Document) -> Vec<&'a Dictionary> {
    let mut out = Vec::new();

    let Ok(catalog) = doc.catalog() else {
        return out;
    };

    // 1. AcroForm signature fields.
    if let Ok(acroform) = catalog.get_deref(b"AcroForm", doc)
        && let Ok(fields_obj) = acroform.as_dict().and_then(|d| d.get(b"Fields"))
        && let Ok((_, fields)) = doc.dereference(fields_obj)
        && let Object::Array(field_refs) = fields
    {
        for field_ref in field_refs {
            collect_field_dicts(doc, field_ref, 0, &mut out);
        }
    }

    // 2. Catalog /Perms signature references (DocMDP, UR).
    if let Ok(perms) = catalog.get_deref(b"Perms", doc)
        && let Ok(perms_dict) = perms.as_dict()
    {
        for key in [b"DocMDP".as_slice(), b"UR".as_slice()] {
            let Ok(reference_obj) = perms_dict.get(key) else {
                continue;
            };
            let Ok((_, reference)) = doc.dereference(reference_obj) else {
                continue;
            };
            let Ok(reference_dict) = reference.as_dict() else {
                continue;
            };
            if let Ok(refs_obj) = reference_dict.get(b"Reference")
                && let Ok((_, refs)) = doc.dereference(refs_obj)
                && let Object::Array(items) = refs
            {
                for item in items {
                    collect_field_dicts(doc, item, 0, &mut out);
                }
            }
        }
    }

    out
}

/// Recursively walk a field dictionary and its `/Kids`, pushing the
/// signature value dictionary of every field that
/// [`is_signature_field`] accepts.
fn collect_field_dicts<'a>(
    doc: &'a Document,
    field_obj: &'a Object,
    depth: usize,
    out: &mut Vec<&'a Dictionary>,
) {
    if depth > MAX_FIELD_DEPTH {
        return;
    }
    let Ok((_, field)) = doc.dereference(field_obj) else {
        return;
    };
    let Ok(dict) = field.as_dict() else {
        return;
    };

    if is_signature_field(doc, dict) {
        let sig = dict
            .get_deref(b"V", doc)
            .ok()
            .and_then(|v| v.as_dict().ok())
            .unwrap_or(dict);
        out.push(sig);
    }

    if let Ok(kids_obj) = dict.get(b"Kids")
        && let Ok((_, kids)) = doc.dereference(kids_obj)
        && let Object::Array(items) = kids
    {
        for kid in items {
            collect_field_dicts(doc, kid, depth + 1, out);
        }
    }
}

/// Raw bytes of a signature dictionary's `/Contents` string, if
/// present, non-empty and within [`MAX_SOURCE_BLOB`].
fn signature_contents(doc: &Document, dict: &Dictionary) -> Option<Vec<u8>> {
    dict.get(b"Contents")
        .ok()
        .and_then(|c| deref(doc, c))
        .and_then(|c| c.as_str().ok())
        .filter(|bytes| !bytes.is_empty() && bytes.len() <= MAX_SOURCE_BLOB)
        .map(|bytes| bytes.to_vec())
}

/// Decoded bytes of every `/Cert` string in a signature dictionary —
/// accepts a single string or an array of strings, following
/// references. Over-size blobs are skipped.
fn cert_strings(doc: &Document, dict: &Dictionary) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let Ok(cert_obj) = dict.get(b"Cert") else {
        return out;
    };
    let Ok((_, cert)) = doc.dereference(cert_obj) else {
        return out;
    };
    match cert {
        Object::String(bytes, _) => {
            if bytes.len() <= MAX_SOURCE_BLOB {
                out.push(bytes.clone());
            }
        }
        Object::Array(items) => {
            for item in items {
                if let Ok(bytes) = item.as_str()
                    && bytes.len() <= MAX_SOURCE_BLOB
                {
                    out.push(bytes.to_vec());
                }
            }
        }
        _ => {}
    }
    out
}

/// Dereference an object one level if it is a reference.
fn deref<'a>(doc: &'a Document, object: &'a Object) -> Option<&'a Object> {
    doc.dereference(object).ok().map(|(_, o)| o)
}

// ---------------------------------------------------------------------
// DER / PEM handling
// ---------------------------------------------------------------------

/// Split `bytes` into DER `SEQUENCE` TLVs. All-or-nothing: if any
/// element fails to parse as a well-formed TLV the whole input yields
/// nothing — garbage (e.g. encrypted ciphertext) must not surface as a
/// "certificate".
fn split_der_certificates(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        match der_tlv_len(rest) {
            Some(total) if total <= rest.len() => {
                out.push(&rest[..total]);
                rest = &rest[total..];
            }
            _ => return Vec::new(),
        }
    }
    out
}

/// Total TLV length (header + content) of the DER element at the start
/// of `buf`, or `None` when it is not a `SEQUENCE` or is malformed.
/// Only definite, short/long-form lengths up to 4 octets are accepted.
fn der_tlv_len(buf: &[u8]) -> Option<usize> {
    // X.509 certificates are always a single top-level SEQUENCE (0x30).
    if buf.len() < 2 || buf[0] != 0x30 {
        return None;
    }
    let lb = buf[1];
    let (len, header) = if lb & 0x80 == 0 {
        (lb as usize, 2)
    } else {
        let n = (lb & 0x7F) as usize;
        if n == 0 || n > 4 || buf.len() < 2 + n {
            return None;
        }
        let mut l = 0usize;
        for &b in &buf[2..2 + n] {
            l = l.checked_mul(256)?.checked_add(b as usize)?;
        }
        (l, 2 + n)
    };
    len.checked_add(header)
}

/// Extract DER certificates from PEM text: every
/// `-----BEGIN CERTIFICATE-----` … `-----END CERTIFICATE-----` block's
/// base64 body, decoded. Malformed blocks are skipped.
fn pem_certificates(pem: &str) -> Vec<Vec<u8>> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let mut out = Vec::new();
    let mut rest = pem;
    while let Some(start) = rest.find(BEGIN) {
        rest = &rest[start + BEGIN.len()..];
        let Some(end) = rest.find(END) else {
            break;
        };
        let body = &rest[..end];
        rest = &rest[end + END.len()..];
        // PEM bodies are base64 with arbitrary whitespace; strip it.
        let b64: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        if let Ok(der) = base64::engine::general_purpose::STANDARD.decode(b64)
            && !der.is_empty()
        {
            out.push(der);
        }
    }
    out
}

// ---------------------------------------------------------------------
// CMS extraction via openssl
// ---------------------------------------------------------------------

/// Run `openssl pkcs7 -print_certs` on a DER CMS blob and return the
/// DER bytes of every certificate it embeds. Best-effort: any failure
/// (spawn, non-zero exit, missing output file, bad PEM) yields an
/// empty list — callers treat "no certificates" uniformly.
fn cms_certificates(tool: &Path, cms: &[u8], dir: &Path, sig_index: usize) -> Vec<Vec<u8>> {
    let tag = format!("sig{sig_index}");
    let sig_path = dir.join(format!("{tag}-contents.der"));
    let pem_path = dir.join(format!("{tag}-certs.pem"));
    if std::fs::write(&sig_path, cms).is_err() {
        return Vec::new();
    }
    let ok = Command::new(tool)
        .args(["pkcs7", "-inform", "DER", "-in"])
        .arg(&sig_path)
        .args(["-print_certs", "-out"])
        .arg(&pem_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return Vec::new();
    }
    let Ok(pem) = std::fs::read_to_string(&pem_path) else {
        return Vec::new();
    };
    pem_certificates(&pem)
}

// ---------------------------------------------------------------------
// openssl x509 parsing
// ---------------------------------------------------------------------

/// Run `openssl x509 -inform DER -noout -subject -issuer -serial
/// -dates -text` with `der` on stdin; return capped stdout on success.
fn run_x509(tool: &Path, der: &[u8]) -> PdfResult<Vec<u8>> {
    let mut child = Command::new(tool)
        .args([
            "x509", "-inform", "DER", "-noout", "-subject", "-issuer", "-serial", "-dates", "-text",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| PdfError::ExternalTool {
            tool: OPENSSL_TOOL.to_string(),
            detail: format!("failed to spawn: {e}"),
        })?;

    // openssl reads the whole cert from stdin; an early exit surfaces
    // as EPIPE here and as a non-zero status below — either way the
    // write result is not trusted on its own.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(der);
    }

    let output = child
        .wait_with_output()
        .map_err(|e| PdfError::ExternalTool {
            tool: OPENSSL_TOOL.to_string(),
            detail: format!("failed to wait: {e}"),
        })?;
    if !output.status.success() {
        return Err(PdfError::ExternalTool {
            tool: OPENSSL_TOOL.to_string(),
            detail: first_line(&output.stderr),
        });
    }
    let mut stdout = output.stdout;
    stdout.truncate(MAX_X509_OUTPUT);
    Ok(stdout)
}

/// First line of tool output, trimmed and capped — for error details.
fn first_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(200)
        .collect()
}

/// Trim and cap a parsed field value.
fn clean(text: &str) -> String {
    text.trim().chars().take(MAX_FIELD_LEN).collect()
}

/// Parse `openssl x509 -text` output into a [`CertSummary`]. Pure
/// function over already-captured output — unit-testable without the
/// tool.
fn parse_x509_output(stdout: &[u8]) -> CertSummary {
    let text = String::from_utf8_lossy(stdout);
    let lines: Vec<&str> = text.lines().collect();

    let mut subject = String::new();
    let mut issuer = String::new();
    let mut serial = String::new();
    let mut not_before = None;
    let mut not_after = None;

    for line in &lines {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("subject=") {
            subject = clean(v);
        } else if let Some(v) = t.strip_prefix("issuer=") {
            issuer = clean(v);
        } else if let Some(v) = t.strip_prefix("serial=") {
            serial = clean(v);
        } else if let Some(v) = t.strip_prefix("notBefore=") {
            not_before = Some(clean(v));
        } else if let Some(v) = t.strip_prefix("notAfter=") {
            not_after = Some(clean(v));
        }
    }

    let key_usage = parse_key_usage(&lines);
    let is_ca = lines.iter().any(|l| l.trim_start().starts_with("CA:TRUE"));
    let self_signed = !subject.is_empty() && subject == issuer;

    CertSummary {
        subject,
        issuer,
        serial,
        not_before,
        not_after,
        key_usage,
        is_ca,
        self_signed,
    }
}

/// Extract the `X509v3 Key Usage` extension value: text after the `:`
/// on the header line plus any more-indented continuation lines
/// (openssl wraps long usage lists). Entries are comma-split.
fn parse_key_usage(lines: &[&str]) -> Vec<String> {
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if !t.starts_with("X509v3 Key Usage") {
            continue;
        }
        let header_indent = indent_of(line);
        let mut raw = String::new();
        if let Some((_, v)) = t.split_once(':') {
            // openssl marks critical extensions with a trailing
            // `critical` keyword — it is a flag, not a usage entry.
            if v.trim() != "critical" {
                raw.push_str(v);
            }
        }
        for cont in &lines[i + 1..] {
            let trimmed = cont.trim();
            if trimmed.is_empty() {
                break;
            }
            if indent_of(cont) <= header_indent {
                break;
            }
            raw.push(' ');
            raw.push_str(trimmed);
        }
        return raw
            .split(',')
            .map(|s| clean(s))
            .filter(|s| !s.is_empty())
            .collect();
    }
    Vec::new()
}

/// Count of leading whitespace columns of a line.
fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;

    fn unlimited() -> EngineLimits {
        EngineLimits {
            max_input_bytes: None,
            max_pages: None,
            max_pixels: None,
            max_outputs: None,
        }
    }

    /// `true` when `openssl` is on `PATH` — certificate-dependent tests
    /// are gated on this like the sign/verify tests.
    fn openssl_available() -> bool {
        find_tool(OPENSSL_TOOL).is_ok()
    }

    /// Build a minimal DER TLV: `0x30` + length + `content`.
    fn der_seq(content: &[u8]) -> Vec<u8> {
        let mut v = vec![0x30];
        if content.len() < 0x80 {
            v.push(content.len() as u8);
        } else {
            v.push(0x81);
            v.push(content.len() as u8);
        }
        v.extend_from_slice(content);
        v
    }

    #[test]
    fn unsigned_pdf_yields_no_certificates() {
        let input = pdf_with_pages(&["no signatures here"]);
        let certs = extract_signer_certificates(&input, &unlimited()).unwrap();
        assert!(certs.is_empty());
    }

    #[test]
    fn split_der_single_and_concatenated() {
        let a = der_seq(&[1, 2, 3]);
        let b = der_seq(&[4, 5]);
        let mut both = a.clone();
        both.extend_from_slice(&b);
        let parts = split_der_certificates(&both);
        assert_eq!(parts, vec![a.as_slice(), b.as_slice()]);
    }

    #[test]
    fn split_der_rejects_garbage() {
        // Random bytes, wrong tag, truncated length — all yield nothing.
        assert!(split_der_certificates(b"not a certificate").is_empty());
        assert!(split_der_certificates(&[0x30]).is_empty());
        assert!(split_der_certificates(&[0x30, 0x05, 0x01]).is_empty());
        // Valid TLV followed by garbage → all-or-nothing → nothing.
        let mut bad = der_seq(&[1]);
        bad.extend_from_slice(b"junk");
        assert!(split_der_certificates(&bad).is_empty());
    }

    #[test]
    fn pem_certificates_decodes_blocks() {
        let der1 = der_seq(&[9, 9]);
        let der2 = der_seq(&[7]);
        let pem = format!(
            "junk before\n-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n\
             -----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\ntrailing",
            base64::engine::general_purpose::STANDARD.encode(&der1),
            base64::engine::general_purpose::STANDARD.encode(&der2),
        );
        let certs = pem_certificates(&pem);
        assert_eq!(certs, vec![der1, der2]);
        // Unterminated block is skipped, not fatal.
        assert!(pem_certificates("-----BEGIN CERTIFICATE-----\nAAAA").is_empty());
    }

    #[test]
    fn parse_x509_output_extracts_fields() {
        let out = b"subject=CN = Test Signer, O = Example\n\
                   issuer=CN = Test Signer, O = Example\n\
                   serial=0A1B2C\n\
                   notBefore=Oct  1 12:00:00 2025 GMT\n\
                   notAfter=Oct  1 12:00:00 2026 GMT\n\
                   X509v3 extensions:\n    \
                       X509v3 Key Usage: critical\n        \
                           Digital Signature, Key Encipherment\n    \
                       X509v3 Basic Constraints: critical\n        \
                           CA:TRUE\n";
        let s = parse_x509_output(out);
        assert_eq!(s.subject, "CN = Test Signer, O = Example");
        assert_eq!(s.issuer, s.subject);
        assert_eq!(s.serial, "0A1B2C");
        assert_eq!(s.not_before.as_deref(), Some("Oct  1 12:00:00 2025 GMT"));
        assert_eq!(
            s.key_usage,
            vec![
                "Digital Signature".to_string(),
                "Key Encipherment".to_string()
            ]
        );
        assert!(s.is_ca);
        assert!(s.self_signed);
    }

    #[test]
    fn parse_cert_garbage_der_never_panics() {
        // Whatever openssl's state, garbage DER must produce an Err —
        // MissingTool when absent, ExternalTool when present — never a
        // panic or a fabricated summary.
        assert!(parse_cert_with_openssl(b"definitely not der").is_err());
    }

    #[test]
    fn signed_pdf_extracts_and_parses_signer_cert() {
        if !openssl_available() {
            return;
        }
        use std::process::{Command, Stdio};

        let dir = tempfile::tempdir().unwrap();
        let tool = find_tool(OPENSSL_TOOL).unwrap();
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        let status = Command::new(&tool)
            .args(["req", "-x509", "-newkey", "rsa:2048", "-keyout"])
            .arg(&key)
            .args(["-out"])
            .arg(&cert)
            .args(["-days", "1", "-nodes", "-subj", "/CN=Cert Info Test"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let creds = super::super::sign::SignCredentials::Pem {
            certificate: cert,
            private_key: key,
            private_key_password: None,
        };

        let input = pdf_with_pages(&["extract my certs"]);
        let signed = super::super::sign::sign(
            &input,
            &creds,
            &super::super::sign::SignOptions::default(),
            &unlimited(),
        )
        .unwrap();

        let certs = extract_signer_certificates(&signed, &unlimited()).unwrap();
        assert!(!certs.is_empty());
        let first = &certs[0];
        assert_eq!(first.index, 0);
        assert_eq!(first.size, first.der_bytes.len());
        assert_eq!(first.der_bytes[0], 0x30); // DER SEQUENCE
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&first.der_b64)
                .unwrap(),
            first.der_bytes
        );

        // The CMS-embedded signer cert parses into a sane summary.
        let summary = parse_cert_with_openssl(&first.der_bytes).unwrap();
        assert!(summary.subject.contains("Cert Info Test"));
        assert!(!summary.serial.is_empty());
        assert!(summary.not_before.is_some());
        assert!(summary.not_after.is_some());
        assert!(summary.self_signed);
    }
}
