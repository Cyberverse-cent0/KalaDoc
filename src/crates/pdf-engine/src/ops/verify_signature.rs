//! Structural signature inspection — **not** cryptographic verification.
//!
//! [`verify_signatures`] reports which signature fields a document
//! declares: their names, sub-filters, signer names, signing times and
//! whether the `/ByteRange` plausibly covers the file. It does **not**
//! verify the cryptographic signature: certificate chains, digest
//! correctness, revocation and timestamp trust are all out of scope.
//! A `byte_range_covers_file == true` result means only that the signed
//! byte ranges span the whole file except the `/Contents` placeholder —
//! it says nothing about whether the signature is valid.
//!
//! Encrypted input is tolerated like `inspect`: field *names* are
//! dictionary keys (never encrypted) so detection still works, but string
//! values (`/T`, `/Name`, `/M`, `/Contents`) are ciphertext and decode to
//! `None` — reported fields will simply have `None` values.

use std::path::{Path, PathBuf};

use lopdf::{Dictionary, Document, Object, decode_text_string};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{doc_is_encrypted, load_pdf};
use super::raster::find_tool;

/// Maximum depth followed through field `/Kids` chains — guards against
/// cyclic or maliciously deep field trees.
const MAX_FIELD_DEPTH: usize = 32;

/// The external tool [`verify_signatures_crypto`] shells out to.
const OPENSSL_TOOL: &str = "openssl";

/// DER encoding of the RFC 3161 `id-ct-TSTInfo` OID
/// (`1.2.840.113549.1.9.16.2.14`) — used only as a *presence* check for
/// timestamp tokens inside a CMS blob; the token itself is never
/// validated.
const TST_INFO_OID: &[u8] = &[
    0x06, 0x0B, 0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x10, 0x02, 0x0E,
];

/// Structural information about one signature field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureInfo {
    /// Field name (`/T` partial name), if decodable.
    pub field_name: Option<String>,
    /// `/SubFilter` name (e.g. `adbe.pkcs7.detached`), if present.
    pub sub_filter: Option<String>,
    /// Signer name (`/Name` in the signature dictionary), if decodable.
    pub name: Option<String>,
    /// Signing time (`/M`, a PDF date string), if decodable.
    pub signed_at: Option<String>,
    /// The signature dictionary carries a `/ByteRange` array.
    pub byte_range_present: bool,
    /// The signature dictionary carries a non-empty `/Contents` string.
    pub contents_present: bool,
    /// `/ByteRange` is four integers `[a b c d]` whose two regions cover
    /// the entire file except the `/Contents` placeholder: region two
    /// ends exactly at `input.len()` and the regions do not overlap
    /// (`a + b <= c`).
    pub byte_range_covers_file: bool,
}

/// Aggregate result of [`verify_signatures`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureReport {
    /// Number of signature fields found.
    pub signature_count: u32,
    /// One entry per detected signature field.
    pub signatures: Vec<SignatureInfo>,
    /// Human-readable findings: missing `/ByteRange`, byte ranges that do
    /// not span the file, "no signatures found", etc. When signatures are
    /// present this also carries a note that the report is structural
    /// only — cryptographic validity is not verified.
    pub issues: Vec<String>,
}

/// Inspect `input` for signature fields and return a
/// [`SignatureReport`].
///
/// Detects signature fields via the catalog's `/AcroForm` `/Fields` tree
/// (fields with `/FT /Sig`, or whose `/V` dictionary carries
/// `/ByteRange`/`/Contents`, recursing through `/Kids`), plus signature
/// references under catalog `/Perms` (`/DocMDP`, `/UR`). See the module
/// docs for the honesty caveat: this is a structural report only.
/// A detected signature field plus the raw material needed for optional
/// cryptographic verification.
pub(crate) struct FoundSignature {
    /// Structural view exposed by [`verify_signatures`].
    pub info: SignatureInfo,
    /// Decoded `/Contents` bytes (the CMS/PKCS#7 blob), if present and
    /// non-empty.
    pub contents: Option<Vec<u8>>,
    /// Parsed `/ByteRange` as `[a b c d]`, if present and well-formed.
    pub byte_range: Option<[i64; 4]>,
}

