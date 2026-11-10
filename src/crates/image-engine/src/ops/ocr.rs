//! OCR of raster images via the external `tesseract` binary.
//!
//! [`ocr_image`] decodes the input through [`decode`] (so
//! `max_input_bytes`/`max_pixels` limits apply before any work),
//! re-encodes the pixels as PNG into a private tempdir, and invokes
//! `tesseract <png> stdout -l <lang> tsv`, parsing the TSV output into
//! an [`OcrReport`].
//!
//! **An external tool is required**: `tesseract` with the relevant
//! language data installed. When it is absent the op fails with
//! [`ImageError::ExternalTool`] — results are never faked. Tesseract
//! runs as a blocking child process; async callers should wrap this in
//! `tokio::task::spawn_blocking` (or equivalent). There is no timeout —
//! the same contract the pdf-engine's raster/OCR ops use.

use std::io::Cursor;
use std::path::PathBuf;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;
use crate::crates::image_engine::error::{ImageError, ImageResult};

use super::load::decode;

/// Binary used for OCR (tesseract).
const OCR_TOOL: &str = "tesseract";

/// Default language reported when [`OcrOptions::language`] is unset —
/// tesseract's own default is `eng`, so we do not pass `-l` in that
/// case but report the effective language honestly.
const DEFAULT_LANGUAGE: &str = "eng";

/// Maximum accepted length for a `-l` language list (e.g. `eng+por`).
const MAX_LANGUAGE_LEN: usize = 64;

/// Minimum/maximum accepted tesseract page-segmentation mode.
const MIN_PSM: u8 = 0;
const MAX_PSM: u8 = 13;

/// Options for [`ocr_image`]. All fields are optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OcrOptions {
    /// Tesseract language code(s) passed via `-l`, e.g. `"eng"` or
    /// `"eng+por"`. Validated by [`checked_language`]; `None` uses
    /// tesseract's default (`eng`).
    pub language: Option<String>,
    /// Page segmentation mode passed via `--psm N` (0–13). `None`
    /// uses tesseract's default (3).
    pub psm: Option<u8>,
}

/// One recognized word with its bounding box.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OcrWord {
    /// Recognized text (a single word; whitespace is already stripped).
    pub text: String,
    /// Bounding box as `[left, top, width, height]` in pixels — the
    /// same column order tesseract's TSV emits.
    pub bbox: [u32; 4],
    /// Tesseract confidence, 0.0–100.0 (may exceed in rare cases; it
    /// is passed through unmodified).
    pub confidence: f32,
}

/// Result of [`ocr_image`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OcrReport {
    /// Recognized text. Words on the same TSV line are joined with a
    /// single space; lines are separated by `'\n'` in tesseract's
    /// reading order.
    pub text: String,
    /// Individual recognized words with bounding boxes and
    /// confidences, in reading order.
    pub words: Vec<OcrWord>,
    /// The effective language used (`eng` when none was requested).
    pub language: String,
}

