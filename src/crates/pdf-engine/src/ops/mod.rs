//! PDF operations — synchronous functions over in-memory byte buffers.
//!
//! # Limit enforcement
//!
//! [`EngineLimits`](crate::crates::core::engine::EngineLimits) are enforced in exactly two places:
//!
//! 1. **Input side** — every op loads through [`load::load_pdf`], which
//!    rejects oversized inputs (`max_input_bytes`) and documents over
//!    `max_pages` before any work happens.
//! 2. **Output side** — operations that produce multiple artifacts
//!    (`split`) check `max_outputs` before producing them.
//!
//! `merge` additionally enforces `max_pages` against the *combined*
//! page count of all inputs.
//!
//! All ops return [`PdfError`] on malformed input and never panic;
//! the `lopdf` load is wrapped in `catch_unwind` for hostile PDFs.

mod annotations;
mod article_threads;
mod attachments;
mod auto_orient;
mod bates;
mod booklet;
mod cert_info;
mod clean_scan;
mod compare;
mod compress;
mod content_audit;
mod convert;
mod crop;
mod crop_view;
mod decrypt;
mod dedup_images;
mod deskew;
mod doc_time;
mod draw_shapes;
mod draw_text;
mod duplicate_pages;
mod edit_field_props;
mod edit_text;
mod encrypt;
mod extract;
mod extract_images;
mod extract_metadata;
mod extract_tables;
mod fill_form;
mod flatten;
mod font_audit;
mod form_fields;
mod freehand;
mod garbage_collect;
mod grayscale;
mod headers_footers;
mod info;
mod insert_blank;
mod inspect_fonts;
mod inspect_javascript;
mod interleave;
mod invert_colors;
mod layers;
mod linearize;
mod links;
mod load;
mod merge;
mod merge_modes;
mod named_destinations;
mod nup;
mod object_report;
mod ocr;
mod optional_content;
mod outline;
mod outline_from_headings;
mod overlay;
mod package_files;
mod page_boxes;
mod page_count;
mod page_import;
mod page_labels;
mod page_size;
mod page_transitions;
mod pages;
mod pdfa;
mod pdfua;
mod pdfx;
mod permissions_report;
mod portfolio;
mod poster;
mod print_marks;
pub(crate) mod raster;
mod redact;
mod redact_pages;
mod redact_regions;
mod remove_blank_pages;
mod remove_hidden_text;
mod remove_images;
mod remove_objects;
mod remove_pages;
mod render;
mod reorder;
mod repair;
mod replace_image;
mod resize;
mod rotate;
mod rotate_view;
mod sanitize;
mod scale_content;
mod searchable_pdf;
mod set_metadata;
mod set_xmp_metadata;
mod sign;
mod sign_timestamp;
mod single_page;
mod split;
mod split_bookmarks;
mod split_modes;
mod stamp;
mod stream_recompress;
mod structure_tree;
mod text;
mod text_markup;
mod text_spans;
mod timestamp;
mod trim_boxes;
mod unlock_fields;
mod validate;
mod verify_signature;
mod watermark;