/// Walk the catalog's signature-bearing structures — the AcroForm
/// `/Fields` tree and catalog `/Perms` (`/DocMDP`, `/UR`) — and return
/// every detected signature field. Shared by [`verify_signatures`],
/// [`verify_signatures_crypto`] and the `sign`/`remove_signatures` ops.
pub(crate) fn find_signatures(doc: &Document, file_len: i64) -> Vec<FoundSignature> {
    let mut signatures = Vec::new();

    let Ok(catalog) = doc.catalog() else {
        return signatures;
    };

    // 1. AcroForm signature fields.
    if let Ok(acroform) = catalog.get_deref(b"AcroForm", doc)
        && let Ok(fields_obj) = acroform.as_dict().and_then(|d| d.get(b"Fields"))
        && let Ok((_, fields)) = doc.dereference(fields_obj)
        && let Object::Array(field_refs) = fields
    {
        for field_ref in field_refs {
            collect_fields(doc, field_ref, None, 0, &mut signatures, file_len);
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
                    collect_fields(doc, item, None, 0, &mut signatures, file_len);
                }
            }
        }
    }

    signatures
}

pub fn verify_signatures(input: &[u8], limits: &EngineLimits) -> PdfResult<SignatureReport> {
    let doc = load_pdf(input, limits)?;
    let file_len = input.len() as i64;
    let mut issues = Vec::new();

    if doc.catalog().is_err() {
        issues.push("could not read the document catalog".to_string());
    }

    let found = find_signatures(&doc, file_len);
    let signatures: Vec<SignatureInfo> = found.into_iter().map(|f| f.info).collect();

    if signatures.is_empty() {
        issues.push("no signatures found".to_string());
    } else {
        issues.push(
            "structural report only: cryptographic signature validity \
             (certificate chain, digest) is not verified"
                .to_string(),
        );
    }

    Ok(SignatureReport {
        signature_count: signatures.len() as u32,
        signatures,
        issues,
    })
}

/// Recursively walk a field dictionary and its `/Kids`, emitting a
/// [`FoundSignature`] for every dictionary that looks like a signature.
fn collect_fields(
    doc: &Document,
    field_obj: &Object,
    inherited_name: Option<String>,
    depth: usize,
    out: &mut Vec<FoundSignature>,
    file_len: i64,
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

    // The field's own partial name wins; kids may omit /T.
    let name = dict
        .get(b"T")
        .ok()
        .and_then(|t| decode_text_string(t).ok())
        .or(inherited_name);

    if is_signature_field(doc, dict) {
        out.push(signature_info(doc, dict, name.clone(), file_len));
    }

    if let Ok(kids_obj) = dict.get(b"Kids")
        && let Ok((_, kids)) = doc.dereference(kids_obj)
        && let Object::Array(items) = kids
    {
        for kid in items {
            collect_fields(doc, kid, name.clone(), depth + 1, out, file_len);
        }
    }
}

/// A dictionary counts as a signature field when it is typed `/Sig`,
/// declares a signature `/V` dictionary carrying `/ByteRange` or
/// `/Contents`, or carries those keys directly (merged field/value dicts).
pub(crate) fn is_signature_field(doc: &Document, dict: &Dictionary) -> bool {
    if dict
        .get(b"FT")
        .ok()
        .and_then(|ft| deref(doc, ft))
        .is_some_and(|o| matches!(o, Object::Name(n) if n == b"Sig"))
    {
        return true;
    }
    if let Ok(v) = dict.get_deref(b"V", doc)
        && let Ok(v_dict) = v.as_dict()
        && (v_dict.has(b"ByteRange") || v_dict.has(b"Contents"))
    {
        return true;
    }
    dict.has(b"ByteRange") || dict.has(b"Contents")
}

