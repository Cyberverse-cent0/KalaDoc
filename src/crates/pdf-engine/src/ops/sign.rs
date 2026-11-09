//! Digitally sign a PDF with an X.509 certificate, via the `openssl` CLI.
//!
//! # What this produces
//!
//! A real, verifiable PDF signature: a signature field whose `/V`
//! dictionary carries `/Filter /Adobe.PPKLite`,
//! `/SubFilter /adbe.pkcs7.detached`, a `/ByteRange` array and a `/Contents`
//! hex string holding a **detached CMS/PKCS#7** signature produced by
//! `openssl cms -sign`. The CMS covers the two byte ranges declared in
//! `/ByteRange` (everything except the `/Contents` value itself), which
//! is exactly what PDF readers verify.
//!
//! # How signing works here
//!
//! `lopdf` has no CMS support, so the cryptography is delegated to
//! `openssl` (probed on `PATH` via the same [`find_tool`] pattern as
//! `render`/`ocr`; a missing binary yields [`PdfError::MissingTool`]).
//! The byte-range surgery is done on the serialized file:
//!
//! 1. A signature field + widget annotation is added to the document
//!    (first page) and `/Contents` is filled with a recognizable hex
//!    placeholder of a guessed size.
//! 2. The document is serialized and the `/ByteRange` array is iterated
//!    to a fixed point: the declared ranges must equal the offsets of
//!    the placeholder measured in that same serialization.
//! 3. The two signed byte ranges are concatenated and handed to
//!    `openssl cms -sign` (detached, DER, SHA-256 by default).
//! 4. Because a detached CMS has a fixed size for a given key/certificate,
//!    the placeholder is resized to exactly `2 * cms_len` hex digits and
//!    the process repeats until the placeholder length matches the CMS
//!    length; the CMS hex then replaces the placeholder bytes in place.
//!
//! # Honest limitations
//!
//! - **Whole-document signatures only.** `/ByteRange` always covers the
//!   entire file; there is no per-page or per-region signing. The widget
//!   annotation is placed on the first page purely as the visible
//!   signature appearance.
//! - **Full rewrite.** Signing re-serializes the whole document, which
//!   invalidates every pre-existing signature. [`sign`] therefore
//!   refuses input that already contains signatures — call
//!   [`remove_signatures`] first if that is what you want. (This is also
//!   why real-world PDF signing normally uses *incremental updates*, a
//!   capability `lopdf` does not provide.)
//! - **No RFC 3161 timestamping.** `SignOptions::timestamp_authority`
//!   exists only to make the limitation explicit: setting it returns
//!   [`PdfError::UnsupportedOperation`].
//! - **No certificate-chain or revocation checking at signing time** —
//!   that is the verifier's job (see [`verify_signatures_crypto`]).
//! - Encrypted input is rejected ([`PdfError::Encrypted`]), like every
//!   other transforming op.
//! - Reason/location/contact/signer-name strings are written as PDF
//!   text strings: ASCII as-is, anything else as UTF-16BE hex.
//! - Requires `openssl` on `PATH`; missing → [`PdfError::MissingTool`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use lopdf::{Dictionary, Document, Object, ObjectId, StringFormat};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;
use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, lopdf_err, reject_encrypted, save_pdf};
use super::raster::find_tool;
use super::verify_signature::{find_signatures, is_signature_field};

/// The external tool all signing/verification shells out to.
const OPENSSL_TOOL: &str = "openssl";

/// Distinctive 8-byte pattern repeated to fill the `/Contents`
/// placeholder; its uppercase hex is searched for in the serialized
/// output to locate the byte range that must stay unsigned.
const MARKER: [u8; 8] = [0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE];
const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Initial `/Contents` placeholder size in *decoded* bytes. CMS blobs
/// for RSA-2048 are ~4–16 KB; the loop resizes to the exact CMS length.
const INITIAL_PLACEHOLDER_LEN: usize = 8192;
/// Outer loop bound for placeholder/CMS size convergence.
const MAX_PLACEHOLDER_ATTEMPTS: usize = 8;
/// Inner loop bound for `/ByteRange` offset convergence.
const MAX_BYTES_RANGE_ITERS: usize = 64;
/// Maximum depth followed through field `/Kids` chains (mirrors
/// `verify_signature`).
const MAX_FIELD_DEPTH: usize = 32;
/// Environment variable used to pass key/PKCS#12 passphrases to
/// `openssl` without exposing them in the process argument list.
const SIGN_PASSPHRASE_ENV: &str = "KALADOC_OPENSSL_PASSPHRASE";

/// Signing credentials: either a PEM certificate + private key pair, or
/// a PKCS#12 (`.p12`/`.pfx`) bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SignCredentials {
    /// PEM-encoded signer certificate and PEM-encoded private key.
    ///
    /// `certificate` may contain the leaf plus any intermediates —
    /// `openssl cms -sign` embeds what it is given. `private_key_password`
    /// unlocks an encrypted PEM key; it is passed to `openssl` via an
    /// environment variable, never on the command line.
    Pem {
        /// Path to the PEM certificate (or certificate chain) file.
        certificate: PathBuf,
        /// Path to the PEM private key file.
        private_key: PathBuf,
        /// Passphrase for an encrypted private key, if any.
        private_key_password: Option<String>,
    },
    /// PKCS#12 bundle containing certificate + private key.
    ///
    /// The bundle is converted to PEM in a temp directory with
    /// `openssl pkcs12 -nodes`; `password` is the bundle passphrase
    /// (empty/`None` for unencrypted bundles).
    Pkcs12 {
        /// Path to the `.p12`/`.pfx` file.
        path: PathBuf,
        /// Bundle passphrase, if any.
        password: Option<String>,
    },
}

