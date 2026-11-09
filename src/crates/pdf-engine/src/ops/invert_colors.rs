//! Invert the color operands of page content streams.
//!
//! PDF has **no "invert colors" operator**, so this op rewrites the
//! color-setting operators' operands to their complement *within the
//! same color space*:
//!
//! - `g v` → `g (1−v)`; `G v` → `G (1−v)`
//! - `rg r g b` → `rg (1−r) (1−g) (1−b)`;
//!   `RG r g b` → `RG (1−r) (1−g) (1−b)`
//! - `k c m y k` → `k (1−c) (1−m) (1−y) (1−k)`;
//!   `K c m y k` → `K (1−c) (1−m) (1−y) (1−k)`
//!
//! # Honest scope / limitations
//!
//! - **CMYK inversion is approximate.** Complementing the four CMYK
//!   components is not the visual inverse of the printed color — real
//!   CMYK→RGB conversion is device-dependent, and inverting in CMYK
//!   space does not produce the RGB complement. It is the best
//!   operand-level rewrite available without resolving each page's
//!   `/ColorSpace` resources.
//! - `sc` / `SC` / `scn` / `SCN` are kept unchanged and counted in
//!   [`ColorReport::ops_skipped`]: their operands belong to whatever
//!   color space `cs`/`CS` selected, which this op does not resolve.
//!   `cs`, `CS` and `ri` pass through untouched.
//! - Operands are clamped to `[0, 1]` before complementing, so
//!   out-of-range values (technically invalid PDF) still produce
//!   in-range output.
//! - Only the page's own `/Contents` streams are rewritten — colors
//!   inside Form XObjects, Type3 glyphs, patterns and inline images
//!   keep their original values.
//! - Rewritten streams are stored uncompressed (`/Filter` and
//!   `/DecodeParms` dropped); output may be larger than the input.
//! - A page whose content stream cannot be decoded is skipped whole
//!   (counted in [`ColorReport::pages_skipped`]); if *every* selected
//!   page fails, an error is returned instead of an unchanged
//!   document.
//!
//! Shares its decode/transform/write machinery with [`grayscale`].

use crate::crates::core::engine::EngineLimits;

use crate::crates::pdf_engine::error::PdfResult;

use super::grayscale::{ColorMode, ColorReport, rewrite_color_ops};

/// Invert the operands of every color-setting operator on the
/// selected pages. See the module docs for the exact rewrite rules
/// and limitations.
///
/// `pages` is an optional 1-based range spec (`"1-3,8"`, `"all"`);
/// `None` processes every page. Returns the rewritten PDF bytes and a
/// [`ColorReport`]. Encrypted documents are rejected.
pub fn invert_colors(
    input: &[u8],
    pages: Option<&str>,
    limits: &EngineLimits,
) -> PdfResult<(Vec<u8>, ColorReport)> {
    rewrite_color_ops(input, pages, limits, ColorMode::Invert)
}

#[cfg(test)]
mod tests {
    use lopdf::content::Content;
    use lopdf::{Document, Object, Stream, dictionary};

