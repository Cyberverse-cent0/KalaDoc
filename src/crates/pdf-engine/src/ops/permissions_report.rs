//! Encryption permission report — read-only inspection of the
//! document's `/Encrypt` dictionary.
//!
//! Unlike the transforming ops, this reports *what the file claims*
//! about access control; it never tries to decrypt anything. For an
//! encrypted input the trailer's `/Encrypt` dictionary is still
//! plaintext (only document strings and streams are encrypted), so its
//! `/Filter`, `/V`, `/R`, `/P` and the `/O`/`/U` password-validation
//! entries can be read directly.
//!
//! Honesty notes:
//!
//! - `/U` and `/O` presence only proves the Standard security handler
//!   stored *a* user/owner password validation string. Whether the user
//!   password is empty (i.e. the file opens without a prompt) cannot be
//!   determined without running the password algorithms — so
//!   `user_password_required` really means "a user-password entry
//!   exists", and `owner_password_set` means "an owner-password entry
//!   exists". This op does not attempt decryption.
//! - The `/P` bits are a *request* to the reader, not enforcement. Any
//!   tool can strip `/Encrypt` entirely and recover the content, so the
//!   reported flags describe declared permissions only.

use lopdf::{Dictionary, Document};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::{doc_is_encrypted, load_pdf};

/// The four user-facing permission flags decoded from `/P`.
///
/// Bit positions follow PDF 32000-1:2008 Table 22 (1-indexed bit
/// numbers, so bit *n* is mask `1 << (n - 1)`):
///
/// | bit | mask | permission                          |
/// |-----|------|-------------------------------------|
/// | 3   | 0x4  | print (low quality if bit 12 clear) |
/// | 4   | 0x8  | modify contents                     |
/// | 5   | 0x10 | copy/extract text and graphics      |
/// | 6   | 0x20 | add annotations / fill forms        |
///
/// Reserved bits (7–32) are ignored; `/P` is a signed 32-bit integer so
/// high bits are usually set and the value is negative — only the low
/// six bits are meaningful here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedPermissions {
    /// Bit 3: printing is allowed.
    pub print: bool,
    /// Bit 4: modifying contents is allowed.
    pub modify: bool,
    /// Bit 5: copying/extracting text and graphics is allowed.
    pub copy: bool,
    /// Bit 6: adding annotations and filling form fields is allowed.
    pub annotations: bool,
}

/// Encryption-handler metadata read verbatim from `/Encrypt`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptionDetails {
    /// `/Filter` — security handler name (e.g. `Standard`). `None` when
    /// the entry is missing or not a name.
    pub filter: Option<String>,
    /// `/V` — crypt-filter dictionary format version.
    pub v: Option<i64>,
    /// `/R` — revision of the standard security handler algorithm.
    pub r: Option<i64>,
    /// `/EncryptMetadata` — whether document metadata is encrypted.
    /// `None` when absent (defaults to true per spec).
    pub encrypt_metadata: Option<bool>,
}

/// Structured report returned by [`permissions_report`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionsReport {
    /// Whether the trailer carries an `/Encrypt` entry.
    pub encrypted: bool,
    /// Whether the `/Encrypt` dictionary contains a `/U` entry — the
    /// user-password validation string. This does **not** prove a
    /// password prompt will appear: an empty user password also
    /// produces a `/U` entry, and verifying it would require decryption,
    /// which this op never attempts. Always `false` when unencrypted.
    pub user_password_required: bool,
    /// Whether the `/Encrypt` dictionary contains an `/O` entry — the
    /// owner-password validation string. Same caveat as
    /// [`Self::user_password_required`]: presence is reported, not
    /// whether a distinct owner password was set.
    pub owner_password_set: bool,
    /// Permissions decoded from `/P`. `Some` only for encrypted
    /// documents that carry a readable integer `/P`; `None` for
    /// unencrypted input (no permissions apply) or when `/P` is
    /// missing/malformed.
    pub permissions: Option<ReportedPermissions>,
    /// Encryption-handler fields (`/Filter`, `/V`, `/R`,
    /// `/EncryptMetadata`). `None` when unencrypted.
    pub encryption: Option<EncryptionDetails>,
    /// The document's PDF version (e.g. `"1.7"`).
    pub pdf_version: Option<String>,
    /// Non-fatal caveats discovered while reading the file.
    pub warnings: Vec<String>,
}

