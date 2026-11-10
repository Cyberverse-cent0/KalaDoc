//! Convert page content-stream color operators to grayscale.
//!
//! For every selected page, each `/Contents` stream is decoded into
//! [`lopdf::content::Content`] operations and the PDF color-setting
//! operators are rewritten:
//!
//! - `rg r g b` (non-stroking RGB) → `g gray`, and `RG r g b`
//!   (stroking RGB) → `G gray`, where
//!   `gray = 0.299·r + 0.587·g + 0.114·b` (Rec. 601 luma).
//! - `k c m y k` → `g gray`, and `K c m y k` → `G gray`, where the
//!   CMYK components are first approximated to RGB with the naive
//!   formula `R = 1 − min(1, C+K)`, `G = 1 − min(1, M+K)`,
//!   `B = 1 − min(1, Y+K)`, then the same luma is applied. This is an
//!   approximation — real CMYK→RGB conversion is device-dependent.
//! - `g` / `G` are already grayscale and pass through unchanged.
//!
//! # Honest scope
//!
//! - `sc` / `SC` / `scn` / `SCN` (special/color-space operands, which
//!   may name a colorspace via `cs`/`CS`) are **kept unchanged** and
//!   counted in [`ColorReport::ops_skipped`]: without resolving each
//!   page's `/ColorSpace` resources we cannot know how to map them to
//!   gray. `cs`, `CS` and `ri` pass through untouched.
//! - Only the page's own `/Contents` streams are rewritten. Colors
//!   inside Form XObjects, Type3 glyph descriptions, tiling patterns
//!   and inline images are **not** visited — those streams keep their
//!   original colors.
//! - Rewritten streams are stored **uncompressed** (`/Filter` and
//!   `/DecodeParms` are dropped), like `redact` does; output may be
//!   larger than the input.
//! - `lopdf`'s content decoder is lenient: it stops at the first
//!   unparseable operator and silently drops the remainder of the
//!   stream, so re-encoded output may be shorter than the input for
//!   already-damaged streams.
//! - A page whose content stream cannot be decoded is skipped whole
//!   (counted in [`ColorReport::pages_skipped`]); if *every* selected
//!   page fails to decode, the operation returns an error rather than
//!   silently producing an unchanged document.

use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, ObjectId};
use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::{PdfError, PdfResult};

use super::load::{load_pdf, reject_encrypted, save_pdf};
use super::pages::parse_page_range;

/// Decompression bound for a single content stream — lopdf's own
/// default, matching the other stream-rewriting ops.
use lopdf::DEFAULT_MAX_DECOMPRESSED_SIZE as MAX_CONTENT_STREAM;

/// Rec. 601 luma coefficients for RGB → gray.
const LUMA_R: f64 = 0.299;
const LUMA_G: f64 = 0.587;
const LUMA_B: f64 = 0.114;

/// Outcome of one page-range color pass, returned alongside the bytes.
///
/// Counts let callers report honestly what the pass could and could
/// not rewrite.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColorReport {
    /// Selected pages whose content streams were successfully
    /// decoded and (re)written.
    pub pages_processed: usize,
    /// Color-setting operations whose operands were rewritten
    /// (`rg`/`RG`/`k`/`K` → gray).
    pub ops_rewritten: usize,
    /// Color-space-dependent operations (`sc`/`SC`/`scn`/`SCN`) and
    /// malformed color operators left unchanged.
    pub ops_skipped: usize,
    /// Selected pages skipped because a content stream failed to
    /// decode (or was missing/corrupt). Non-zero means the output may
    /// still contain color on those pages.
    pub pages_skipped: usize,
}

/// Which color rewrite to apply. Shared with `invert_colors`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColorMode {
    /// RGB/CMYK → gray (`g`/`G` operators).
    Grayscale,
    /// Complement each operand within its own color space.
    Invert,
}

/// Result of transforming a single decoded operation.
enum OpTransform {
    /// Keep the operation exactly as decoded.
    Unchanged,
    /// Replace the operation with these operations.
    Rewritten(Vec<Operation>),
    /// A color op we deliberately leave alone (`sc` family) or that
    /// was malformed — count it as skipped.
    Skipped,
}

/// Extract a numeric operand as `f64` (`Real` or `Integer`).
fn operand_number(obj: &Object) -> Option<f64> {
    match obj {
        Object::Real(v) => Some(f64::from(*v)),
        Object::Integer(v) => Some(*v as f64),
        _ => None,
    }
}