    use super::*;
    use crate::crates::pdf_engine::ops::grayscale::grayscale;
    use crate::crates::pdf_engine::ops::test_utils::pdf_with_pages;

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    /// One-page PDF with the given raw content-stream operators.
    fn pdf_with_ops(ops: &str) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let content_id = doc.add_object(Stream::new(dictionary! {}, ops.as_bytes().to_vec()));
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
        buf
    }

    fn page_ops(pdf: &[u8]) -> Vec<lopdf::content::Operation> {
        let doc = Document::load_mem(pdf).unwrap();
        let page_id = doc.get_pages()[&1];
        Content::decode(&doc.get_page_content(page_id))
            .unwrap()
            .operations
    }

    #[test]
    fn complements_each_color_space() {
        let input = pdf_with_ops("0.2 g\n1 0 0 rg\n0 0 0 1 k\n");
        let (out, report) = invert_colors(&input, None, &limits()).unwrap();
        assert_eq!(report.pages_processed, 1);
        assert_eq!(report.ops_rewritten, 3);
        assert_eq!(report.ops_skipped, 0);

        let ops = page_ops(&out);
        let names: Vec<&str> = ops.iter().map(|o| o.operator.as_str()).collect();
        assert_eq!(names, vec!["g", "rg", "k"]);
        // 0.2 → 0.8 (f32 rounding tolerance).
        let g = ops[0].operands[0].as_float().unwrap();
        assert!((g - 0.8).abs() < 0.001, "g={g}");
        // 1 0 0 → 0 1 1.
        let rgb: Vec<f32> = ops[1]
            .operands
            .iter()
            .map(|o| o.as_float().unwrap())
            .collect();
        assert_eq!(rgb, vec![0.0, 1.0, 1.0]);
        // 0 0 0 1 → 1 1 1 0.
        let cmyk: Vec<f32> = ops[2]
            .operands
            .iter()
            .map(|o| o.as_float().unwrap())
            .collect();
        assert_eq!(cmyk, vec![1.0, 1.0, 1.0, 0.0]);
    }

    #[test]
    fn double_invert_restores_original() {
        let input = pdf_with_ops("0.3 0.6 0.9 rg\n0.1 g\n");
        let (once, _) = invert_colors(&input, None, &limits()).unwrap();
        let (twice, report) = invert_colors(&once, None, &limits()).unwrap();
        assert_eq!(report.ops_rewritten, 2);
        let ops = page_ops(&twice);
        let rgb: Vec<f32> = ops[0]
            .operands
            .iter()
            .map(|o| o.as_float().unwrap())
            .collect();
        for (got, want) in rgb.iter().zip([0.3f32, 0.6, 0.9]) {
            assert!((got - want).abs() < 0.002, "rgb={rgb:?}");
        }
        let g = ops[1].operands[0].as_float().unwrap();
        assert!((g - 0.1).abs() < 0.002, "g={g}");
    }

    #[test]
    fn sc_ops_skipped_and_stroking_ops_inverted() {
        let input = pdf_with_ops("0.5 scn\n0 0 0 RG\n0.25 0.5 0.75 1 K\n");
        let (out, report) = invert_colors(&input, None, &limits()).unwrap();
        assert_eq!(report.ops_rewritten, 2);
        assert_eq!(report.ops_skipped, 1);

        let ops = page_ops(&out);
        let names: Vec<&str> = ops.iter().map(|o| o.operator.as_str()).collect();
        assert_eq!(names, vec!["scn", "RG", "K"]);
        // Stroking ops keep their uppercase names.
        let rg = ops[1]
            .operands
            .iter()
            .map(|o| o.as_float().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rg, vec![1.0, 1.0, 1.0]);
        let k = ops[2]
            .operands
            .iter()
            .map(|o| o.as_float().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(k, vec![0.75, 0.5, 0.25, 0.0]);
    }

    #[test]
    fn plain_pages_are_noop() {
        let input = pdf_with_pages(&["hello"]);
        let (out, report) = invert_colors(&input, None, &limits()).unwrap();
        assert_eq!(report.ops_rewritten, 0);
        assert_eq!(report.ops_skipped, 0);
        let doc = Document::load_mem(&out).unwrap();
        let page_id = doc.get_pages()[&1];
        let raw = doc.get_page_content(page_id);
        let content = String::from_utf8_lossy(&raw);
        assert!(content.contains("(hello)"));
    }

    #[test]
    fn shares_report_type_with_grayscale() {
        // Sanity: both ops return the same report shape.
        let input = pdf_with_pages(&["x"]);
        let (_, report): (Vec<u8>, ColorReport) = grayscale(&input, None, &limits()).unwrap();
        assert_eq!(report.pages_processed, 1);
    }
}