/// Build a [`FoundSignature`] from a signature field dictionary. The
/// signature value dictionary is `/V` when present, else the field dict
/// itself.
fn signature_info(
    doc: &Document,
    field: &Dictionary,
    field_name: Option<String>,
    file_len: i64,
) -> FoundSignature {
    let sig: &Dictionary = field
        .get_deref(b"V", doc)
        .ok()
        .and_then(|v| v.as_dict().ok())
        .unwrap_or(field);

    let sub_filter = sig
        .get(b"SubFilter")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| match o {
            Object::Name(n) => Some(String::from_utf8_lossy(n).into_owned()),
            _ => None,
        });
    let name = sig
        .get(b"Name")
        .ok()
        .and_then(|n| decode_text_string(n).ok());
    let signed_at = sig.get(b"M").ok().and_then(|m| decode_text_string(m).ok());

    let byte_range = sig
        .get(b"ByteRange")
        .ok()
        .and_then(|br| deref(doc, br))
        .and_then(|br| match br {
            Object::Array(items) => Some(items),
            _ => None,
        });
    let byte_range_values = byte_range.and_then(|items| parse_byte_range(items));
    let byte_range_present = byte_range.is_some();
    let byte_range_covers_file = byte_range_values
        .is_some_and(|[a, b, c, d]| a >= 0 && b >= 0 && d >= 0 && a + b <= c && c + d == file_len);

    let contents = sig
        .get(b"Contents")
        .ok()
        .and_then(|c| deref(doc, c))
        .and_then(|c| c.as_str().ok())
        .filter(|bytes| !bytes.is_empty())
        .map(|bytes| bytes.to_vec());
    let contents_present = contents.is_some();

    FoundSignature {
        info: SignatureInfo {
            field_name,
            sub_filter,
            name,
            signed_at,
            byte_range_present,
            contents_present,
            byte_range_covers_file,
        },
        contents,
        byte_range: byte_range_values,
    }
}

/// Dereference an object one level if it is a reference.
fn deref<'a>(doc: &'a Document, object: &'a Object) -> Option<&'a Object> {
    doc.dereference(object).ok().map(|(_, o)| o)
}

/// Parse a `/ByteRange` array into four integers.
fn parse_byte_range(items: &[Object]) -> Option<[i64; 4]> {
    if items.len() != 4 {
        return None;
    }
    let mut out = [0i64; 4];
    for (slot, item) in out.iter_mut().zip(items) {
        *slot = item.as_i64().ok()?;
    }
    Some(out)
}

// ---------------------------------------------------------------------
// Cryptographic verification (via `openssl`)
// ---------------------------------------------------------------------

/// Cryptographic verification result for one signature field.
///
/// Unlike [`SignatureInfo`] this reports *computed* facts: whether the
/// CMS signature validates over the declared `/ByteRange`, the signer's
/// certificate subject, and — only when trust anchors were supplied —
/// whether the certificate chain verifies. Every field is `Option`:
/// `None` means "could not determine", never "assume ok".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CryptoSignatureInfo {
    /// Field name (`/T` partial name), if decodable.
    pub field_name: Option<String>,
    /// `/SubFilter` name (e.g. `adbe.pkcs7.detached`), if present.
    pub sub_filter: Option<String>,
    /// Signer name (`/Name` in the signature dictionary), if decodable.
    pub signer_name: Option<String>,
    /// Whether the CMS/PKCS#7 signature cryptographically validates over
    /// the bytes selected by `/ByteRange`. `None` when the signature
    /// lacks `/Contents` or a usable `/ByteRange`, or when `openssl`
    /// could not run — see the report's `issues`.
    pub digest_valid: Option<bool>,
    /// Subject (RFC 2253) of the first certificate embedded in the CMS,
    /// if one could be extracted.
    pub signer_subject: Option<String>,
    /// `Some(bool)` when `trust_anchors` were supplied to
    /// [`verify_signatures_crypto`]; `None` otherwise — chain
    /// verification is opt-in because it needs a trust store.
    pub chain_verified: Option<bool>,
    /// The CMS blob contains the RFC 3161 `id-ct-TSTInfo` OID.
    ///
    /// **Presence check only** — the timestamp token is not validated
    /// (that would require `openssl ts -verify` against the original
    /// message *and* the TSA's certificate).
    pub timestamp_token_present: bool,
}

/// Aggregate result of [`verify_signatures_crypto`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CryptoSignatureReport {
    /// Number of signature fields found.
    pub signature_count: u32,
    /// One entry per detected signature field.
    pub signatures: Vec<CryptoSignatureInfo>,
    /// Human-readable findings: skipped checks, out-of-bounds byte
    /// ranges, tool failures, unverified chains, etc.
    pub issues: Vec<String>,
}