/// Extract `n` numeric operands, or `None` if arity/types are wrong.
fn numeric_operands(operands: &[Object], n: usize) -> Option<Vec<f64>> {
    if operands.len() != n {
        return None;
    }
    operands.iter().map(operand_number).collect()
}

/// Clamp a color component to the PDF `[0, 1]` range.
fn clamp01(v: f64) -> f64 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Rec. 601 luma of RGB components (each expected in `[0, 1]`).
fn luma(r: f64, g: f64, b: f64) -> f64 {
    LUMA_R * r + LUMA_G * g + LUMA_B * b
}

/// Naive CMYK → RGB approximation, then luma.
///
/// `R = 1 − min(1, C+K)` etc. — documented in the module docs as an
/// approximation; real CMYK conversion is device-dependent.
fn cmyk_luma(c: f64, m: f64, y: f64, k: f64) -> f64 {
    let r = 1.0 - (c + k).min(1.0);
    let g = 1.0 - (m + k).min(1.0);
    let b = 1.0 - (y + k).min(1.0);
    luma(r, g, b)
}

/// Transform one operation according to `mode`.
fn transform_operation(op: &Operation, mode: ColorMode) -> OpTransform {
    let operator = op.operator.as_str();
    match operator {
        // Already grayscale: unchanged in Grayscale mode; in Invert
        // mode the single operand is complemented. Malformed (non-
        // numeric or wrong arity) counts as skipped.
        "g" | "G" => match numeric_operands(&op.operands, 1) {
            Some(v) => match mode {
                ColorMode::Grayscale => OpTransform::Unchanged,
                ColorMode::Invert => OpTransform::Rewritten(vec![Operation::new(
                    operator,
                    vec![Object::Real((1.0 - clamp01(v[0])) as f32)],
                )]),
            },
            None => OpTransform::Skipped,
        },
        "rg" | "RG" => {
            let Some(rgb) = numeric_operands(&op.operands, 3) else {
                return OpTransform::Skipped;
            };
            match mode {
                ColorMode::Grayscale => {
                    let gray = luma(clamp01(rgb[0]), clamp01(rgb[1]), clamp01(rgb[2]));
                    OpTransform::Rewritten(vec![Operation::new(
                        gray_op_name(operator),
                        vec![Object::Real(clamp01(gray) as f32)],
                    )])
                }
                ColorMode::Invert => OpTransform::Rewritten(vec![Operation::new(
                    operator,
                    rgb.iter()
                        .map(|v| Object::Real((1.0 - clamp01(*v)) as f32))
                        .collect(),
                )]),
            }
        }
        // `k`/`K` take four operands: c m y k.
        "k" | "K" => {
            let Some(cmyk) = numeric_operands(&op.operands, 4) else {
                return OpTransform::Skipped;
            };
            match mode {
                ColorMode::Grayscale => {
                    let gray = cmyk_luma(
                        clamp01(cmyk[0]),
                        clamp01(cmyk[1]),
                        clamp01(cmyk[2]),
                        clamp01(cmyk[3]),
                    );
                    OpTransform::Rewritten(vec![Operation::new(
                        gray_op_name(operator),
                        vec![Object::Real(clamp01(gray) as f32)],
                    )])
                }
                ColorMode::Invert => OpTransform::Rewritten(vec![Operation::new(
                    operator,
                    cmyk.iter()
                        .map(|v| Object::Real((1.0 - clamp01(*v)) as f32))
                        .collect(),
                )]),
            }
        }
        // Color-space-dependent ops: cannot map to gray / invert
        // without resolving /ColorSpace resources — keep and count.
        "sc" | "SC" | "scn" | "SCN" => OpTransform::Skipped,
        _ => OpTransform::Unchanged,
    }
}

/// The non-stroking `g` vs stroking `G` name for a rewritten gray op.
fn gray_op_name(rgb_or_k_operator: &str) -> &'static str {
    // `rg`/`k` are non-stroking → `g`; `RG`/`K` are stroking → `G`.
    if rgb_or_k_operator
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
    {
        "G"
    } else {
        "g"
    }
}

/// Decode a content stream's bytes. Returns `None` when the stream is
/// missing, is not a stream, or its filter cannot be decoded (raw
/// compressed bytes must never be rewritten).
fn decode_stream_bytes(doc: &Document, stream_id: ObjectId) -> Option<Vec<u8>> {
    let object = doc.get_object(stream_id).ok()?;
    let stream = object.as_stream().ok()?;
    match stream.decompressed_content_with_limit(MAX_CONTENT_STREAM) {
        Ok(bytes) => Some(bytes),
        Err(_) => {
            if stream.dict.has(b"Filter") {
                // Still compressed — cannot safely rewrite.
                None
            } else {
                Some(stream.content.clone())
            }
        }
    }
}