/// Optional signature-dictionary entries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SignOptions {
    /// Signature `/Reason` (e.g. `"Approved"`).
    pub reason: Option<String>,
    /// Signature `/Location` (e.g. a city or host name).
    pub location: Option<String>,
    /// Signature `/ContactInfo`.
    pub contact_info: Option<String>,
    /// Signer `/Name` shown by viewers; when `None` the entry is omitted
    /// and viewers fall back to the CMS signer identity.
    pub signer_name: Option<String>,
    /// RFC 3161 timestamp authority URL. **Not supported**: setting this
    /// returns [`PdfError::UnsupportedOperation`].
    pub timestamp_authority: Option<String>,
}

/// Decrypted signer material handed to `openssl cms -sign`.
struct SignerMaterial {
    /// PEM certificate file (may also contain the chain).
    certificate: PathBuf,
    /// PEM private key file.
    private_key: PathBuf,
    /// Passphrase for `-inkey`, delivered via `SIGN_PASSPHRASE_ENV`.
    passphrase: Option<String>,
}

/// Sign `input` and return the signed PDF bytes.
///
/// See the module docs for the produced structure and limitations.
/// `pages`-scoped signing does not exist — the signature always covers
/// the whole file.
pub fn sign(
    input: &[u8],
    credentials: &SignCredentials,
    options: &SignOptions,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    let tool = find_tool(OPENSSL_TOOL)?;
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    // Signing rewrites the whole file, which would silently invalidate
    // every existing signature's /ByteRange. Refuse instead.
    let existing = find_signatures(&doc, input.len() as i64);
    if !existing.is_empty() {
        return Err(PdfError::InvalidInput(format!(
            "document already contains {} signature(s); signing rewrites \
             the file and would invalidate them — call remove_signatures \
             first",
            existing.len()
        )));
    }
    if options.timestamp_authority.is_some() {
        return Err(PdfError::UnsupportedOperation(
            "RFC 3161 timestamping is not supported: `openssl cms -sign` \
             cannot embed a timestamp token without a TSA round-trip, \
             which this engine does not implement"
                .to_string(),
        ));
    }

    let workdir = tempfile::tempdir().map_err(|e| PdfError::Io(std::io::Error::other(e)))?;
    let material = prepare_signer(credentials, &tool, workdir.path())?;

    let sig_id = add_signature_field(&mut doc, options)?.1;

    // Outer loop: the /Contents placeholder must be exactly as many hex
    // digits as the CMS signature needs. CMS size is independent of the
    // message contents, so this converges after ~2 rounds.
    let mut placeholder_len = INITIAL_PLACEHOLDER_LEN;
    for _ in 0..MAX_PLACEHOLDER_ATTEMPTS {
        set_signature_placeholder(&mut doc, sig_id, placeholder_len)?;
        let needle = marker_hex(placeholder_len);
        let (bytes, cs, ce) = serialize_locate(&mut doc, sig_id, &needle)?;

        // The signed "message" is the two byte ranges concatenated —
        // exactly what PDF verifiers hash.
        let message = [&bytes[..cs], &bytes[ce..]].concat();
        let msg_path = workdir.path().join("message.bin");
        std::fs::write(&msg_path, &message).map_err(PdfError::Io)?;
        let cms = cms_sign(&tool, &material, &msg_path, workdir.path())?;

        if ce - cs == cms.len() * 2 {
            let mut out = bytes;
            write_hex_upper(&cms, &mut out[cs..ce]);
            return Ok(out);
        }
        placeholder_len = cms.len();
    }

    Err(PdfError::ExternalTool {
        tool: OPENSSL_TOOL.to_string(),
        detail: "CMS signature size did not converge with the /Contents placeholder".to_string(),
    })
}

