//! Password-based encryption (PDF 2.0, V5/R6 — AES-256).
//!
//! This op wraps `lopdf`'s [`lopdf::EncryptionVersion::V5`]: a 256-bit
//! file encryption key, AES-256-CBC (`/AESV3` `StdCF` crypt filter) for
//! both streams and strings, and the R6 password algorithms (Algorithms
//! 8–11, including SASLprep password normalization and the AES-256
//! password hash). This is the strongest scheme `lopdf` 0.47 can
//! *produce* — `V4` tops out at AES-128/R4 and `R5` is a deprecated
//! proprietary Adobe extension — and it is also the cleanest to
//! construct: unlike the MD5-based revisions it does not require a
//! document `/ID` entry.
//!
//! The random 32-byte file encryption key is drawn from the OS RNG
//! (`rand_core::OsRng`), never derived from the passwords or the
//! document. Nothing here fabricates encryption metadata — the output
//! opens in any conforming reader only with the user (or owner)
//! password.
//!
//! The owner password defaults to the user password when not supplied,
//! matching common PDF-tool behavior. Both must be non-empty.

use std::collections::BTreeMap;
use std::sync::Arc;

use lopdf::encryption::crypt_filters::{Aes256CryptFilter, CryptFilter};
use lopdf::{EncryptionState, EncryptionVersion, Permissions};
use rand_core::{OsRng, RngCore as _};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{doc_is_encrypted, load_pdf, lopdf_err, save_pdf};

/// Document access permissions recorded in the encryption dictionary.
///
/// Each flag maps to the corresponding PDF permission bits; reserved
/// bits are always set to 1 by `lopdf` itself. Defaults to all-allowed.
/// Denied permissions are a hint for well-behaved readers, not a hard
/// guarantee — anyone with the file can strip the `/Encrypt` dictionary
/// with a suitable tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PdfPermissions {
    /// Allow printing (low- and high-quality).
    pub allow_print: bool,
    /// Allow copying/extracting text and graphics.
    pub allow_copy: bool,
    /// Allow modifying the document's contents.
    pub allow_modify: bool,
    /// Allow adding annotations and filling in form fields.
    pub allow_annotations: bool,
}

impl Default for PdfPermissions {
    fn default() -> Self {
        Self {
            allow_print: true,
            allow_copy: true,
            allow_modify: true,
            allow_annotations: true,
        }
    }
}

impl PdfPermissions {
    fn to_lopdf(self) -> Permissions {
        let mut p = Permissions::empty();
        // COPYABLE_FOR_ACCESSIBILITY must always be set per the PDF spec
        // so assistive-technology readers are not blocked by the copy bit.
        p |= Permissions::COPYABLE_FOR_ACCESSIBILITY;
        if self.allow_print {
            p |= Permissions::PRINTABLE | Permissions::PRINTABLE_IN_HIGH_QUALITY;
        }
        if self.allow_copy {
            p |= Permissions::COPYABLE;
        }
        if self.allow_modify {
            p |= Permissions::MODIFIABLE;
        }
        if self.allow_annotations {
            p |= Permissions::ANNOTABLE | Permissions::FILLABLE;
        }
        p
    }
}