/// Rewrite one page's content streams in place. Returns `true` when
/// the page was processed, `false` when a stream failed to decode (the
/// page is left untouched in that case).
fn rewrite_page(
    doc: &mut Document,
    page_id: ObjectId,
    mode: ColorMode,
    report: &mut ColorReport,
) -> bool {
    let stream_ids = doc.get_page_contents(page_id);

    // Phase 1: decode every stream first so a single bad stream
    // leaves the whole page untouched.
    let mut decoded: Vec<(ObjectId, Content)> = Vec::with_capacity(stream_ids.len());
    for stream_id in &stream_ids {
        let Some(bytes) = decode_stream_bytes(doc, *stream_id) else {
            return false;
        };
        match Content::decode(&bytes) {
            Ok(content) => decoded.push((*stream_id, content)),
            Err(_) => return false,
        }
    }

    // Phase 2: transform and write back only streams that changed.
    for (stream_id, content) in decoded {
        let mut changed = false;
        let mut rewritten_here = 0usize;
        let mut new_ops = Vec::with_capacity(content.operations.len());
        for op in content.operations {
            match transform_operation(&op, mode) {
                OpTransform::Unchanged => new_ops.push(op),
                OpTransform::Skipped => {
                    report.ops_skipped += 1;
                    new_ops.push(op);
                }
                OpTransform::Rewritten(replacements) => {
                    rewritten_here += 1;
                    changed = true;
                    new_ops.extend(replacements);
                }
            }
        }
        if !changed {
            continue;
        }
        let encoded = match (Content {
            operations: new_ops,
        })
        .encode()
        {
            Ok(bytes) => bytes,
            Err(_) => {
                // Encoding failed: leave this stream as it was.
                continue;
            }
        };
        report.ops_rewritten += rewritten_here;
        let Ok(object) = doc.get_object_mut(stream_id) else {
            continue;
        };
        let Ok(stream) = object.as_stream_mut() else {
            continue;
        };
        stream.content = encoded;
        // Stored uncompressed — drop filter entries so readers do not
        // try to decode the raw operator bytes.
        stream.dict.remove(b"Filter");
        stream.dict.remove(b"DecodeParms");
    }

    report.pages_processed += 1;
    true
}

/// Shared driver for the color-rewriting ops (`grayscale`,
/// `invert_colors`).
pub(crate) fn rewrite_color_ops(
    input: &[u8],
    pages: Option<&str>,
    limits: &EngineLimits,
    mode: ColorMode,
) -> PdfResult<(Vec<u8>, ColorReport)> {
    let mut doc = load_pdf(input, limits)?;
    reject_encrypted(&doc)?;

    let page_count = doc.get_pages().len() as u32;
    let selected = match pages {
        Some(spec) => parse_page_range(spec, page_count)?,
        None => (1..=page_count).collect(),
    };

    let pages_map = doc.get_pages();
    let mut report = ColorReport::default();
    for page_number in selected {
        let Some(&page_id) = pages_map.get(&page_number) else {
            continue;
        };
        if !rewrite_page(&mut doc, page_id, mode, &mut report) {
            report.pages_skipped += 1;
        }
    }

    if report.pages_processed == 0 && report.pages_skipped > 0 {
        return Err(PdfError::InvalidInput(
            "every selected page had an undecodable content stream".to_string(),
        ));
    }

    let out = save_pdf(&mut doc)?;
    Ok((out, report))
}

/// Convert all color operators on the selected pages to grayscale.
///
/// `pages` is an optional 1-based range spec (`"1-3,8"`, `"all"`);
/// `None` processes every page. Returns the rewritten PDF bytes and a
/// [`ColorReport`] describing what was rewritten and skipped.
/// Encrypted documents are rejected.
pub fn grayscale(
    input: &[u8],
    pages: Option<&str>,
    limits: &EngineLimits,
) -> PdfResult<(Vec<u8>, ColorReport)> {
    rewrite_color_ops(input, pages, limits, ColorMode::Grayscale)
}

#[cfg(test)]
mod tests {
    use lopdf::content::Content;
    use lopdf::{Document, Object, Stream, dictionary};