/// Remove every signature field from `input` and return the cleaned PDF.
///
/// This unlinks signature fields from the AcroForm `/Fields` tree
/// (recursing through `/Kids`), removes their widget annotations from
/// every page's `/Annots`, and drops the `/DocMDP` and `/UR` entries
/// under the catalog's `/Perms`. `/SigFlags` is reset to `0`.
///
/// Like every op here the document is fully re-serialized, so **any
/// signatures not removed by this op would also be invalidated** — this
/// op removes all signatures it can find. Signature field/value objects
/// are left in the file as unreferenced objects (`lopdf` performs no
/// garbage collection); they are inert but their bytes remain until a
/// `compress`/`repair`-style rewrite — note those also do not collect
/// orphans, so the CMS blob may persist in the output.
pub fn remove_signatures(input: &[u8], limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    // Object ids of widget annotations to strip from page /Annots.
    let mut removed_widgets: HashSet<ObjectId> = HashSet::new();

    // 1. Prune signature fields out of the AcroForm /Fields tree. A
    // direct (non-indirect) AcroForm dictionary is promoted to an
    // indirect object first so it can be mutated.
    let acro_id = acroform_id(&doc).or_else(|| promote_direct_acroform(&mut doc));
    if let Some(acro_id) = acro_id {
        let fields_items = {
            let acro = doc.get_dictionary(acro_id).map_err(lopdf_err)?;
            match acro.get(b"Fields") {
                Ok(fields_obj) => match doc.dereference(fields_obj) {
                    Ok((_, Object::Array(items))) => items.clone(),
                    _ => Vec::new(),
                },
                Err(_) => Vec::new(),
            }
        };

        let mut kept = Vec::with_capacity(fields_items.len());
        for item in &fields_items {
            if let Some(kept_item) = prune_field(&mut doc, item, &mut removed_widgets, 0) {
                kept.push(kept_item);
            }
        }

        let acro = doc.get_dictionary_mut(acro_id).map_err(lopdf_err)?;
        acro.set("Fields", Object::Array(kept));
        acro.set("SigFlags", Object::Integer(0));
    }

    // 2. Strip the removed widget annotations from every page's /Annots.
    let page_ids: Vec<ObjectId> = doc.get_pages().into_values().collect();
    for page_id in page_ids {
        strip_annots(&mut doc, page_id, &removed_widgets)?;
    }

    // 3. Drop signature permission references under the catalog /Perms.
    let perms_obj = doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"Perms").ok())
        .cloned();
    if let Some(perms) = perms_obj {
        match perms {
            Object::Reference(id) => {
                if let Ok(dict) = doc.get_dictionary_mut(id) {
                    dict.remove(b"DocMDP");
                    dict.remove(b"UR");
                }
            }
            Object::Dictionary(_) => {
                if let Ok(catalog) = doc.catalog_mut()
                    && let Ok(Object::Dictionary(dict)) = catalog.get_mut(b"Perms")
                {
                    dict.remove(b"DocMDP");
                    dict.remove(b"UR");
                }
            }
            _ => {}
        }
    }

    save_pdf(&mut doc)
}

// ---------------------------------------------------------------------
// Signing pipeline
// ---------------------------------------------------------------------