/// Encrypt `input` so that it opens with `user_password` (or
/// `owner_password`, which authenticates as owner regardless of the
/// permission bits).
///
/// - `owner_password`: `None` reuses `user_password`.
/// - `permissions`: [`PdfPermissions`] controlling the `/P` permission
///   bits; defaults to all-allowed.
///
/// Errors:
/// - [`PdfError::InvalidInput`] — a password is empty, or the input is
///   already encrypted.
/// - [`PdfError::InvalidPdf`] — the input cannot be parsed.
/// - [`PdfError::Processing`] — lopdf failed while building the security
///   dictionary or re-encrypting the objects.
pub fn encrypt(
    input: &[u8],
    user_password: &str,
    owner_password: Option<&str>,
    permissions: PdfPermissions,
    limits: &EngineLimits,
) -> PdfResult<Vec<u8>> {
    if user_password.is_empty() {
        return Err(PdfError::InvalidInput(
            "user password must not be empty".to_string(),
        ));
    }
    let owner_password = owner_password.unwrap_or(user_password);
    if owner_password.is_empty() {
        return Err(PdfError::InvalidInput(
            "owner password must not be empty".to_string(),
        ));
    }

    let mut doc = load_pdf(input, limits)?;
    if doc_is_encrypted(&doc) {
        return Err(PdfError::InvalidInput(
            "document is already encrypted".to_string(),
        ));
    }

    // 32 bytes of OS randomness for the file encryption key (Algorithm 8
    // input). AES-256 revision 6 requires exactly 32 bytes.
    let mut file_encryption_key = [0u8; 32];
    OsRng.fill_bytes(&mut file_encryption_key);

    let crypt_filter: Arc<dyn CryptFilter> = Arc::new(Aes256CryptFilter);
    let state = EncryptionState::try_from(EncryptionVersion::V5 {
        encrypt_metadata: true,
        crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), crypt_filter)]),
        file_encryption_key: &file_encryption_key,
        stream_filter: b"StdCF".to_vec(),
        string_filter: b"StdCF".to_vec(),
        owner_password,
        user_password,
        permissions: permissions.to_lopdf(),
    })
    .map_err(|err| match err {
        // Password sanitization failures (e.g. over-long passwords) are
        // caller-fixable input problems, not processing failures.
        lopdf::Error::Decryption(_) => PdfError::InvalidInput(format!(
            "password rejected by the PDF standard security handler: {err}"
        )),
        other => lopdf_err(other),
    })?;

    doc.encrypt(&state).map_err(lopdf_err)?;
    save_pdf(&mut doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::decrypt::decrypt;
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};

    fn unlimited() -> EngineLimits {
        EngineLimits {
            max_input_bytes: None,
            max_pages: None,
            max_pixels: None,
            max_outputs: None,
        }
    }

    #[test]
    fn encrypt_then_decrypt_round_trip() {
        let input = pdf_with_pages(&["Secret page one", "Secret page two"]);
        let encrypted = encrypt(
            &input,
            "user-pw",
            Some("owner-pw"),
            PdfPermissions::default(),
            &unlimited(),
        )
        .unwrap();

        // The encrypted buffer must itself be detected as encrypted.
        let doc = load_pdf(&encrypted, &unlimited()).unwrap();
        assert!(doc_is_encrypted(&doc));

        // User password opens it.
        let decrypted = decrypt(&encrypted, "user-pw", &unlimited()).unwrap();
        assert_eq!(page_count(&decrypted), 2);
        assert!(!doc_is_encrypted(
            &load_pdf(&decrypted, &unlimited()).unwrap()
        ));

        // Owner password also opens it.
        let decrypted = decrypt(&encrypted, "owner-pw", &unlimited()).unwrap();
        assert_eq!(page_count(&decrypted), 2);
    }

    #[test]
    fn encrypt_produces_real_v5_r6_encryption() {
        let input = pdf_with_pages(&["confidential"]);
        let encrypted = encrypt(
            &input,
            "open-sesame",
            None,
            PdfPermissions::default(),
            &unlimited(),
        )
        .unwrap();
        let doc = lopdf::Document::load_mem(&encrypted).unwrap();
        let enc = doc.get_encrypted().unwrap();
        assert_eq!(enc.get(b"V").unwrap().as_i64().unwrap(), 5);
        assert_eq!(enc.get(b"R").unwrap().as_i64().unwrap(), 6);
        assert_eq!(enc.get(b"Filter").unwrap().as_name().unwrap(), b"Standard");
    }

    #[test]
    fn rejects_empty_passwords() {
        let input = pdf_with_pages(&["Hi"]);
        assert!(encrypt(&input, "", None, PdfPermissions::default(), &unlimited()).is_err());
        assert!(
            encrypt(
                &input,
                "pw",
                Some(""),
                PdfPermissions::default(),
                &unlimited()
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_already_encrypted_input() {
        let input = pdf_with_pages(&["Hi"]);
        let encrypted =
            encrypt(&input, "pw", None, PdfPermissions::default(), &unlimited()).unwrap();
        assert!(
            encrypt(
                &encrypted,
                "pw2",
                None,
                PdfPermissions::default(),
                &unlimited()
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_garbage_input() {
        assert!(
            encrypt(
                b"not a pdf",
                "pw",
                None,
                PdfPermissions::default(),
                &unlimited()
            )
            .is_err()
        );
    }

    #[test]
    fn restricted_permissions_still_round_trip() {
        let input = pdf_with_pages(&["locked down"]);
        let perms = PdfPermissions {
            allow_print: true,
            allow_copy: false,
            allow_modify: false,
            allow_annotations: false,
        };
        let encrypted = encrypt(&input, "pw", None, perms, &unlimited()).unwrap();
        let decrypted = decrypt(&encrypted, "pw", &unlimited()).unwrap();
        assert_eq!(page_count(&decrypted), 1);
    }
}