    use super::*;
    use crate::crates::pdf_engine::ops::extract_text;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    /// Build a one-page PDF whose content stream contains the given
    /// raw operators plus a text op for extractability checks.
    fn pdf_with_ops(ops: &str) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Courier",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let content_id = doc.add_object(Stream::new(dictionary! {}, ops.as_bytes().to_vec()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    /// Decode a page's content stream back into operations.
    fn page_ops(pdf: &[u8]) -> Vec<Operation> {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&1];
        let raw = doc.get_page_content(page_id);
        Content::decode(&raw).unwrap().operations
    }

    #[test]
    fn rgb_fill_becomes_gray_op() {
        let input = pdf_with_ops("1 0 0 rg\n0 0 0 rg\n0.2 0.4 0.6 RG\n");
        let (out, report) = grayscale(&input, None, &limits()).unwrap();
        assert_eq!(report.pages_processed, 1);
        assert_eq!(report.ops_rewritten, 3);
        assert_eq!(report.ops_skipped, 0);
        assert_eq!(report.pages_skipped, 0);

        let ops = page_ops(&out);
        let names: Vec<&str> = ops.iter().map(|o| o.operator.as_str()).collect();
        assert_eq!(names, vec!["g", "g", "G"]);
        // Red → ~0.299 gray.
        let gray = ops[0].operands[0].as_float().unwrap();
        assert!((gray - 0.299).abs() < 0.01, "gray={gray}");
    }

    #[test]
    fn cmyk_and_existing_gray_ops() {
        let input = pdf_with_ops("0 0 0 1 k\n0.5 g\n0 0 0 0 K\n");
        let (out, report) = grayscale(&input, None, &limits()).unwrap();
        assert_eq!(report.ops_rewritten, 2);
        assert_eq!(report.ops_skipped, 0);

        let ops = page_ops(&out);
        let names: Vec<&str> = ops.iter().map(|o| o.operator.as_str()).collect();
        // Pure black CMYK → gray 0.
        assert_eq!(names, vec!["g", "g", "G"]);
        assert_eq!(ops[0].operands[0].as_float().unwrap(), 0.0);
        assert_eq!(ops[2].operands[0].as_float().unwrap(), 1.0);
    }

    #[test]
    fn sc_ops_are_kept_and_counted() {
        let input = pdf_with_ops("/Pattern cs\n0.5 scn\n0.2 0.3 sc\n1 0 0 rg\n");
        let (out, report) = grayscale(&input, None, &limits()).unwrap();
        assert_eq!(report.ops_rewritten, 1);
        assert_eq!(report.ops_skipped, 2);

        let names: Vec<String> = page_ops(&out).iter().map(|o| o.operator.clone()).collect();
        assert_eq!(names, vec!["cs", "scn", "sc", "g"]);
    }

    #[test]
    fn non_color_ops_untouched_and_text_survives() {
        let input = pdf_with_pages(&["keep me"]);
        let (out, report) = grayscale(&input, None, &limits()).unwrap();
        assert_eq!(report.ops_rewritten, 0);
        let texts = extract_text(&out, None, &limits()).unwrap();
        assert!(texts[0].text.contains("keep me"));
    }

    #[test]
    fn undecodable_content_stream_skips_page() {
        // A stream whose declared filter cannot be decoded: the raw
        // bytes are still "compressed", so the page must be skipped
        // rather than rewritten.
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let content_id = doc.add_object(Stream::new(
            dictionary! { "Filter" => "NoSuchFilter" },
            b"1 0 0 rg\n".to_vec(),
        ));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => dictionary! {},
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();

        let err = grayscale(&buf, None, &limits()).unwrap_err();
        assert!(matches!(err, PdfError::InvalidInput(_)), "got {err:?}");
    }

    #[test]
    fn page_range_is_respected() {
        // Two pages: page 1 has rg, page 2 has rg too.
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let mut kids = Vec::new();
        for _ in 0..2 {
            let content_id = doc.add_object(Stream::new(dictionary! {}, b"1 0 0 rg\n".to_vec()));
            let page_id = doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "Contents" => content_id,
                "Resources" => dictionary! {},
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            });
            kids.push(Object::Reference(page_id));
        }
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => 2,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();

        let (out, report) = grayscale(&buf, Some("2"), &limits()).unwrap();
        assert_eq!(report.pages_processed, 1);
        let doc = Document::load_mem(&out).unwrap();
        let pages = doc.get_pages();
        let p1 = Content::decode(&doc.get_page_content(pages[&1])).unwrap();
        let p2 = Content::decode(&doc.get_page_content(pages[&2])).unwrap();
        assert_eq!(p1.operations[0].operator, "rg");
        assert_eq!(p2.operations[0].operator, "g");
    }
}