/// Cryptographically verify the signatures in `input` using `openssl`.
///
/// For each detected signature field this reconstructs the signed byte
/// sequence from `/ByteRange` (`input[a..a+b]` concatenated with
/// `input[c..c+d]`) and runs `openssl cms -verify` on the `/Contents`
/// CMS blob. The signer certificate is extracted with
/// `openssl pkcs7 -print_certs`; when `trust_anchors` (PEM-encoded CA
/// certificates) are supplied, `openssl verify` additionally checks the
/// chain.
///
/// Requires `openssl` on `PATH` whenever at least one signature carries
/// a `/Contents` blob — a missing tool yields
/// [`PdfError::MissingTool`](crate::crates::pdf_engine::PdfError::MissingTool).
///
/// **Honesty notes:**
/// - `digest_valid == Some(true)` means the CMS signature matches the
///   declared byte ranges *of this file as given* — it does not detect
///   incremental-update tricks or prove the document is the one the
///   signer intended (that is what `/Reference` transforms and
///   DocMDP permissions are for, and they are out of scope).
/// - Revocation (CRL/OCSP) is never checked.
/// - Timestamp tokens are detected but never verified.
/// - Encrypted input yields unreliable results (string values are
///   ciphertext); a warning is added to `issues`.
pub fn verify_signatures_crypto(
    input: &[u8],
    limits: &EngineLimits,
    trust_anchors: Option<&[u8]>,
) -> PdfResult<CryptoSignatureReport> {
    let doc = load_pdf(input, limits)?;
    let found = find_signatures(&doc, input.len() as i64);

    let mut issues = Vec::new();
    if doc_is_encrypted(&doc) {
        issues.push(
            "document is encrypted: signature strings may be ciphertext \
             and verification results are unreliable"
                .to_string(),
        );
    }

    // Only probe for openssl when there is actually a CMS blob to check.
    let needs_tool = found.iter().any(|f| f.contents.is_some());
    let tool = if needs_tool {
        Some(find_tool(OPENSSL_TOOL)?)
    } else {
        None
    };

    let workdir = if needs_tool {
        Some(tempfile::tempdir().map_err(|e| PdfError::Io(std::io::Error::other(e)))?)
    } else {
        None
    };

    let mut signatures = Vec::with_capacity(found.len());
    for (index, f) in found.iter().enumerate() {
        signatures.push(crypto_check_one(
            tool.as_deref(),
            workdir.as_ref().map(|d| d.path()),
            index,
            input,
            f,
            trust_anchors,
            &mut issues,
        ));
    }

    if signatures.is_empty() {
        issues.push("no signatures found".to_string());
    }

    Ok(CryptoSignatureReport {
        signature_count: signatures.len() as u32,
        signatures,
        issues,
    })
}