/// Report the declared encryption state and permission bits of `input`.
///
/// Input-side [`EngineLimits`] are enforced via [`load_pdf`]; encrypted
/// documents are *not* rejected because their `/Encrypt` dictionary is
/// readable without decrypting. No decryption is attempted and no
/// passwords are verified.
pub fn permissions_report(input: &[u8], limits: &EngineLimits) -> PdfResult<PermissionsReport> {
    let doc = load_pdf(input, limits)?;
    let encrypted = doc_is_encrypted(&doc);

    if !encrypted {
        return Ok(PermissionsReport {
            encrypted: false,
            user_password_required: false,
            owner_password_set: false,
            permissions: None,
            encryption: None,
            pdf_version: Some(doc.version.clone()),
            warnings: Vec::new(),
        });
    }

    let mut warnings = Vec::new();
    let enc_dict = encrypt_dict(&doc);

    if enc_dict.is_none() {
        warnings.push(
            "trailer has an /Encrypt entry but it is not a resolvable dictionary; \
             permission bits could not be read"
                .to_string(),
        );
    }

    let permissions = enc_dict.and_then(|dict| dict_i64(dict, b"P")).map(|p| {
        let p = p as u32 as u64; // sign-extend irrelevant: only low 6 bits used
        ReportedPermissions {
            print: p & 0x4 != 0,
            modify: p & 0x8 != 0,
            copy: p & 0x10 != 0,
            annotations: p & 0x20 != 0,
        }
    });
    if encrypted && permissions.is_none() {
        warnings.push(
            "encrypted document has no readable /P permission integer; \
             permissions reported as unknown"
                .to_string(),
        );
    }

    let encryption = enc_dict.map(|dict| EncryptionDetails {
        filter: dict_name(dict, b"Filter"),
        v: dict_i64(dict, b"V"),
        r: dict_i64(dict, b"R"),
        encrypt_metadata: dict_bool(dict, b"EncryptMetadata"),
    });

    if let Some(details) = &encryption
        && details.filter.as_deref() != Some("Standard")
    {
        warnings.push(format!(
            "security handler /Filter is {:?}, not /Standard; /O /U /P semantics \
             may differ and no password verification was attempted",
            details.filter
        ));
    }

    warnings.push(
        "passwords were not verified (no decryption attempted): /U and /O \
         presence only shows validation entries exist, and an empty user \
         password cannot be ruled out"
            .to_string(),
    );
    warnings.push(
        "permission bits are advisory only — any tool can remove the \
         /Encrypt dictionary and recover the full content"
            .to_string(),
    );

    Ok(PermissionsReport {
        encrypted: true,
        user_password_required: enc_dict.is_some_and(|d| d.get(b"U").is_ok()),
        owner_password_set: enc_dict.is_some_and(|d| d.get(b"O").is_ok()),
        permissions,
        encryption,
        pdf_version: Some(doc.version.clone()),
        warnings,
    })
}

/// Resolve the trailer's `/Encrypt` entry to its dictionary, whether it
/// is a direct dictionary or an indirect reference.
fn encrypt_dict<'a>(doc: &'a Document) -> Option<&'a Dictionary> {
    let object = doc.trailer.get(b"Encrypt").ok()?;
    let (_, resolved) = doc.dereference(object).ok()?;
    resolved.as_dict().ok()
}

fn dict_i64(dict: &Dictionary, key: &[u8]) -> Option<i64> {
    dict.get(key).ok()?.as_i64().ok()
}

fn dict_bool(dict: &Dictionary, key: &[u8]) -> Option<bool> {
    dict.get(key).ok()?.as_bool().ok()
}

fn dict_name(dict: &Dictionary, key: &[u8]) -> Option<String> {
    let object = dict.get(key).ok()?;
    let name = object.as_name().ok().or_else(|| {
        // Some producers wrap a single name in a one-element array.
        object
            .as_array()
            .ok()
            .and_then(|a| a.first())
            .and_then(|o| o.as_name().ok())
    })?;
    String::from_utf8(name.to_vec()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::encrypt::{PdfPermissions, encrypt};
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
    fn unencrypted_reports_no_permissions() {
        let input = pdf_with_pages(&["plain"]);
        let report = permissions_report(&input, &unlimited()).unwrap();
        assert!(!report.encrypted);
        assert!(!report.user_password_required);
        assert!(!report.owner_password_set);
        assert_eq!(report.permissions, None);
        assert_eq!(report.encryption, None);
        assert!(report.pdf_version.is_some());
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn encrypted_reports_v5_r6_and_permission_bits() {
        let input = pdf_with_pages(&["confidential"]);
        let perms = PdfPermissions {
            allow_print: true,
            allow_copy: false,
            allow_modify: false,
            allow_annotations: false,
        };
        let encrypted = encrypt(&input, "user-pw", Some("owner-pw"), perms, &unlimited()).unwrap();

        let report = permissions_report(&encrypted, &unlimited()).unwrap();
        assert!(report.encrypted);
        assert!(report.user_password_required);
        assert!(report.owner_password_set);
        assert_eq!(report.pdf_version.as_deref(), Some("1.5"));

        let p = report.permissions.expect("/P should decode");
        assert!(p.print);
        assert!(!p.copy);
        assert!(!p.modify);
        assert!(!p.annotations);

        let enc = report.encryption.expect("/Encrypt details");
        assert_eq!(enc.filter.as_deref(), Some("Standard"));
        assert_eq!(enc.v, Some(5));
        assert_eq!(enc.r, Some(6));
        assert_eq!(enc.encrypt_metadata, Some(true));

        // Caveats are always documented for encrypted input.
        assert!(report.warnings.iter().any(|w| w.contains("not verified")));
        assert!(report.warnings.iter().any(|w| w.contains("advisory")));
    }

    #[test]
    fn encrypted_all_permissions_allowed() {
        let input = pdf_with_pages(&["open"]);
        let encrypted =
            encrypt(&input, "pw", None, PdfPermissions::default(), &unlimited()).unwrap();
        let report = permissions_report(&encrypted, &unlimited()).unwrap();
        let p = report.permissions.unwrap();
        assert!(p.print && p.copy && p.modify && p.annotations);
    }

    #[test]
    fn rejects_garbage_input() {
        assert!(permissions_report(b"not a pdf", &unlimited()).is_err());
    }
}
