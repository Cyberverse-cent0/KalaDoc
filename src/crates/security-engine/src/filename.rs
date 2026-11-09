//! Filename sanitization and validation.
//!
//! [`sanitize_filename`] decides whether a user-supplied name is safe to
//! use as a *single path component* inside a storage root. The policy is
//! deliberately strict and **rejects rather than silently rewrites**
//! dangerous input — a rejected upload is debuggable and auditable, while
//! a silently mangled name can hide attacker intent or collide with an
//! innocent file.
//!
//! Rejected (returns [`SecurityError::InvalidFilename`]):
//! - empty names, and names over 255 bytes (common filesystem limit);
//! - any path syntax: `/`, `\`, or `:` — so `../..`, absolute paths,
//!   drive letters (`C:`), and NTFS alternate data streams (`file:ads`)
//!   all fail uniformly. Callers must pass a bare filename, never a path;
//! - ASCII control characters and DEL/C1 controls (includes NUL);
//! - bidirectional/format control characters (U+061C, U+200E, U+200F,
//!   U+202A..=U+202E, U+2066..=U+2069) that enable filename spoofing via
//!   RTL override; other unicode passes through unchanged — we do not
//!   attempt NFKC normalization, only exclusion of unsafe codepoints;
//! - characters unsafe on Windows: `< > : " | ? *`;
//! - leading or trailing `.` or space (Windows strips these, enabling
//!   `file .txt` → `file.txt` collisions and hidden `..` tricks);
//! - Windows reserved device names: `CON`, `PRN`, `AUX`, `NUL`,
//!   `COM1`–`COM9`, `LPT1`–`LPT9` — matched case-insensitively on the
//!   stem before the first `.`, so `con.txt` is also rejected.
//!
//! Everything else — including spaces *inside* the name, unicode letters,
//! and dots in the middle — is accepted unchanged. The returned `String`
//! is always a bare filename: it can never contain a separator and can be
//! joined onto a storage root without escaping it.

use super::error::{SecurityError, SecurityResult};

/// Maximum filename length in bytes accepted by common filesystems.
const MAX_FILENAME_BYTES: usize = 255;

/// ASCII characters that are illegal in Windows filenames.
const WINDOWS_UNSAFE: &[char] = &['<', '>', ':', '"', '|', '?', '*'];

/// Unicode bidirectional/format controls that enable visual spoofing.
fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

/// Whether `stem` (the part before the first `.`) names a Windows device.
fn is_reserved_device_name(stem: &str) -> bool {
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r))
}