/// Verify one [`FoundSignature`] cryptographically. Never fails —
/// problems are recorded in `issues` and surfaced as `None`/`Some(false)`
/// fields, so one bad signature does not sink the whole report.
fn crypto_check_one(
    tool: Option<&Path>,
    workdir: Option<&Path>,
    index: usize,
    input: &[u8],
    found: &FoundSignature,
    trust_anchors: Option<&[u8]>,
    issues: &mut Vec<String>,
) -> CryptoSignatureInfo {
    let label = found
        .info
        .field_name
        .clone()
        .unwrap_or_else(|| "<unnamed>".to_string());
    let mut entry = CryptoSignatureInfo {
        field_name: found.info.field_name.clone(),
        sub_filter: found.info.sub_filter.clone(),
        signer_name: found.info.name.clone(),
        digest_valid: None,
        signer_subject: None,
        chain_verified: None,
        timestamp_token_present: false,
    };

    let (Some(contents), Some(byte_range)) = (&found.contents, &found.byte_range) else {
        issues.push(format!(
            "{label}: missing /Contents or /ByteRange — cryptographic \
             check skipped"
        ));
        return entry;
    };
    entry.timestamp_token_present = contains_subslice(contents, TST_INFO_OID);

    let Some(tool) = tool else {
        // No /Contents anywhere → openssl was never probed; nothing to do.
        return entry;
    };
    let Some(dir) = workdir else {
        return entry;
    };

    // Reconstruct the signed message: the two byte ranges concatenated.
    let [a, b, c, d] = *byte_range;
    let ranges_ok = a >= 0
        && b >= 0
        && c >= 0
        && d >= 0
        && (a as i128 + b as i128) <= c as i128
        && (c as i128 + d as i128) <= input.len() as i128;
    if !ranges_ok {
        issues.push(format!("{label}: /ByteRange is out of bounds"));
        return entry;
    }
    let (a, b, c, d) = (a as usize, b as usize, c as usize, d as usize);
    let mut message = Vec::with_capacity(b + d);
    message.extend_from_slice(&input[a..a + b]);
    message.extend_from_slice(&input[c..c + d]);

    // Temp file names use the signature index, never the field name —
    // `/T` is attacker-controlled and must not reach a path.
    let tag = format!("sig{index}");
    let msg_path = dir.join(format!("{tag}-message.bin"));
    let sig_path = dir.join(format!("{tag}-signature.der"));
    let certs_path = dir.join(format!("{tag}-certs.pem"));
    if std::fs::write(&msg_path, &message).is_err() || std::fs::write(&sig_path, contents).is_err()
    {
        issues.push(format!("{label}: failed to write temporary files"));
        return entry;
    }

    // 1. CMS signature over the byte-range message.
    match run_capture(
        tool,
        &[
            "cms".into(),
            "-verify".into(),
            "-binary".into(),
            "-inform".into(),
            "DER".into(),
            "-in".into(),
            sig_path.as_os_str().to_os_string(),
            "-content".into(),
            msg_path.as_os_str().to_os_string(),
            "-noverify".into(),
        ],
    ) {
        Ok(output) if output.status.success() => entry.digest_valid = Some(true),
        Ok(output) => {
            entry.digest_valid = Some(false);
            issues.push(format!(
                "{label}: CMS verification failed ({})",
                first_line(&output.stderr)
            ));
        }
        Err(detail) => {
            issues.push(format!(
                "{label}: could not run openssl cms -verify: {detail}"
            ));
        }
    }

    // 2. Extract the signer certificate and its subject.
    let signer_cert = extract_signer_cert(tool, &sig_path, &certs_path, &label, issues);
    if let Some(cert_path) = &signer_cert {
        match run_capture(
            tool,
            &[
                "x509".into(),
                "-in".into(),
                cert_path.clone().into_os_string(),
                "-noout".into(),
                "-subject".into(),
                "-nameopt".into(),
                "RFC2253".into(),
            ],
        ) {
            Ok(output) if output.status.success() => {
                entry.signer_subject = Some(
                    String::from_utf8_lossy(&output.stdout)
                        .trim()
                        .trim_start_matches("subject=")
                        .to_string(),
                );
            }
            _ => {}
        }
    }

    // 3. Certificate chain — only when the caller supplied trust anchors.
    if let (Some(cert_path), Some(anchors)) = (&signer_cert, trust_anchors) {
        let ca_path = dir.join(format!("{tag}-ca.pem"));
        if std::fs::write(&ca_path, anchors).is_ok() {
            match run_capture(
                tool,
                &[
                    "verify".into(),
                    "-CAfile".into(),
                    ca_path.clone().into_os_string(),
                    cert_path.clone().into_os_string(),
                ],
            ) {
                Ok(output) if output.status.success() => entry.chain_verified = Some(true),
                Ok(output) => {
                    entry.chain_verified = Some(false);
                    issues.push(format!(
                        "{label}: certificate chain verification failed ({})",
                        first_line(&output.stderr)
                    ));
                }
                Err(detail) => {
                    issues.push(format!("{label}: could not run openssl verify: {detail}"));
                }
            }
        }
    }

    entry
}

/// Run `openssl pkcs7 -print_certs` on a CMS blob and return the path of
/// the first embedded certificate as a standalone PEM file.
fn extract_signer_cert(
    tool: &Path,
    sig_path: &Path,
    certs_path: &Path,
    label: &str,
    issues: &mut Vec<String>,
) -> Option<PathBuf> {
    let output = run_capture(
        tool,
        &[
            "pkcs7".into(),
            "-inform".into(),
            "DER".into(),
            "-in".into(),
            sig_path.as_os_str().to_os_string(),
            "-print_certs".into(),
            "-out".into(),
            certs_path.as_os_str().to_os_string(),
        ],
    )
    .ok()?;
    if !output.status.success() {
        issues.push(format!(
            "{label}: could not extract signer certificate ({})",
            first_line(&output.stderr)
        ));
        return None;
    }
    let pem = std::fs::read(certs_path).ok()?;
    let text = String::from_utf8_lossy(&pem);
    let begin = text.find("-----BEGIN CERTIFICATE-----")?;
    let end = text[begin..].find("-----END CERTIFICATE-----")? + begin;
    let end = end + "-----END CERTIFICATE-----".len();
    let block = &text[begin..end];
    let cert_path = certs_path.with_extension("signer.pem");
    std::fs::write(&cert_path, block.as_bytes()).ok()?;
    Some(cert_path)
}