pub use annotations::{
    AnnotFilter, AnnotationInfo, CommentIcon, CommentSpec, FreeTextSpec, add_comment, add_freetext,
    list_annotations, remove_annotations,
};
pub use article_threads::{ArticleInfo, list_articles, remove_articles};
pub use auto_orient::{OrientMode, PageOrientation, detect_orientation, normalize_orientation};
pub use booklet::{BookletMode, BookletSize, BookletSpec, booklet};
pub use cert_info::{
    CertSummary, SignerCert, extract_signer_certificates, parse_cert_with_openssl,
};
pub use compare::{CompareMode, CompareReport, DiffEntry, DiffKind, compare};
pub use compress::compress;
pub use convert::{ConvertTarget, convert};
pub use crop::crop;
pub use crop_view::{clear_crop, set_view_crop};
pub use decrypt::decrypt;
pub use doc_time::{DocDates, get_document_dates, set_document_dates};
pub use draw_shapes::{DrawSpec, Shape, ShapeStyle, draw_shapes};
pub use draw_text::{DrawTextSpec, draw_text};
pub use duplicate_pages::{DupPos, duplicate_pages};
pub use edit_field_props::{FieldPatch, FieldPatchReport, edit_field_props};
pub use edit_text::{EditOutcome, EditReport, TextEdit, edit_text};
pub use encrypt::{PdfPermissions, encrypt};
pub use extract::extract_pages;
pub use extract_images::{EmbeddedImage, extract_images};
pub use extract_metadata::{PdfDate, PdfMetadata, XmpMetadata, extract_metadata};
pub use fill_form::{clear_form, fill_form};
pub use flatten::flatten;
pub use form_fields::{
    FieldInfo, TextFieldSpec, create_text_field, delete_form_field, list_form_fields,
};
pub use freehand::{InkInfo, InkSpec, Stroke, add_ink, list_ink, remove_ink};
pub use garbage_collect::{GcReport, garbage_collect};
pub use grayscale::{ColorReport, grayscale};
pub use headers_footers::{HeaderFooterSpec, PageNumberStyle, add_page_numbers, headers_footers};
pub use info::{PdfInfo, inspect};
pub use insert_blank::{InsertPos, PageSize, insert_blank};
pub use inspect_javascript::{JsInfo, inspect_javascript};
pub use interleave::interleave;
pub use invert_colors::invert_colors;
pub use linearize::{LinearizeReport, linearize};
pub use merge::merge;
pub use merge_modes::{MergeMode, merge_mode};
pub use nup::nup;
pub use ocr::{OcrLine, OcrOptions, OcrPageDetail, OcrWord, ocr, ocr_detailed, ocr_languages};
pub use outline::{OutlineEntry, OutlineItemSpec, list_outline, remove_outline, set_outline};
pub use overlay::overlay;
pub use package_files::{PackageLayout, package_documents};
pub use page_count::page_count;
pub use page_import::{ImportPos, import_pages, replace_pages};
pub use page_labels::{clear_page_labels, get_page_labels, set_page_labels};
pub use page_size::{PageSizeInfo, PageSizeSpec, SizeUnit, get_page_sizes, set_page_size};
pub use page_transitions::{
    PageTransition, TransitionDir, TransitionMotion, TransitionSpec, TransitionType,
    clear_transitions, list_transitions, set_transition,
};
pub use pages::parse_page_range;
pub use pdfa::{PdfaLevel, PdfaReport, to_pdfa};
pub use pdfua::{PdfuaLevel, PdfuaReport, to_pdfua};
pub use pdfx::{PdfxLevel, PdfxReport, to_pdfx};
pub use portfolio::{PortfolioEntry, create_portfolio, list_portfolio};
pub use poster::poster;
pub use print_marks::{DEFAULT_PRINT_MARK_OFFSET, PrintMarksSpec, add_print_marks};
pub use raster::{PageImage, PageImageReport, RasterFormat, RasterOptions, RenderedPage};
pub use redact::redact;
pub use remove_blank_pages::{BlankReport, remove_blank_pages};
pub use remove_images::{RemoveReport, remove_images};
pub use remove_objects::{
    RemoveObjectsReport, remove_embedded_files, remove_javascript_names, remove_named_objects,
};
pub use remove_pages::remove_pages;
pub use render::{RenderFormat, RenderPageOptions, RenderRegion, render, render_pages};
pub use reorder::{reorder, swap_pages};
pub use repair::{RepairReport, repair};
pub use replace_image::{ImageInputFormat, replace_image};
pub use resize::{ResizeSpec, resize};
pub use rotate::{RotateDegrees, rotate};
pub use rotate_view::{get_page_rotations, normalize_rotations, set_page_rotation};
pub use sanitize::sanitize;
pub use scale_content::scale_content;
pub use searchable_pdf::{SearchableReport, make_searchable};
pub use set_metadata::{MetadataPatch, clear_metadata, set_metadata};
pub use set_xmp_metadata::{get_xmp, set_xmp};
pub use sign::{SignCredentials, SignOptions, remove_signatures, sign};
pub use sign_timestamp::{SignedTimestamp, has_timestamps, list_signature_timestamps};
pub use single_page::single_page;
pub use split::split;
pub use split_bookmarks::split_at_outline;
pub use split_modes::{SplitMode, split_mode};
pub use stamp::{StampOptions, stamp};
pub use structure_tree::{StructElementInfo, StructReport, inspect_structure};
pub use text::{PageText, extract_text};
pub use text_markup::{MarkupKind, text_markup};
pub use text_spans::{TextMatch, TextSpan, extract_spans, search_text};
pub use timestamp::{
    DigestReport, DocumentTimestamp, TimestampAlgorithm, TimestampToken, build_timestamp_request,
    document_digest, request_external_timestamp, timestamp_document, timestamp_query,
};
pub use unlock_fields::{FieldFlagReport, lock_fields, unlock_fields};
pub use validate::{ValidationReport, validate};
pub use verify_signature::{
    CryptoSignatureInfo, CryptoSignatureReport, SignatureInfo, SignatureReport, verify_signatures,
    verify_signatures_crypto,
};
pub use watermark::{MarkPosition, WatermarkOptions, watermark};

