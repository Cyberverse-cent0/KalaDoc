//! Decrypt a password-protected PDF, producing an unencrypted copy.
//!
//! `lopdf` 0.47 implements the PDF "Standard" security handler (revisions
//! 2–6, RC4/AES-128/AES-256) for real: [`lopdf::Document::decrypt`] and
//! the password-aware loaders in [`lopdf::LoadOptions`]. This op uses
//! [`lopdf::Document::load_mem_with_options`] with
//! [`lopdf::LoadOptions::with_password`], which authenticates the
//! password, derives the file encryption key and decrypts every string
//! and stream while loading. The result is saved back out with no
//! `/Encrypt` entry — a genuine unencrypted PDF, not a re-labelled
//! encrypted one.
//!
//! Non-Standard security handlers (e.g. public-key `/Filter` other than
//! `Standard`) are rejected by `lopdf` and surface as
//! [`PdfError::InvalidPdf`]. An incorrect password surfaces as
//! [`PdfError::InvalidInput`]. An empty password is a legitimate PDF
//! user password ("opens without a password") and is tried as given.

use std::panic::{AssertUnwindSafe, catch_unwind};

use lopdf::{Document, LoadOptions};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{check_page_limit, doc_is_encrypted, load_pdf, save_pdf};

/// Decrypt `input` with `password` and return the unencrypted PDF bytes.
///
/// Both the user password and the owner password are accepted (lopdf
/// tries owner-then-user authentication internally). The input is
/// limit-checked exactly like every other op; the decrypted document is
/// additionally page-limit checked after decryption, since an
/// encrypted file cannot report a trustworthy page count before its
/// objects are decoded.
///
/// Errors:
/// - [`PdfError::InvalidInput`] — input is not encrypted, or the
///   password is wrong.
/// - [`PdfError::InvalidPdf`] — the document cannot be parsed, or uses
///   a security handler `lopdf` does not implement.
pub fn decrypt(input: &[u8], password: &str, limits: &EngineLimits) -> PdfResult<Vec<u8>> {
    // Load once without a password for limit enforcement and to learn
    // whether the document is encrypted at all. `load_pdf` tolerates
    // encrypted input; page count is unreliable here (objects are still
    // ciphertext), which is why the real load below re-checks it.
    let probe = load_pdf(input, limits)?;
    if !doc_is_encrypted(&probe) {
        return Err(PdfError::InvalidInput(
            "document is not encrypted; nothing to decrypt".to_string(),
        ));
    }
    drop(probe);

    // Second load with the password: lopdf authenticates, derives the
    // key and decrypts every object during the read. Wrapped in
    // `catch_unwind` like `load_pdf` — hostile encrypted files can still
    // crash the parser.
    let mut doc = catch_unwind(AssertUnwindSafe(|| {
        Document::load_mem_with_options(input, LoadOptions::with_password(password))
    }))
    .map_err(|_| PdfError::InvalidInput("malformed PDF crashed the parser".to_string()))?
    .map_err(|err| match err {
        lopdf::Error::InvalidPassword => {
            PdfError::InvalidInput("incorrect password for encrypted PDF".to_string())
        }
        lopdf::Error::IO(io) => PdfError::Encode(io),
        other => PdfError::InvalidPdf(other),
    })?;

    check_page_limit(doc.get_pages().len(), limits)?;
    save_pdf(&mut doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::pdf_engine::ops::encrypt::{self, PdfPermissions};
    use crate::crates::pdf_engine::ops::test_utils::{page_count, pdf_with_pages};

    #[test]
    fn decrypt_roundtrip() {
        let plain = pdf_with_pages(&["secret page one", "secret page two"]);
        let encrypted = encrypt::encrypt(
            &plain,
            "user-pw",
            Some("owner-pw"),
            PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .expect("encrypt should succeed");

        // The encrypted bytes really are encrypted.
        let doc = Document::load_mem(&encrypted).unwrap();
        assert!(doc_is_encrypted(&doc));

        // Owner password works too.
        let decrypted = decrypt(&encrypted, "owner-pw", &EngineLimits::default()).unwrap();
        assert!(!doc_is_encrypted(&Document::load_mem(&decrypted).unwrap()));
        assert_eq!(page_count(&decrypted), 2);

        let decrypted = decrypt(&encrypted, "user-pw", &EngineLimits::default()).unwrap();
        assert_eq!(page_count(&decrypted), 2);
    }

    #[test]
    fn decrypt_rejects_wrong_password_and_plaintext() {
        let plain = pdf_with_pages(&["not encrypted"]);
        assert!(matches!(
            decrypt(&plain, "anything", &EngineLimits::default()),
            Err(PdfError::InvalidInput(_))
        ));

        let encrypted = encrypt::encrypt(
            &plain,
            "pw",
            None,
            PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        assert!(matches!(
            decrypt(&encrypted, "wrong", &EngineLimits::default()),
            Err(PdfError::InvalidInput(_))
        ));
    }

    #[test]
    fn decrypt_enforces_limits() {
        let plain = pdf_with_pages(&["x"]);
        let encrypted = encrypt::encrypt(
            &plain,
            "pw",
            None,
            PdfPermissions::default(),
            &EngineLimits::default(),
        )
        .unwrap();
        let limits = EngineLimits {
            max_input_bytes: Some(10),
            ..EngineLimits::default()
        };
        assert!(matches!(
            decrypt(&encrypted, "pw", &limits),
            Err(PdfError::LimitExceeded(_))
        ));
    }
}