/// Spawn `tool` with `args`, capturing output. `Err` carries a short
/// spawn-failure description — never used for exit-status failures,
/// which are reported through `Output::status`.
fn run_capture(tool: &Path, args: &[std::ffi::OsString]) -> Result<std::process::Output, String> {
    let mut cmd = std::process::Command::new(tool);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    cmd.output().map_err(|e| format!("failed to spawn: {e}"))
}

/// First line of tool stderr/stdout, trimmed — for issue messages.
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

/// Whether `needle` occurs in `haystack`.
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack.windows(needle.len()).any(|w| w == needle)
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

    #[test]
    fn unsigned_document_reports_no_signatures() {
        let input = pdf_with_pages(&["Hello"]);
        let report = verify_signatures(&input, &unlimited()).unwrap();
        assert_eq!(report.signature_count, 0);
        assert!(report.signatures.is_empty());
        assert!(report.issues.iter().any(|i| i.contains("no signatures")));
    }

    /// `true` when `openssl` is on `PATH` — certificate-dependent tests
    /// are gated on this like the raster/OCR/sign tests.
    fn openssl_available() -> bool {
        find_tool(OPENSSL_TOOL).is_ok()
    }

    /// Create a self-signed PEM cert+key in `dir` via `openssl req` and
    /// return `(cert_pem_bytes, SignCredentials)` for `ops::sign`.
    fn self_signed_credentials(
        dir: &std::path::Path,
    ) -> (
        Vec<u8>,
        crate::crates::pdf_engine::ops::sign::SignCredentials,
    ) {
        use std::process::{Command, Stdio};

        let tool = find_tool(OPENSSL_TOOL).unwrap();
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        let status = Command::new(tool)
            .args(["req", "-x509", "-newkey", "rsa:2048", "-keyout"])
            .arg(&key)
            .args(["-out"])
            .arg(&cert)
            .args(["-days", "1", "-nodes", "-subj", "/CN=Crypto Verify Test"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "openssl req failed: {status}");
        let cert_pem = std::fs::read(&cert).unwrap();
        let creds = crate::crates::pdf_engine::ops::sign::SignCredentials::Pem {
            certificate: cert,
            private_key: key,
            private_key_password: None,
        };
        (cert_pem, creds)
    }

    #[test]
    fn crypto_verify_reports_digest_and_chain() {
        if !openssl_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (cert_pem, creds) = self_signed_credentials(dir.path());
        let input = pdf_with_pages(&["verify me cryptographically"]);
        let signed = crate::crates::pdf_engine::ops::sign::sign(
            &input,
            &creds,
            &crate::crates::pdf_engine::ops::sign::SignOptions::default(),
            &unlimited(),
        )
        .unwrap();

        // Without trust anchors: digest validates, chain is not attempted.
        let report = verify_signatures_crypto(&signed, &unlimited(), None).unwrap();
        assert_eq!(report.signature_count, 1);
        assert_eq!(report.signatures[0].digest_valid, Some(true));
        assert_eq!(report.signatures[0].chain_verified, None);
        assert!(report.signatures[0].signer_subject.is_some());

        // With the self-signed cert as its own trust anchor: chain
        // verification succeeds (`openssl verify` accepts a self-signed
        // cert present in the CAfile).
        let report = verify_signatures_crypto(&signed, &unlimited(), Some(&cert_pem)).unwrap();
        assert_eq!(report.signatures[0].digest_valid, Some(true));
        assert_eq!(report.signatures[0].chain_verified, Some(true));

        // An unrelated trust anchor must fail chain verification —
        // never silently pass.
        let dir2 = tempfile::tempdir().unwrap();
        let (other_ca, _) = self_signed_credentials(dir2.path());
        let report = verify_signatures_crypto(&signed, &unlimited(), Some(&other_ca)).unwrap();
        assert_eq!(report.signatures[0].digest_valid, Some(true));
        assert_eq!(report.signatures[0].chain_verified, Some(false));
    }
}
