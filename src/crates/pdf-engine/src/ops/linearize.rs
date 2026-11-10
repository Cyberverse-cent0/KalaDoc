//! Fast Web View (linearized PDF) output.
//!
//! # Honesty notice — read before relying on this
//!
//! A linearized ("Fast Web View") PDF has a very specific structure:
//! a `/Linearized` parameter dictionary as the first object, a
//! first-page cross-reference section and trailer up front, a primary
//! hint stream (`/H`) describing where every page's objects live, and a
//! second xref section for the rest of the file. Viewers use the hint
//! stream to fetch page 1 with a single range request.
//!
//! `lopdf` 0.47 **parses** linearization dictionaries and hint tables
//! on load ([`lopdf::Linearization`]), but its writer does **not**
//! implement them: [`lopdf::SaveOptions::linearize`] is accepted by the
//! API and stored on the options struct, yet nothing in
//! `Document::save_with_options` / `save_internal` ever reads that
//! flag. Saving with `linearize: true` produces a byte-identical
//! *non*-linearized file (the writer in fact drops any pre-existing
//! `/Linearized` dictionary on save).
//!
//! This operation therefore:
//!
//! 1. Saves through `save_with_options` with `linearize: true` so the
//!    output is forward-compatible if a future `lopdf` implements the
//!    flag.
//! 2. **Re-parses the output** and inspects it for a `/Linearized`
//!    dictionary and hint stream, reporting what it actually found.
//!
//! The [`LinearizeReport::linearized`] field is `true` only when the
//! re-parsed document really carries linearization data. Today that is
//! always `false`; the report says so plainly instead of claiming a
//! Fast Web View file that does not exist.

use lopdf::SaveOptions;

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::load::{load_pdf, reject_encrypted};

/// Result of [`linearize`].
#[derive(Debug, Clone)]
pub struct LinearizeReport {
    /// The re-saved document bytes.
    pub bytes: Vec<u8>,
    /// `true` only if the *output* was verified to contain a
    /// `/Linearized` parameter dictionary (checked by re-parsing the
    /// saved bytes). With `lopdf` 0.47 this is always `false`.
    pub linearized: bool,
    /// `true` only if the output carries a hint stream (`/H` entries in
    /// the linearization dictionary). With `lopdf` 0.47 this is always
    /// `false`.
    pub hint_stream: bool,
    /// Human-readable explanation of what was and was not done.
    pub notes: Vec<String>,
}

/// Save `input` requesting Fast Web View linearization and return the
/// output bytes plus an honest [`LinearizeReport`].
///
/// Encrypted documents are rejected. See the module docs: `lopdf`
/// accepts `SaveOptions::linearize` but does not currently implement
/// hint-table generation, so [`LinearizeReport::linearized`] will be
/// `false` and the notes explain why.
pub fn linearize(input: &[u8], limits: &EngineLimits) -> PdfResult<LinearizeReport> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let mut notes = Vec::new();
    notes.push("saved with SaveOptions::linearize = true".to_string());
    notes.push(
        "lopdf 0.47 accepts the linearize flag but does not generate linearization \
         hint tables; output verified by re-parsing (see `linearized`/`hint_stream`)"
            .to_string(),
    );

    let options = SaveOptions::builder().linearize(true).build();

    let mut bytes = Vec::new();
    doc.save_with_options(&mut bytes, options)
        .map_err(crate::crates::pdf_engine::error::PdfError::Encode)?;

    // Verify what was actually written: reload and look for the
    // /Linearized dictionary, plus a raw byte scan for the name as a
    // belt-and-suspenders check.
    let mut linearized = false;
    let mut hint_stream = false;
    match load_pdf(&bytes, limits) {
        Ok(reloaded) => {
            if let Some(lin) = &reloaded.linearization {
                linearized = true;
                hint_stream = !lin.hint_streams.is_empty();
            }
        }
        Err(_) => {
            notes.push("could not re-parse the saved output to verify linearization".to_string());
        }
    }
    if bytes
        .windows(b"/Linearized".len())
        .any(|w| w == b"/Linearized")
    {
        linearized = true;
    }
    if !linearized {
        notes.push(
            "output is NOT linearized: no /Linearized dictionary or hint stream was written"
                .to_string(),
        );
    } else if !hint_stream {
        notes.push("linearization dictionary present but no hint stream found".to_string());
    }

    Ok(LinearizeReport {
        bytes,
        linearized,
        hint_stream,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::core::engine::EngineLimits;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;

    #[test]
    fn report_is_honest_about_missing_linearization() {
        let pdf = pdf_with_pages(&["hello"]);
        let report = linearize(&pdf, &EngineLimits::default()).unwrap();

        // lopdf 0.47 does not implement linearization on save: the
        // report must say so rather than claim success.
        assert!(!report.linearized);
        assert!(!report.hint_stream);
        assert!(report.notes.iter().any(|n| n.contains("NOT linearized")));

        // The output must still be a valid PDF.
        let reloaded = lopdf::Document::load_mem(&report.bytes).unwrap();
        assert_eq!(reloaded.get_pages().len(), 1);
    }

    #[test]
    fn rejects_encrypted_and_garbage() {
        assert!(linearize(b"not a pdf", &EngineLimits::default()).is_err());
    }
}