pub use clean_scan::{CleanMode, CleanReport, clean_scan};
pub use inspect_fonts::{FontInfo, inspect_fonts};
pub use named_destinations::{
    FitMode, NamedDest, NamedDestInfo, add_named_destinations, list_named_destinations,
    remove_named_destination,
};
pub use object_report::{ObjectReport, ObjectStats, StreamInfo, object_report};
pub use optional_content::{LayerInfo, list_layers, remove_oc_properties, set_layer_visibility};
pub use redact_pages::{RedactReplacement, redact_pages};
pub use redact_regions::{RedactReport, Redaction, redact_regions};

pub use bates::{BatesSpec, add_bates};
pub use deskew::{DeskewPageReport, DeskewReport, deskew};
pub use layers::{OcUsage, list_oc_marked_content};
pub use page_boxes::{PageBox, PageBoxInfo, get_page_boxes, remove_page_box, set_page_box};
pub use trim_boxes::{
    BoxPreset, DEFAULT_BLEED_PT, DEFAULT_MARKS_OFFSET_PT, MAX_BLEED_PT,
    PageBoxInfo as TrimPageBoxInfo, TrimPreset, apply_trim_preset, get_trim_boxes,
    get_trim_boxes_for_pages,
};

pub use stream_recompress::{RecompressReport, recompress_streams};

pub use attachments::{
    AttachmentSpec, EmbeddedFileInfo, add_attachment, extract_attachment, list_attachments,
    remove_attachment,
};
pub use content_audit::{ContentAudit, ContentRisk, RiskKind, audit_content};
pub use dedup_images::{DedupReport, dedup_images};
pub use font_audit::{BASE14, FontAudit, FontRisk, audit_fonts};
pub use links::{LinkInfo, LinkSpec, LinkTarget, add_links, list_links, remove_links};
pub use outline_from_headings::{HeadingSpec, OutlineBuildReport, build_outline_from_text};
pub use permissions_report::{
    EncryptionDetails, PermissionsReport, ReportedPermissions, permissions_report,
};
pub use remove_hidden_text::{HiddenTextReport, remove_hidden_text};

pub use extract_tables::{KvPair, Table, TableReport, extract_key_values, extract_tables};

#[cfg(test)]
pub(crate) mod test_utils {
    //! Helpers that build tiny valid PDFs with `lopdf` itself — no
    //! fixture files needed.

    use lopdf::content::{Content, Operation};
    use lopdf::{Document, Object, Stream, dictionary};

    /// Build a PDF with one text page per entry in `page_texts`.
    /// Returns the serialized bytes.
    pub(crate) fn pdf_with_pages(page_texts: &[&str]) -> Vec<u8> {
        pdf_with_pages_and_title(page_texts, "KalaDoc test document")
    }

    /// Like [`pdf_with_pages`] but with a caller-chosen `/Title`.
    pub(crate) fn pdf_with_pages_and_title(page_texts: &[&str], title: &str) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");

        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Courier",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! {
                "F1" => font_id,
            },
        });

        let mut kids = Vec::with_capacity(page_texts.len());
        for text in page_texts {
            let content = Content {
                operations: vec![
                    Operation::new("BT", vec![]),
                    Operation::new("Tf", vec!["F1".into(), 12.into()]),
                    Operation::new("Td", vec![100.into(), 700.into()]),
                    Operation::new("Tj", vec![Object::string_literal(*text)]),
                    Operation::new("ET", vec![]),
                ],
            };
            let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
            let page_id = doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "Contents" => content_id,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            });
            kids.push(Object::Reference(page_id));
        }

        let pages = dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => page_texts.len() as i64,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        };
        doc.objects.insert(pages_id, Object::Dictionary(pages));

        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        let info_id = doc.add_object(dictionary! {
            "Title" => Object::string_literal(title),
            "Author" => Object::string_literal("KalaDoc Tests"),
            "Creator" => Object::string_literal("kaladoc-test-utils"),
            "Producer" => Object::string_literal("lopdf"),
        });
        doc.trailer.set("Root", catalog_id);
        doc.trailer.set("Info", info_id);

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        buf
    }

    /// Page count of a serialized PDF, for assertions.
    pub(crate) fn page_count(pdf: &[u8]) -> usize {
        Document::load_mem(pdf).unwrap().get_pages().len()
    }
}