/// Validate `name` as a bare filename and return it unchanged when safe.
///
/// The input must be a single filename — not a path. On success the
/// returned string is guaranteed separator-free, within the byte limit,
/// free of control/bidi characters and Windows-unsafe characters, not a
/// reserved device name, and without leading/trailing dots or spaces.
///
/// # Errors
/// Returns [`SecurityError::InvalidFilename`] with a static `reason`
/// describing the violated rule; the untrusted input is never echoed.
pub fn sanitize_filename(name: &str) -> SecurityResult<String> {
    if name.is_empty() {
        return Err(SecurityError::InvalidFilename {
            reason: "empty filename",
        });
    }
    if name.len() > MAX_FILENAME_BYTES {
        return Err(SecurityError::InvalidFilename {
            reason: "filename exceeds 255 bytes",
        });
    }
    for c in name.chars() {
        if c == '/' || c == '\\' || c == ':' {
            return Err(SecurityError::InvalidFilename {
                reason: "path separators and drive/stream syntax are not allowed",
            });
        }
        if c.is_control() {
            return Err(SecurityError::InvalidFilename {
                reason: "control characters are not allowed",
            });
        }
        if is_bidi_control(c) {
            return Err(SecurityError::InvalidFilename {
                reason: "bidirectional control characters are not allowed",
            });
        }
        if WINDOWS_UNSAFE.contains(&c) {
            return Err(SecurityError::InvalidFilename {
                reason: "character is unsafe on Windows",
            });
        }
    }
    if name.starts_with('.') || name.starts_with(' ') || name.ends_with('.') || name.ends_with(' ')
    {
        return Err(SecurityError::InvalidFilename {
            reason: "leading or trailing dots/spaces are not allowed",
        });
    }
    // `split` always yields at least one item; the first is the stem.
    let stem = name.split('.').next().unwrap_or(name);
    if is_reserved_device_name(stem) {
        return Err(SecurityError::InvalidFilename {
            reason: "Windows reserved device name",
        });
    }
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_names() {
        for name in [
            "report.pdf",
            "my document (final).PDF",
            "résumé-2024.docx",
            "a.b.c.txt",
            "file with spaces.txt",
            "ファイル.pdf",
        ] {
            assert_eq!(sanitize_filename(name).unwrap(), name);
        }
        assert_eq!(
            sanitize_filename("my document (final).PDF").unwrap(),
            "my document (final).PDF"
        );
    }

    #[test]
    fn accepts_interior_spaces_and_dots() {
        assert_eq!(
            sanitize_filename("quarterly report.v2.final.pdf").unwrap(),
            "quarterly report.v2.final.pdf"
        );
        assert_eq!(sanitize_filename("a b c.txt").unwrap(), "a b c.txt");
    }

    #[test]
    fn rejects_path_traversal() {
        for name in [
            "../../etc/passwd",
            "..\\win\\system32",
            "..",
            "../secret.txt",
            "sub/dir/file.txt",
            "sub\\dir\\file.txt",
            "/etc/passwd",
            "C:\\Windows\\System32",
            "C:stream",
            "file.txt:ads",
        ] {
            assert!(
                sanitize_filename(name).is_err(),
                "expected rejection: {name:?}"
            );
        }
    }

    #[test]
    fn rejects_control_characters() {
        for name in [
            "file\0.txt",
            "file\n.txt",
            "file\u{7}.txt",
            "file\u{85}.txt",
        ] {
            assert!(
                sanitize_filename(name).is_err(),
                "expected rejection: {name:?}"
            );
        }
    }

    #[test]
    fn rejects_bidi_overrides() {
        // "exe.pdf" visually reversed via RLO.
        for name in [
            "exe\u{202E}fdp.pdf",
            "\u{202E}evil.exe",
            "a\u{2066}b\u{2069}.txt",
            "\u{061C}file.txt",
            "\u{200F}file.txt",
        ] {
            assert!(
                sanitize_filename(name).is_err(),
                "expected rejection: {name:?}"
            );
        }
        // Benign unicode (combining marks, CJK) still passes.
        assert!(sanitize_filename("café.txt").is_ok());
        assert!(sanitize_filename("naïve résumé.pdf").is_ok());
    }

    #[test]
    fn rejects_windows_unsafe_characters() {
        for name in [
            "a<b.txt", "a>b.txt", "a|b.txt", "a?b.txt", "a*b.txt", "a\"b.txt",
        ] {
            assert!(
                sanitize_filename(name).is_err(),
                "expected rejection: {name:?}"
            );
        }
    }

    #[test]
    fn rejects_leading_trailing_dots_and_spaces() {
        for name in [
            ".hidden",
            "..",
            ".",
            "file.",
            "file ",
            " file.txt",
            "...txt",
        ] {
            assert!(
                sanitize_filename(name).is_err(),
                "expected rejection: {name:?}"
            );
        }
        assert!(sanitize_filename("a.b.txt").is_ok());
    }

    #[test]
    fn rejects_windows_reserved_names() {
        for name in [
            "CON", "con", "Con.txt", "NUL", "nul.pdf", "PRN", "AUX", "COM1", "com9.log", "LPT1",
            "lpt9",
        ] {
            assert!(
                sanitize_filename(name).is_err(),
                "expected rejection: {name:?}"
            );
        }
        // Similar but non-reserved names pass.
        assert!(sanitize_filename("console.txt").is_ok());
        assert!(sanitize_filename("com10.txt").is_ok());
        assert!(sanitize_filename("lpt.txt").is_ok());
    }

    #[test]
    fn rejects_oversized_and_empty_names() {
        let long = "a".repeat(256);
        assert!(sanitize_filename(&long).is_err());
        assert!(sanitize_filename(&"a".repeat(255)).is_ok());
        assert!(sanitize_filename("").is_err());
        assert!(sanitize_filename("   ").is_err()); // leading/trailing space
    }

    #[test]
    fn error_never_echoes_raw_input() {
        let err = sanitize_filename("..\\evil\u{202E}.exe").unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains("evil"));
        assert!(msg.contains("invalid filename"));
    }
}