/// Recognize text in the raster image `input` via tesseract.
///
/// `input` may be any raster format this engine decodes (PNG, JPEG,
/// GIF, WebP, BMP, TIFF); it is normalized through [`decode`] so input
/// size and pixel-count limits apply. Fails with
/// [`ImageError::ExternalTool`] when `tesseract` is not on `PATH` or
/// exits non-zero, and with [`ImageError::InvalidInput`] for an
/// out-of-range `psm` or malformed `language`.
pub fn ocr_image(
    input: &[u8],
    options: &OcrOptions,
    limits: &EngineLimits,
) -> ImageResult<OcrReport> {
    let language = checked_language(options.language.as_deref())?;
    let psm = checked_psm(options.psm)?;

    let decoded = decode(input, limits)?;

    // Tesseract accepts PNG reliably regardless of the input codec —
    // re-encode into a private tempdir that is removed on return.
    let dir = tempfile::tempdir().map_err(ImageError::Io)?;
    let png_path = dir.path().join("input.png");
    let mut png = Vec::new();
    decoded
        .image
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(ImageError::Encode)?;
    std::fs::write(&png_path, &png).map_err(ImageError::Io)?;

    let tesseract = find_tool(OCR_TOOL)?;

    // `tesseract <image> stdout -l <lang> tsv` prints TSV to stdout.
    let mut cmd = Command::new(&tesseract);
    cmd.arg(&png_path).arg("stdout");
    if let Some(lang) = language.as_deref() {
        cmd.arg("-l").arg(lang);
    }
    if let Some(psm) = psm {
        cmd.arg("--psm").arg(psm.to_string());
    }
    cmd.arg("tsv");
    let output = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| ImageError::ExternalTool {
            tool: OCR_TOOL.to_string(),
            detail: format!("failed to spawn: {e}"),
        })?;
    if !output.status.success() {
        return Err(ImageError::ExternalTool {
            tool: OCR_TOOL.to_string(),
            detail: format!("exited with {}", output.status),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let (text, words) = parse_tsv(&stdout);
    Ok(OcrReport {
        text,
        words,
        language: language.unwrap_or_else(|| DEFAULT_LANGUAGE.to_string()),
    })
}

/// List the language codes installed for tesseract
/// (`tesseract --list-langs`). Mirrors the pdf-engine helper: skips
/// the header line and takes the first whitespace-separated token of
/// each remaining line.
pub fn ocr_languages() -> ImageResult<Vec<String>> {
    let tesseract = find_tool(OCR_TOOL)?;
    let output = Command::new(&tesseract)
        .arg("--list-langs")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| ImageError::ExternalTool {
            tool: OCR_TOOL.to_string(),
            detail: format!("failed to spawn: {e}"),
        })?;
    if !output.status.success() {
        return Err(ImageError::ExternalTool {
            tool: OCR_TOOL.to_string(),
            detail: format!("--list-langs exited with {}", output.status),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .skip(1) // header: "List of available languages in ..."
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect())
}

/// Whether the `tesseract` binary is on `PATH`. Used by
/// [`ProcessingEngine::health`](crate::crates::core::engine::ProcessingEngine::health)
/// to report degraded status; performs a plain directory scan with no
/// side effects.
pub(crate) fn tesseract_available() -> bool {
    find_tool(OCR_TOOL).is_ok()
}

/// Locate `tool` on `PATH`, returning the full path or
/// [`ImageError::ExternalTool`] when absent. Plain directory scan —
/// no shell — so it is cheap enough to call unconditionally.
fn find_tool(tool: &str) -> ImageResult<PathBuf> {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(tool);
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(ImageError::ExternalTool {
        tool: tool.to_string(),
        detail: "not found on PATH".to_string(),
    })
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

/// Validates a caller-supplied language list. Allowed characters are
/// ASCII alphanumerics plus `_`, `+`, `-`, and `.` (tesseract language
/// codes look like `eng`, `chi_sim`, `eng+por`). Total length is
/// capped at [`MAX_LANGUAGE_LEN`]. `None`/empty → tesseract default.
fn checked_language(language: Option<&str>) -> ImageResult<Option<String>> {
    match language {
        None => Ok(None),
        Some(lang) if lang.is_empty() => Ok(None),
        Some(lang) => {
            if lang.len() > MAX_LANGUAGE_LEN {
                return Err(ImageError::InvalidInput(format!(
                    "language must be at most {MAX_LANGUAGE_LEN} characters, got {}",
                    lang.len()
                )));
            }
            if !lang
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'+' | b'-' | b'.'))
            {
                return Err(ImageError::InvalidInput(format!(
                    "language may only contain [a-zA-Z0-9_+.-], got {lang:?}"
                )));
            }
            Ok(Some(lang.to_string()))
        }
    }
}

/// Validates an optional psm value (0–13); `None` → tesseract default.
fn checked_psm(psm: Option<u8>) -> ImageResult<Option<u8>> {
    match psm {
        None => Ok(None),
        Some(p) if (MIN_PSM..=MAX_PSM).contains(&p) => Ok(Some(p)),
        Some(p) => Err(ImageError::InvalidInput(format!(
            "psm must be between {MIN_PSM} and {MAX_PSM}, got {p}"
        ))),
    }
}

/// Parses tesseract TSV output into `(text, words)`.
///
/// TSV columns (tab-separated, header row first):
/// `level page_num block_num par_num line_num word_num left top width
/// height conf text`. Only rows with `level == 5` are words; rows
/// with empty text or unparseable fields are skipped rather than
/// failing the whole report — tesseract emits `conf == -1` on
/// structural rows we already filter out, but defensive parsing keeps
/// the op robust against version differences.
///
/// Words are grouped by `(block_num, par_num, line_num)` into lines;
/// words within a line join with `' '`, lines with `'\n'`, preserving
/// tesseract's reading order.
fn parse_tsv(tsv: &str) -> (String, Vec<OcrWord>) {
    let mut words = Vec::new();
    // line key → index into `lines`; BTreeMap not needed — insertion
    // order of first sight is reading order.
    let mut line_keys: Vec<(u32, u32, u32)> = Vec::new();
    let mut line_texts: Vec<String> = Vec::new();

    for (i, line) in tsv.lines().enumerate() {
        if i == 0 || line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 12 || fields[0] != "5" {
            continue;
        }
        let text = fields[11].trim();
        if text.is_empty() {
            continue;
        }
        let (Ok(left), Ok(top), Ok(width), Ok(height), Ok(conf)) = (
            fields[6].parse::<u32>(),
            fields[7].parse::<u32>(),
            fields[8].parse::<u32>(),
            fields[9].parse::<u32>(),
            fields[10].parse::<f32>(),
        ) else {
            continue;
        };
        let key = (
            fields[2].parse::<u32>().unwrap_or(0),
            fields[3].parse::<u32>().unwrap_or(0),
            fields[4].parse::<u32>().unwrap_or(0),
        );
        let pos = match line_keys.iter().position(|k| *k == key) {
            Some(p) => p,
            None => {
                line_keys.push(key);
                line_texts.push(String::new());
                line_texts.len() - 1
            }
        };
        if !line_texts[pos].is_empty() {
            line_texts[pos].push(' ');
        }
        line_texts[pos].push_str(text);
        words.push(OcrWord {
            text: text.to_string(),
            bbox: [left, top, width, height],
            confidence: conf,
        });
    }
    (line_texts.join("\n"), words)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::image_engine::error::ImageError;

    /// Minimal valid TSV with a header, two words on one line and one
    /// word on a second line, plus noise rows that must be skipped.
    const TSV_FIXTURE: &str = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
1\t1\t0\t0\t0\t0\t0\t0\t100\t20\t-1\t\n\
5\t1\t1\t1\t1\t1\t10\t20\t30\t10\t96.5\tHello\n\
5\t1\t1\t1\t1\t2\t45\t20\t35\t10\t88.0\tworld\n\
5\t1\t1\t1\t2\t1\t10\t40\t50\t10\t91.2\tagain\n\
4\t1\t1\t1\t1\t0\t10\t20\t70\t10\t-1\t\n";

    #[test]
    fn parse_tsv_extracts_words() {
        let (text, words) = parse_tsv(TSV_FIXTURE);
        assert_eq!(words.len(), 3);
        assert_eq!(words[0].text, "Hello");
        assert_eq!(words[0].bbox, [10, 20, 30, 10]);
        assert!((words[0].confidence - 96.5).abs() < f32::EPSILON);
        assert_eq!(words[1].text, "world");
        assert_eq!(words[2].text, "again");
        // Line grouping: two words on line 1, one on line 2.
        assert_eq!(text, "Hello world\nagain");
    }

    #[test]
    fn parse_tsv_handles_empty_and_malformed() {
        let (text, words) = parse_tsv("");
        assert!(text.is_empty());
        assert!(words.is_empty());
        // Garbage lines are skipped, not fatal.
        let (_, words) = parse_tsv("not\ta\ttsv\n5\t1\t1\t1\t1\t1\tx\ty\tz\tw\tbad\tword\n");
        assert!(words.is_empty());
    }

    #[test]
    fn checked_language_accepts_valid_codes() {
        assert_eq!(checked_language(None).unwrap(), None);
        assert_eq!(checked_language(Some("")).unwrap(), None);
        assert_eq!(
            checked_language(Some("eng")).unwrap(),
            Some("eng".to_string())
        );
        assert_eq!(
            checked_language(Some("chi_sim+eng")).unwrap(),
            Some("chi_sim+eng".to_string())
        );
    }

    #[test]
    fn checked_language_rejects_bad_input() {
        for bad in ["eng; rm -rf /", "eng --psm 6", "../etc", "a b", "éng"] {
            assert!(
                matches!(
                    checked_language(Some(bad)),
                    Err(ImageError::InvalidInput(_))
                ),
                "{bad:?} must be rejected"
            );
        }
        let long = "a".repeat(MAX_LANGUAGE_LEN + 1);
        assert!(matches!(
            checked_language(Some(&long)),
            Err(ImageError::InvalidInput(_))
        ));
    }

    #[test]
    fn checked_psm_bounds() {
        assert_eq!(checked_psm(None).unwrap(), None);
        assert_eq!(checked_psm(Some(6)).unwrap(), Some(6));
        assert!(matches!(
            checked_psm(Some(14)),
            Err(ImageError::InvalidInput(_))
        ));
    }

    #[test]
    fn missing_tesseract_is_honest_error() {
        // tesseract is NOT installed on this machine — this must hit
        // the missing-tool path. If it ever is installed the test
        // still passes only when find_tool fails; guard explicitly.
        if find_tool(OCR_TOOL).is_ok() {
            // Tool present: decode a tiny PNG and run end-to-end is
            // covered by integration elsewhere; here just ensure the
            // probe agrees.
            return;
        }
        let img = tiny_png();
        let err = ocr_image(&img, &OcrOptions::default(), &EngineLimits::default())
            .expect_err("tesseract is absent; must fail");
        assert!(
            matches!(err, ImageError::ExternalTool { .. }),
            "expected ExternalTool, got {err:?}"
        );
        assert!(ocr_languages().is_err());
    }

    #[test]
    fn garbage_image_is_decode_error() {
        let err = ocr_image(
            b"this is not an image",
            &OcrOptions::default(),
            &EngineLimits::default(),
        )
        .expect_err("garbage must fail");
        assert!(
            matches!(
                err,
                ImageError::Decode(_) | ImageError::UnsupportedFormat(_)
            ),
            "expected decode/unsupported error, got {err:?}"
        );
    }

    /// A 2x2 white PNG for exercising the pipeline up to the tool call.
    fn tiny_png() -> Vec<u8> {
        use image::{DynamicImage, Rgb, RgbImage};
        let mut img = RgbImage::new(2, 2);
        for p in img.pixels_mut() {
            *p = Rgb([255, 255, 255]);
        }
        let mut buf = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("test PNG must encode");
        buf
    }
}