/// Validate credential paths and produce the material `openssl` needs.
/// For PKCS#12 this extracts a combined PEM into `workdir` first.
fn prepare_signer(
    credentials: &SignCredentials,
    tool: &Path,
    workdir: &Path,
) -> PdfResult<SignerMaterial> {
    match credentials {
        SignCredentials::Pem {
            certificate,
            private_key,
            private_key_password,
        } => {
            for (label, path) in [("certificate", certificate), ("private key", private_key)] {
                if !path.is_file() {
                    return Err(PdfError::InvalidInput(format!(
                        "{label} file not found: {}",
                        path.display()
                    )));
                }
            }
            Ok(SignerMaterial {
                certificate: certificate.clone(),
                private_key: private_key.clone(),
                passphrase: private_key_password.clone(),
            })
        }
        SignCredentials::Pkcs12 { path, password } => {
            if !path.is_file() {
                return Err(PdfError::InvalidInput(format!(
                    "PKCS#12 file not found: {}",
                    path.display()
                )));
            }
            let pem_path = workdir.join("signer.pem");
            let mut cmd = Command::new(tool);
            cmd.arg("pkcs12")
                .arg("-in")
                .arg(path)
                .arg("-nodes")
                .arg("-out")
                .arg(&pem_path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            if let Some(pw) = password {
                cmd.env(SIGN_PASSPHRASE_ENV, pw)
                    .arg("-passin")
                    .arg(format!("env:{SIGN_PASSPHRASE_ENV}"));
            }
            let status = cmd.status().map_err(|e| PdfError::ExternalTool {
                tool: OPENSSL_TOOL.to_string(),
                detail: format!("failed to spawn: {e}"),
            })?;
            check_status("openssl pkcs12", status)?;
            // The combined PEM holds both certificate and key; `cms
            // -sign` accepts it for -signer and -inkey alike.
            Ok(SignerMaterial {
                certificate: pem_path.clone(),
                private_key: pem_path,
                passphrase: None,
            })
        }
    }
}

/// Run `openssl cms -sign` over `msg_path`, returning the DER CMS blob.
fn cms_sign(
    tool: &Path,
    material: &SignerMaterial,
    msg_path: &Path,
    workdir: &Path,
) -> PdfResult<Vec<u8>> {
    let sig_path = workdir.join("signature.der");
    let mut cmd = Command::new(tool);
    cmd.arg("cms")
        .arg("-sign")
        .arg("-binary")
        .arg("-md")
        .arg("sha256")
        .arg("-in")
        .arg(msg_path)
        .arg("-signer")
        .arg(&material.certificate)
        .arg("-inkey")
        .arg(&material.private_key)
        .arg("-outform")
        .arg("DER")
        .arg("-out")
        .arg(&sig_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(pw) = &material.passphrase {
        cmd.env(SIGN_PASSPHRASE_ENV, pw)
            .arg("-passin")
            .arg(format!("env:{SIGN_PASSPHRASE_ENV}"));
    }
    let status = cmd.status().map_err(|e| PdfError::ExternalTool {
        tool: OPENSSL_TOOL.to_string(),
        detail: format!("failed to spawn: {e}"),
    })?;
    check_status("openssl cms -sign", status)?;
    std::fs::read(&sig_path).map_err(PdfError::Io)
}

fn check_status(tool: &str, status: std::process::ExitStatus) -> PdfResult<()> {
    if status.success() {
        Ok(())
    } else {
        Err(PdfError::ExternalTool {
            tool: OPENSSL_TOOL.to_string(),
            detail: format!("{tool} exited with {status}"),
        })
    }
}

// ---------------------------------------------------------------------
// Document construction
// ---------------------------------------------------------------------

/// Add the signature field + signature value dictionary and register
/// the field in the AcroForm. Returns `(field_id, sig_dict_id)`.
fn add_signature_field(
    doc: &mut Document,
    options: &SignOptions,
) -> PdfResult<(ObjectId, ObjectId)> {
    // Widget goes on the first page.
    let page_id = *doc
        .get_pages()
        .values()
        .next()
        .ok_or_else(|| PdfError::InvalidInput("document has no pages".to_string()))?;
    let rect = signature_rect(doc, page_id);

    let acro_id = ensure_acroform(doc)?;
    let field_name = unique_field_name(doc, acro_id);

    // Signature value dictionary. /ByteRange and /Contents start as
    // placeholders and are finalized by the serialize/sign loop.
    let mut sig = Dictionary::new();
    sig.set("Type", "Sig");
    sig.set("Filter", "Adobe.PPKLite");
    sig.set("SubFilter", "adbe.pkcs7.detached");
    sig.set(
        "ByteRange",
        Object::Array(vec![0.into(), 0.into(), 0.into(), 0.into()]),
    );
    sig.set(
        "Contents",
        Object::String(
            marker_bytes(INITIAL_PLACEHOLDER_LEN),
            StringFormat::Hexadecimal,
        ),
    );
    sig.set("M", Object::string_literal(pdf_date_now()));
    if let Some(reason) = &options.reason {
        sig.set("Reason", pdf_text(reason));
    }
    if let Some(location) = &options.location {
        sig.set("Location", pdf_text(location));
    }
    if let Some(contact) = &options.contact_info {
        sig.set("ContactInfo", pdf_text(contact));
    }
    if let Some(name) = &options.signer_name {
        sig.set("Name", pdf_text(name));
    }
    let sig_id = doc.add_object(Object::Dictionary(sig));

    // Field dictionary merged with its widget annotation
    // (/Subtype /Widget + /Rect + /P on the field itself).
    let mut field = Dictionary::new();
    field.set("FT", "Sig");
    field.set("T", Object::string_literal(field_name));
    field.set("F", 4i64); // print flag
    field.set("Subtype", "Widget");
    field.set(
        "Rect",
        Object::Array(rect.iter().map(|v| Object::Real(*v)).collect()),
    );
    field.set("P", Object::Reference(page_id));
    field.set("V", Object::Reference(sig_id));
    let field_id = doc.add_object(Object::Dictionary(field));

    // Register in AcroForm /Fields and set /SigFlags bit 0.
    let mut items = {
        let acro = doc.get_dictionary(acro_id).map_err(lopdf_err)?;
        match acro.get(b"Fields") {
            Ok(fields_obj) => match doc.dereference(fields_obj) {
                Ok((_, Object::Array(items))) => items.clone(),
                _ => Vec::new(),
            },
            Err(_) => Vec::new(),
        }
    };
    items.push(Object::Reference(field_id));
    let acro = doc.get_dictionary_mut(acro_id).map_err(lopdf_err)?;
    acro.set("Fields", Object::Array(items));
    acro.set("SigFlags", Object::Integer(1));

    // Register the widget in the page's /Annots.
    let mut annots = {
        let page = doc.get_dictionary(page_id).map_err(lopdf_err)?;
        match page.get(b"Annots") {
            Ok(annots_obj) => match doc.dereference(annots_obj) {
                Ok((_, Object::Array(items))) => items.clone(),
                _ => Vec::new(),
            },
            Err(_) => Vec::new(),
        }
    };
    annots.push(Object::Reference(field_id));
    let page = doc.get_dictionary_mut(page_id).map_err(lopdf_err)?;
    page.set("Annots", Object::Array(annots));

    Ok((field_id, sig_id))
}

/// Return the AcroForm dictionary's object id, creating one (as an
/// indirect object on the catalog) when absent or direct.
fn ensure_acroform(doc: &mut Document) -> PdfResult<ObjectId> {
    if let Some(id) = acroform_id(doc) {
        return Ok(id);
    }
    // Promote a direct AcroForm dictionary to indirect, or create one.
    let existing = doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"AcroForm").ok())
        .cloned();
    let acro_id = match existing {
        Some(Object::Dictionary(d)) => doc.add_object(Object::Dictionary(d)),
        _ => doc.add_object(Object::Dictionary(Dictionary::new())),
    };
    doc.catalog_mut()
        .map_err(lopdf_err)?
        .set("AcroForm", Object::Reference(acro_id));
    Ok(acro_id)
}

/// The AcroForm dictionary's object id when it exists as an indirect
/// object. A *direct* AcroForm dictionary returns `None` — callers that
/// mutate must go through [`ensure_acroform`] or
/// [`promote_direct_acroform`] first.
fn acroform_id(doc: &Document) -> Option<ObjectId> {
    let catalog = doc.catalog().ok()?;
    match catalog.get(b"AcroForm").ok()? {
        Object::Reference(id) => Some(*id),
        _ => None,
    }
}

/// Promote a *direct* AcroForm dictionary on the catalog to an indirect
/// object and return its new id. Returns `None` when there is no
/// AcroForm at all.
fn promote_direct_acroform(doc: &mut Document) -> Option<ObjectId> {
    let dict = doc
        .catalog()
        .ok()?
        .get(b"AcroForm")
        .ok()?
        .as_dict()
        .ok()?
        .clone();
    let id = doc.add_object(Object::Dictionary(dict));
    doc.catalog_mut()
        .ok()?
        .set("AcroForm", Object::Reference(id));
    Some(id)
}

/// A `Signature1`, `Signature2`, ... field name not already used by a
/// top-level AcroForm field.
fn unique_field_name(doc: &Document, acro_id: ObjectId) -> String {
    let mut used = HashSet::new();
    if let Ok(acro) = doc.get_dictionary(acro_id)
        && let Ok(fields_obj) = acro.get(b"Fields")
        && let Ok((_, fields)) = doc.dereference(fields_obj)
        && let Object::Array(items) = fields
    {
        for item in items {
            let Ok((_, field)) = doc.dereference(item) else {
                continue;
            };
            let Ok(dict) = field.as_dict() else {
                continue;
            };
            if let Ok(t) = dict.get(b"T")
                && let Ok(name) = lopdf::decode_text_string(t)
            {
                used.insert(name);
            }
        }
    }
    for i in 1.. {
        let candidate = format!("Signature{i}");
        if !used.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

/// Widget rectangle on `page_id`: bottom-left corner of the MediaBox,
/// 120x24 pt (clamped to the page size).
fn signature_rect(doc: &Document, page_id: ObjectId) -> [f32; 4] {
    let media_box = doc
        .get_dictionary(page_id)
        .ok()
        .and_then(|p| p.get(b"MediaBox").ok())
        .and_then(|o| doc.dereference(o).ok())
        .and_then(|(_, o)| o.as_array().ok())
        .and_then(|arr| {
            if arr.len() == 4 {
                let nums: Option<Vec<f32>> = arr
                    .iter()
                    .map(|o| match o {
                        Object::Integer(i) => Some(*i as f32),
                        Object::Real(r) => Some(*r),
                        _ => None,
                    })
                    .collect();
                nums
            } else {
                None
            }
        });
    let [x0, y0, x1, y1]: [f32; 4] = media_box
        .and_then(|v| <[f32; 4]>::try_from(v.as_slice()).ok())
        .unwrap_or([0.0, 0.0, 612.0, 792.0]);
    let w = (x1 - x0).max(1.0);
    let h = (y1 - y0).max(1.0);
    [x0, y0, x0 + w.min(120.0), y0 + h.min(24.0)]
}

// ---------------------------------------------------------------------
// Byte-range surgery
// ---------------------------------------------------------------------

/// Set the `/Contents` placeholder of the signature dictionary to
/// `len` decoded marker bytes (written as a hex string of `2*len`
/// digits).
fn set_signature_placeholder(doc: &mut Document, sig_id: ObjectId, len: usize) -> PdfResult<()> {
    let dict = doc.get_dictionary_mut(sig_id).map_err(lopdf_err)?;
    dict.set(
        "Contents",
        Object::String(marker_bytes(len), StringFormat::Hexadecimal),
    );
    Ok(())
}

/// Serialize `doc` and locate the `/Contents` placeholder, iterating
/// `/ByteRange` to a fixed point. Returns the serialized bytes and the
/// half-open offset range `[cs, ce)` of the placeholder's hex digits —
/// the bytes excluded from the signature.
fn serialize_locate(
    doc: &mut Document,
    sig_id: ObjectId,
    needle: &[u8],
) -> PdfResult<(Vec<u8>, usize, usize)> {
    let mut guess = [0i64; 4];
    for _ in 0..MAX_BYTES_RANGE_ITERS {
        {
            let dict = doc.get_dictionary_mut(sig_id).map_err(lopdf_err)?;
            dict.set(
                "ByteRange",
                Object::Array(guess.iter().map(|v| Object::Integer(*v)).collect()),
            );
        }
        let bytes = save_pdf(doc)?;
        let hits = find_subslice_all(&bytes, needle);
        let &[pos] = hits.as_slice() else {
            return Err(PdfError::InvalidInput(format!(
                "internal error: signature placeholder found {} times in \
                 the serialized PDF",
                hits.len()
            )));
        };
        let ce = pos + needle.len();
        let actual = [0, pos as i64, ce as i64, (bytes.len() - ce) as i64];
        if actual == guess {
            return Ok((bytes, pos, ce));
        }
        guess = actual;
    }
    Err(PdfError::InvalidInput(
        "internal error: /ByteRange offsets did not converge".to_string(),
    ))
}

// ---------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------

/// `len` bytes of the repeating [`MARKER`] pattern.
fn marker_bytes(len: usize) -> Vec<u8> {
    MARKER.iter().cycle().take(len).copied().collect()
}

/// Uppercase hex encoding of the [`MARKER`] pattern repeated `len` times
/// — the exact byte string `lopdf` writes for the placeholder.
fn marker_hex(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len * 2);
    for byte in marker_bytes(len) {
        out.push(HEX[(byte >> 4) as usize]);
        out.push(HEX[(byte & 0x0F) as usize]);
    }
    out
}

/// Write `bytes` as uppercase hex into `out` (which must be exactly
/// `2 * bytes.len()` long).
fn write_hex_upper(bytes: &[u8], out: &mut [u8]) {
    debug_assert_eq!(out.len(), bytes.len() * 2);
    for (i, &byte) in bytes.iter().enumerate() {
        out[2 * i] = HEX[(byte >> 4) as usize];
        out[2 * i + 1] = HEX[(byte & 0x0F) as usize];
    }
}

/// All start offsets of `needle` in `haystack` (search stops after the
/// second hit — callers only need uniqueness).
fn find_subslice_all(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    if needle.is_empty() || haystack.len() < needle.len() {
        return out;
    }
    let mut start = 0;
    while let Some(pos) = haystack[start..]
        .windows(needle.len())
        .position(|w| w == needle)
    {
        out.push(start + pos);
        if out.len() > 1 {
            break;
        }
        start += pos + 1;
    }
    out
}

/// A PDF text string: literal for ASCII, UTF-16BE hex otherwise.
fn pdf_text(s: &str) -> Object {
    if s.is_ascii() {
        return Object::string_literal(s);
    }
    let mut bytes = vec![0xFE, 0xFF]; // UTF-16BE BOM
    for unit in s.encode_utf16() {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    Object::String(bytes, StringFormat::Hexadecimal)
}

/// Current UTC time as a PDF date string (`D:YYYYMMDDHHmmSSZ`).
fn pdf_date_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "D:{year:04}{month:02}{day:02}{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 → proleptic Gregorian (y, m, d).
/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------
// Signature removal helpers
// ---------------------------------------------------------------------

/// Recursively prune signature fields from a `/Fields` entry.
///
/// Returns `Some(object)` for entries to keep — the original object, or
/// a modified direct dictionary when a kept field's `/Kids` changed —
/// and `None` when the whole subtree is signature material. Widget
/// annotation object ids of dropped subtrees are collected into
/// `removed_widgets` for the page `/Annots` cleanup.
fn prune_field(
    doc: &mut Document,
    obj: &Object,
    removed_widgets: &mut HashSet<ObjectId>,
    depth: usize,
) -> Option<Object> {
    if depth > MAX_FIELD_DEPTH {
        return Some(obj.clone());
    }
    let Ok((id_opt, resolved)) = doc.dereference(obj) else {
        return Some(obj.clone());
    };
    let Ok(dict) = resolved.as_dict() else {
        return Some(obj.clone());
    };

    if is_signature_field(doc, dict) {
        collect_widget_ids(doc, obj, removed_widgets, 0);
        return None;
    }

    // Non-signature field: prune signature kids, if any.
    let kids_obj = dict.get(b"Kids").ok().cloned();
    let field_id = id_opt;
    let Some(kids_obj) = kids_obj else {
        return Some(obj.clone());
    };
    let kids_items = match doc.dereference(&kids_obj) {
        Ok((_, Object::Array(items))) => items.clone(),
        _ => return Some(obj.clone()),
    };

    let mut kept = Vec::with_capacity(kids_items.len());
    let mut changed = false;
    for kid in &kids_items {
        match prune_field(doc, kid, removed_widgets, depth + 1) {
            Some(kept_kid) => kept.push(kept_kid),
            None => changed = true,
        }
    }
    if !changed {
        return Some(obj.clone());
    }

    // Write the filtered /Kids back. Indirect fields are mutated in
    // place; a direct field dictionary is cloned and returned modified.
    if let Some(fid) = field_id {
        if let Ok(d) = doc.get_dictionary_mut(fid) {
            if kept.is_empty() {
                d.remove(b"Kids");
            } else {
                d.set("Kids", Object::Array(kept));
            }
        }
        Some(obj.clone())
    } else {
        let Ok((_, resolved)) = doc.dereference(obj) else {
            return Some(obj.clone());
        };
        let Ok(d) = resolved.as_dict() else {
            return Some(obj.clone());
        };
        let mut nd = d.clone();
        if kept.is_empty() {
            nd.remove(b"Kids");
        } else {
            nd.set("Kids", Object::Array(kept));
        }
        Some(Object::Dictionary(nd))
    }
}

/// Collect every object id in a dropped signature subtree that could
/// appear in a page `/Annots` array: the field itself plus all `/Kids`
/// descendants (widget annotations).
fn collect_widget_ids(doc: &Document, obj: &Object, removed: &mut HashSet<ObjectId>, depth: usize) {
    if depth > MAX_FIELD_DEPTH {
        return;
    }
    let Ok((id_opt, resolved)) = doc.dereference(obj) else {
        return;
    };
    if let Some(id) = id_opt {
        removed.insert(id);
    }
    let Ok(dict) = resolved.as_dict() else {
        return;
    };
    if let Ok(kids) = dict.get_deref(b"Kids", doc)
        && let Object::Array(items) = kids
    {
        for kid in items {
            collect_widget_ids(doc, kid, removed, depth + 1);
        }
    }
}

/// Remove signature widgets from one page's `/Annots` array. Entries
/// are dropped when their object id was collected during field pruning
/// or when the referenced dictionary still looks like a signature
/// (covers signature fields that were never registered in `/Fields`).
fn strip_annots(
    doc: &mut Document,
    page_id: ObjectId,
    removed: &HashSet<ObjectId>,
) -> PdfResult<()> {
    let annots_obj = {
        let page = doc.get_dictionary(page_id).map_err(lopdf_err)?;
        page.get(b"Annots").ok().cloned()
    };
    let Some(annots_obj) = annots_obj else {
        return Ok(());
    };

    let kept = {
        let Ok((_, resolved)) = doc.dereference(&annots_obj) else {
            return Ok(());
        };
        let Object::Array(items) = resolved else {
            return Ok(());
        };
        let mut kept = Vec::with_capacity(items.len());
        let mut changed = false;
        for item in items {
            let drop_it = match item {
                Object::Reference(id) => removed.contains(id),
                _ => false,
            } || doc
                .dereference(item)
                .ok()
                .and_then(|(_, o)| o.as_dict().ok())
                .is_some_and(|d| is_signature_field(doc, d));
            if drop_it {
                changed = true;
            } else {
                kept.push(item.clone());
            }
        }
        if !changed {
            return Ok(());
        }
        kept
    };

    match annots_obj {
        Object::Reference(arr_id) => {
            if let Ok(Object::Array(arr)) = doc.get_object_mut(arr_id) {
                *arr = kept;
            }
        }
        _ => {
            let page = doc.get_dictionary_mut(page_id).map_err(lopdf_err)?;
            if let Ok(Object::Array(arr)) = page.get_mut(b"Annots") {
                *arr = kept;
            }
        }
    }
    Ok(())
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

    fn pem_credentials() -> SignCredentials {
        SignCredentials::Pem {
            certificate: PathBuf::from("/nonexistent/cert.pem"),
            private_key: PathBuf::from("/nonexistent/key.pem"),
            private_key_password: None,
        }
    }

    #[test]
    fn missing_openssl_is_missing_tool() {
        if find_tool(OPENSSL_TOOL).is_ok() {
            return; // environment has openssl; nothing to assert here
        }
        let limits = unlimited();
        let err = sign(
            b"%PDF-1.4",
            &pem_credentials(),
            &SignOptions::default(),
            &limits,
        )
        .unwrap_err();
        assert!(matches!(err, PdfError::MissingTool(_)));
    }

    #[test]
    fn nonexistent_credential_files_are_invalid_input() {
        if find_tool(OPENSSL_TOOL).is_err() {
            return;
        }
        let input = pdf_with_pages(&["Hello"]);
        let err = sign(
            &input,
            &pem_credentials(),
            &SignOptions::default(),
            &unlimited(),
        )
        .unwrap_err();
        assert!(matches!(err, PdfError::InvalidInput(_)));
    }

    #[test]
    fn timestamp_authority_is_unsupported() {
        // Checked before credentials are touched? — order: tool probe,
        // load, existing-sig check, then timestamp check. With a valid
        // PDF and no signatures this reaches the timestamp error only
        // when openssl exists; otherwise MissingTool fires first.
        if find_tool(OPENSSL_TOOL).is_err() {
            return;
        }
        let input = pdf_with_pages(&["Hello"]);
        let options = SignOptions {
            timestamp_authority: Some("http://tsa.example".to_string()),
            ..Default::default()
        };
        // pem_credentials paths don't exist, but the timestamp check
        // runs before prepare_signer, so UnsupportedOperation wins.
        let err = sign(&input, &pem_credentials(), &options, &unlimited()).unwrap_err();
        assert!(matches!(err, PdfError::UnsupportedOperation(_)));
    }

    #[test]
    fn remove_signatures_on_unsigned_doc_is_stable() {
        let input = pdf_with_pages(&["Hello"]);
        let out = remove_signatures(&input, &unlimited()).unwrap();
        // Re-written but still a valid PDF with the same page count.
        let report =
            crate::crates::pdf_engine::ops::verify_signature::verify_signatures(&out, &unlimited())
                .unwrap();
        assert_eq!(report.signature_count, 0);
    }

    /// `true` when `openssl` is on `PATH` — cert-dependent tests are
    /// gated on this like the raster/OCR tests gate on their tools.
    fn openssl_available() -> bool {
        find_tool(OPENSSL_TOOL).is_ok()
    }

    /// Create a self-signed PEM cert+key in `dir` via `openssl req`.
    fn make_self_signed(dir: &Path) -> SignCredentials {
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        let status = Command::new(find_tool(OPENSSL_TOOL).unwrap())
            .args(["req", "-x509", "-newkey", "rsa:2048", "-keyout"])
            .arg(&key)
            .args(["-out"])
            .arg(&cert)
            .args(["-days", "1", "-nodes", "-subj", "/CN=KalaDoc Test Signer"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "openssl req failed: {status}");
        SignCredentials::Pem {
            certificate: cert,
            private_key: key,
            private_key_password: None,
        }
    }

    #[test]
    fn sign_verify_remove_roundtrip() {
        if !openssl_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let creds = make_self_signed(dir.path());
        let input = pdf_with_pages(&["please sign me"]);
        let options = SignOptions {
            reason: Some("unit test".to_string()),
            location: Some("testbed".to_string()),
            signer_name: Some(" KalaDoc Tester ".to_string()),
            ..Default::default()
        };

        let signed = sign(&input, &creds, &options, &unlimited()).unwrap();

        // Structural report sees one signature with sane byte ranges.
        let report =
            super::super::verify_signature::verify_signatures(&signed, &unlimited()).unwrap();
        assert_eq!(report.signature_count, 1);
        assert!(report.signatures[0].byte_range_covers_file);
        assert!(report.signatures[0].contents_present);
        assert_eq!(
            report.signatures[0].sub_filter.as_deref(),
            Some("adbe.pkcs7.detached")
        );

        // Cryptographic verification: the CMS validates over the ranges.
        let crypto =
            super::super::verify_signature::verify_signatures_crypto(&signed, &unlimited(), None)
                .unwrap();
        assert_eq!(crypto.signature_count, 1);
        assert_eq!(crypto.signatures[0].digest_valid, Some(true));
        assert!(crypto.signatures[0].signer_subject.is_some());

        // Tampering with signed content must break verification: mutate
        // a byte inside the /Reason string — inside the signed region but
        // outside /Contents, so the file stays parseable.
        let mut tampered = signed.clone();
        let needle = b"unit test";
        let pos = tampered
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("/Reason string present in signed file");
        tampered[pos] ^= 0x01;
        let bad =
            super::super::verify_signature::verify_signatures_crypto(&tampered, &unlimited(), None)
                .unwrap();
        assert_eq!(bad.signatures[0].digest_valid, Some(false));

        // Removal strips the signature; the result can be re-signed.
        let cleaned = remove_signatures(&signed, &unlimited()).unwrap();
        let after =
            super::super::verify_signature::verify_signatures(&cleaned, &unlimited()).unwrap();
        assert_eq!(after.signature_count, 0);
        let resigned = sign(&cleaned, &creds, &SignOptions::default(), &unlimited()).unwrap();
        let crypto2 =
            super::super::verify_signature::verify_signatures_crypto(&resigned, &unlimited(), None)
                .unwrap();
        assert_eq!(crypto2.signatures[0].digest_valid, Some(true));
    }

    #[test]
    fn sign_with_pkcs12_bundle() {
        if !openssl_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let tool = find_tool(OPENSSL_TOOL).unwrap();
        // Self-signed cert+key, then pack into a password-protected p12.
        let key = dir.path().join("key.pem");
        let cert = dir.path().join("cert.pem");
        let p12 = dir.path().join("bundle.p12");
        let ok = Command::new(&tool)
            .args(["req", "-x509", "-newkey", "rsa:2048", "-keyout"])
            .arg(&key)
            .args(["-out"])
            .arg(&cert)
            .args(["-days", "1", "-nodes", "-subj", "/CN=P12 Signer"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let ok = Command::new(&tool)
            .args(["pkcs12", "-export", "-inkey"])
            .arg(&key)
            .args(["-in"])
            .arg(&cert)
            .args(["-out"])
            .arg(&p12)
            .args(["-passout", "pass:s3cret"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok);

        let creds = SignCredentials::Pkcs12 {
            path: p12,
            password: Some("s3cret".to_string()),
        };
        let input = pdf_with_pages(&["p12 sign me"]);
        let signed = sign(&input, &creds, &SignOptions::default(), &unlimited()).unwrap();
        let crypto =
            super::super::verify_signature::verify_signatures_crypto(&signed, &unlimited(), None)
                .unwrap();
        assert_eq!(crypto.signatures[0].digest_valid, Some(true));
    }

    #[test]
    fn sign_rejects_already_signed_document() {
        if !openssl_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let creds = make_self_signed(dir.path());
        let input = pdf_with_pages(&["sign me"]);
        let signed = sign(&input, &creds, &SignOptions::default(), &unlimited()).unwrap();
        let err = sign(&signed, &creds, &SignOptions::default(), &unlimited()).unwrap_err();
        assert!(matches!(err, PdfError::InvalidInput(_)));
    }

    #[test]
    fn pdf_date_format() {
        let d = pdf_date_now();
        assert!(d.starts_with("D:"));
        assert!(d.ends_with('Z'));
        assert_eq!(d.len(), 17); // D: + 14 digits + Z
    }

    #[test]
    fn marker_roundtrip() {
        let len = 100;
        let bytes = marker_bytes(len);
        let hex = marker_hex(len);
        assert_eq!(hex.len(), len * 2);
        let mut decoded = Vec::new();
        for pair in hex.chunks(2) {
            decoded.push(u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap());
        }
        assert_eq!(decoded, bytes);
    }
}
