//! `kaladoc pdf <op>` — local-file PDF operations via `PdfEngine`.
//!
//! Commands read PDF files from disk, run the synchronous pdf-engine
//! operation on a blocking thread, and write byte outputs to
//! `--output-file`/`--output-dir` or emit structured reports as JSON.
//! Page numbers are 1-based; `--pages` uses the shared range grammar
//! (`"1-3,8"`, `"all"`). Passwords may be `-` to prompt without echo.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::crates::core::engine::EngineLimits;
use crate::crates::pdf_engine::{
    CompareMode, ConvertTarget, MarkPosition, PdfEngine, PdfError, PdfPermissions, RenderFormat,
    ResizeSpec, SignCredentials, SignOptions, StampOptions, WatermarkOptions,
};

use super::super::args::PdfArgs;
use super::{CliError, Context};

/// Input-size/page caps mirroring the job-handler limits — generous
/// for interactive CLI use while still bounding hostile files.
fn limits() -> EngineLimits {
    EngineLimits {
        max_input_bytes: Some(256 * 1024 * 1024),
        max_pages: Some(10_000),
        ..Default::default()
    }
}

fn engine() -> PdfEngine {
    PdfEngine::with_limits(limits())
}

pub async fn run(ctx: &Context, args: &PdfArgs) -> Result<i32, CliError> {
    match args {
        PdfArgs::Merge { input, output_file } => {
            let inputs = read_inputs(input)?;
            let out = blocking(move || {
                let refs: Vec<&[u8]> = inputs.iter().map(Vec::as_slice).collect();
                engine().merge(&refs)
            })
            .await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        PdfArgs::Split {
            input,
            pages_per_file,
            ranges,
            every,
            output_dir,
            output_file,
        } => {
            let data = read_file(input)?;
            let mode_count =
                pages_per_file.is_some() as u8 + ranges.is_some() as u8 + every.is_some() as u8;
            if mode_count != 1 {
                return Err(CliError::Usage(
                    "split requires exactly one of --pages-per-file, --ranges, --every".to_string(),
                ));
            }
            let outputs = if let Some(n) = pages_per_file {
                let n = *n;
                blocking(move || engine().split(&data, n)).await?
            } else if let Some(spec) = ranges.clone() {
                blocking(move || {
                    engine().split_mode(
                        &data,
                        crate::crates::pdf_engine::SplitMode::ByRange { spec: &spec },
                    )
                })
                .await?
            } else {
                let n = every.unwrap();
                blocking(move || {
                    engine().split_mode(&data, crate::crates::pdf_engine::SplitMode::EveryNth { n })
                })
                .await?
            };
            write_split_outputs(
                input,
                &outputs,
                output_dir.as_deref(),
                output_file.as_deref(),
            )?;
            Ok(0)
        }
        PdfArgs::Rotate {
            input,
            pages,
            degrees,
            output_file,
        } => {
            let data = read_file(input)?;
            let degrees = *degrees;
            let pages = pages.clone();
            let out = blocking(move || engine().rotate_degrees(&data, &pages, degrees)).await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        PdfArgs::Crop {
            input,
            pages,
            bx0y0x1y1,
            output_file,
        } => {
            let data = read_file(input)?;
            let crop_box = parse_box(bx0y0x1y1.as_deref())?;
            let pages = pages.clone();
            let out = blocking(move || engine().crop(&data, &pages, crop_box)).await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        PdfArgs::Resize {
            input,
            pages,
            scale,
            width,
            height,
            output_file,
        } => {
            let data = read_file(input)?;
            let spec = resize_spec(*scale, *width, *height)?;
            let pages = pages.clone();
            let out = blocking(move || engine().resize(&data, &pages, &spec)).await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        PdfArgs::Reorder {
            input,
            order,
            output_file,
        } => {
            let data = read_file(input)?;
            let order = order.clone();
            let out = blocking(move || engine().reorder(&data, &order)).await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        PdfArgs::RemovePages {
            input,
            pages,
            output_file,
        } => {
            let data = read_file(input)?;
            let pages = pages.clone();
            let out = blocking(move || engine().remove_pages(&data, &pages)).await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        PdfArgs::ExtractPages {
            input,
            pages,
            output_file,
        } => {
            let data = read_file(input)?;
            let pages = pages.clone();
            let out = blocking(move || engine().extract_pages(&data, &pages)).await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        PdfArgs::Compress { input, output_file } => {
            let data = read_file(input)?;
            let out = blocking(move || engine().compress(&data)).await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        PdfArgs::Repair { input, output_file } => {
            let data = read_file(input)?;
            let report = blocking(move || engine().repair(&data)).await?;
            if let Some(path) = output_file {
                write_file(path, &report.bytes)?;
            }
            // The repaired bytes were just written (or discarded); don't
            // dump them into the JSON report as a number array.
            let summary = serde_json::json!({
                "bytes": report.bytes.len(),
                "notes": report.notes,
            });
            if ctx.emit_structured(&summary)? {
                return Ok(0);
            }
            println!("{}", serde_json::to_string_pretty(&summary)?);
            Ok(0)
        }
        PdfArgs::Flatten { input, output_file } => {
            let data = read_file(input)?;
            let out = blocking(move || engine().flatten(&data)).await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        PdfArgs::Sanitize { input, output_file } => {
            let data = read_file(input)?;
            let out = blocking(move || engine().sanitize(&data)).await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        PdfArgs::Overlay {
            input,
            overlay,
            pages,
            output_file,
        } => {
            let data = read_file(input)?;
            let overlay_data = read_file(overlay)?;
            let pages = pages.clone();
            let out =
                blocking(move || engine().overlay(&data, &overlay_data, Some(pages.as_str())))
                    .await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        PdfArgs::Watermark {
            input,
            text,
            position,
            font_size,
            opacity,
            rotation,
            pages,
            color,
            output_file,
        } => {
            let data = read_file(input)?;
            let mut opts = WatermarkOptions {
                text: text.clone(),
                pages: pages.clone(),
                ..Default::default()
            };
            opts.position = parse_position(position)?;
            if let Some(v) = font_size {
                opts.font_size = *v;
            }
            if let Some(v) = opacity {
                opts.opacity = *v;
            }
            if let Some(v) = rotation {
                opts.rotation_degrees = *v;
            }
            if let Some(c) = color {
                opts.color = Some(parse_color(c)?);
            }
            let out = blocking(move || engine().watermark(&data, &opts)).await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        PdfArgs::Stamp {
            input,
            text,
            image,
            position,
            scale,
            opacity,
            pages,
            output_file,
        } => {
            let data = read_file(input)?;
            let mut opts = StampOptions {
                pages: pages.clone(),
                ..Default::default()
            };
            opts.position = parse_position(position)?;
            if let Some(t) = text {
                opts.text = Some(t.clone());
            }
            if let Some(p) = image {
                opts.image_png = Some(fs::read(p)?);
            }
            if opts.text.is_none() && opts.image_png.is_none() {
                return Err(CliError::Usage(
                    "stamp requires --text or --image".to_string(),
                ));
            }
            if let Some(v) = scale {
                opts.scale = *v;
            }
            if let Some(v) = opacity {
                opts.opacity = *v;
            }
            let out = blocking(move || engine().stamp(&data, &opts)).await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        PdfArgs::Redact {
            input,
            term,
            pages,
            output_file,
        } => {
            let data = read_file(input)?;
            let terms = term.clone();
            let pages = pages.clone();
            let out = blocking(move || engine().redact(&data, &terms, pages.as_deref())).await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        PdfArgs::Encrypt {
            input,
            password,
            owner_password,
            no_print,
            no_copy,
            no_modify,
            no_annotations,
            output_file,
        } => {
            let data = read_file(input)?;
            let user_password = resolve_password(password.as_deref(), "User password")?;
            if user_password.is_empty() {
                return Err(CliError::Usage(
                    "encrypt requires a non-empty user password".to_string(),
                ));
            }
            let owner_password = match owner_password.as_deref() {
                None => None,
                Some("-") => Some(prompt_password("Owner password")?),
                Some(v) => Some(v.to_string()),
            };
            let owner = owner_password.clone();
            let permissions = PdfPermissions {
                allow_print: !*no_print,
                allow_copy: !*no_copy,
                allow_modify: !*no_modify,
                allow_annotations: !*no_annotations,
            };
            let out = blocking(move || {
                engine().encrypt(&data, &user_password, owner.as_deref(), permissions)
            })
            .await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        PdfArgs::Decrypt {
            input,
            password,
            output_file,
        } => {
            let data = read_file(input)?;
            let password = resolve_password(password.as_deref(), "Password")?;
            let out = blocking(move || engine().decrypt(&data, &password)).await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        PdfArgs::Sign {
            input,
            cert,
            key,
            key_password,
            p12,
            p12_password,
            reason,
            location,
            contact,
            signer_name,
            output_file,
        } => {
            let data = read_file(input)?;
            let credentials = if let Some(bundle) = p12 {
                SignCredentials::Pkcs12 {
                    path: bundle.clone(),
                    password: optional_password(p12_password.as_deref(), "PKCS#12 passphrase")?,
                }
            } else {
                let (Some(c), Some(k)) = (cert, key) else {
                    return Err(CliError::Usage(
                        "sign requires --cert/--key or --p12".to_string(),
                    ));
                };
                SignCredentials::Pem {
                    certificate: c.clone(),
                    private_key: k.clone(),
                    private_key_password: optional_password(
                        key_password.as_deref(),
                        "Key passphrase",
                    )?,
                }
            };
            let options = SignOptions {
                reason: reason.clone(),
                location: location.clone(),
                contact_info: contact.clone(),
                signer_name: signer_name.clone(),
                ..Default::default()
            };
            let out = blocking(move || engine().sign(&data, &credentials, &options)).await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        PdfArgs::RemoveSignatures { input, output_file } => {
            let data = read_file(input)?;
            let out = blocking(move || engine().remove_signatures(&data)).await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        PdfArgs::VerifySignature {
            input,
            crypto,
            ca_file,
        } => {
            let data = read_file(input)?;
            if *crypto {
                let anchors = match ca_file {
                    Some(path) => Some(fs::read(path)?),
                    None => None,
                };
                let report =
                    blocking(move || engine().verify_signatures_crypto(&data, anchors.as_deref()))
                        .await?;
                emit_report(ctx, &report)?;
            } else {
                let report = blocking(move || engine().verify_signatures(&data)).await?;
                emit_report(ctx, &report)?;
            }
            Ok(0)
        }
        PdfArgs::Info { input } => {
            let data = read_file(input)?;
            let info = blocking(move || engine().inspect(&data)).await?;
            emit_report(ctx, &info)?;
            Ok(0)
        }
        PdfArgs::PageCount { input } => {
            let data = read_file(input)?;
            let count = blocking(move || engine().page_count(&data)).await?;
            if ctx.emit_structured(&count)? {
                return Ok(0);
            }
            println!("{count}");
            Ok(0)
        }
        PdfArgs::Validate { input } => {
            let data = read_file(input)?;
            let report = blocking(move || engine().validate(&data)).await?;
            emit_report(ctx, &report)?;
            Ok(0)
        }
        PdfArgs::ExtractMetadata { input } => {
            let data = read_file(input)?;
            let report = blocking(move || engine().extract_metadata(&data)).await?;
            emit_report(ctx, &report)?;
            Ok(0)
        }
        PdfArgs::ExtractImages {
            input,
            pages,
            output_dir,
        } => {
            let data = read_file(input)?;
            let pages = pages.clone();
            let images = blocking(move || engine().extract_images(&data, pages.as_deref())).await?;
            if let Some(dir) = output_dir {
                fs::create_dir_all(dir)?;
                let mut meta = Vec::with_capacity(images.len());
                for (i, img) in images.iter().enumerate() {
                    let safe = img
                        .name
                        .chars()
                        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                        .collect::<String>();
                    let path = dir.join(format!("p{}-{}-{}.bin", img.page, i, safe));
                    fs::write(&path, &img.data)?;
                    meta.push(serde_json::json!({
                        "page": img.page,
                        "name": img.name,
                        "width": img.width,
                        "height": img.height,
                        "color_space": img.color_space,
                        "bits_per_component": img.bits_per_component,
                        "filter": img.filter,
                        "bytes": img.data.len(),
                        "file": path.file_name().map(|n| n.to_string_lossy().into_owned()),
                    }));
                }
                if ctx.emit_structured(&meta)? {
                    return Ok(0);
                }
                println!("{}", serde_json::to_string_pretty(&meta)?);
            } else {
                // No output dir: report metadata only, no stream bytes.
                let meta: Vec<_> = images
                    .iter()
                    .map(|img| {
                        serde_json::json!({
                            "page": img.page,
                            "name": img.name,
                            "width": img.width,
                            "height": img.height,
                            "color_space": img.color_space,
                            "bits_per_component": img.bits_per_component,
                            "filter": img.filter,
                            "bytes": img.data.len(),
                        })
                    })
                    .collect();
                if ctx.emit_structured(&meta)? {
                    return Ok(0);
                }
                println!("{}", serde_json::to_string_pretty(&meta)?);
            }
            Ok(0)
        }
        PdfArgs::ExtractText {
            input,
            pages,
            output_file,
        } => {
            let data = read_file(input)?;
            let pages = pages.clone();
            let pages_text =
                blocking(move || engine().extract_text(&data, pages.as_deref())).await?;
            let text = format_page_text(&pages_text);
            write_text(output_file.as_deref(), &text)?;
            Ok(0)
        }
        PdfArgs::Compare { a, b, mode } => {
            let data_a = read_file(a)?;
            let data_b = read_file(b)?;
            let mode = parse_compare_mode(mode)?;
            let report = blocking(move || engine().compare(&data_a, &data_b, mode)).await?;
            emit_report(ctx, &report)?;
            Ok(0)
        }
        PdfArgs::Render {
            input,
            pages,
            dpi,
            format,
            output_file,
        } => {
            let data = read_file(input)?;
            let format = parse_render_format(format)?;
            let dpi = *dpi;
            let pages = pages.clone();
            let out =
                blocking(move || engine().render(&data, pages.as_deref(), dpi, format)).await?;
            write_file(output_file, &out)?;
            Ok(0)
        }
        PdfArgs::Convert {
            input,
            to,
            pages,
            output_file,
        } => {
            let data = read_file(input)?;
            let target = parse_convert_target(to)?;
            let pages = pages.clone();
            let out = blocking(move || engine().convert(&data, target, pages.as_deref())).await?;
            let text = String::from_utf8_lossy(&out).into_owned();
            write_text(output_file.as_deref(), &text)?;
            Ok(0)
        }
        PdfArgs::Ocr {
            input,
            pages,
            output_file,
        } => {
            let data = read_file(input)?;
            let pages = pages.clone();
            let pages_text = blocking(move || engine().ocr(&data, pages.as_deref())).await?;
            let text = format_page_text(&pages_text);
            write_text(output_file.as_deref(), &text)?;
            Ok(0)
        }
        PdfArgs::OcrLanguages => {
            let langs = blocking(|| engine().ocr_languages()).await?;
            if ctx.emit_structured(&langs)? {
                return Ok(0);
            }
            for lang in langs {
                println!("{lang}");
            }
            Ok(0)
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run a synchronous engine call on the blocking pool.
async fn blocking<F, R>(f: F) -> Result<R, CliError>
where
    F: FnOnce() -> Result<R, PdfError> + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| CliError::Internal(format!("pdf worker task failed: {e}")))?
        .map_err(pdf_err)
}

fn pdf_err(e: PdfError) -> CliError {
    match e {
        PdfError::UnsupportedOperation(m) => CliError::Unsupported(m),
        PdfError::InvalidInput(m) | PdfError::InvalidPageRange(m) => CliError::Usage(m),
        PdfError::MissingTool(m) => CliError::Unsupported(format!("missing tool: {m}")),
        PdfError::ExternalTool { tool, detail } => {
            CliError::Internal(format!("external tool {tool} failed: {detail}"))
        }
        PdfError::LimitExceeded(m) => CliError::Validation(m),
        PdfError::Encrypted => {
            CliError::Validation("document is encrypted; decrypt it first".to_string())
        }
        PdfError::Io(io) => CliError::Io(io),
        other => CliError::Validation(other.to_string()),
    }
}

fn read_file(path: &Path) -> Result<Vec<u8>, CliError> {
    fs::read(path).map_err(|e| {
        CliError::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", path.display()),
        ))
    })
}

fn read_inputs(paths: &[PathBuf]) -> Result<Vec<Vec<u8>>, CliError> {
    paths.iter().map(|p| read_file(p)).collect()
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    // Write-then-rename keeps a failed write from leaving a truncated
    // output at the target path.
    let tmp = path.with_extension("kaladoc-tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn write_text(path: Option<&Path>, text: &str) -> Result<(), CliError> {
    match path {
        Some(p) => {
            if let Some(parent) = p.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)?;
            }
            fs::write(p, text)?;
            Ok(())
        }
        None => {
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
            Ok(())
        }
    }
}

/// Split outputs: single file goes to `--output-file`; multiple go to
/// `--output-dir` as `<stem>-part-N.pdf`.
fn write_split_outputs(
    input: &Path,
    outputs: &[Vec<u8>],
    output_dir: Option<&Path>,
    output_file: Option<&Path>,
) -> Result<(), CliError> {
    if outputs.len() == 1 {
        let path = output_file
            .map(Path::to_path_buf)
            .or_else(|| output_dir.map(|d| d.join(split_name(input, 1))))
            .ok_or_else(|| {
                CliError::Usage(
                    "split produced one file; pass --output-file or --output-dir".into(),
                )
            })?;
        return write_file(&path, &outputs[0]);
    }
    let dir = output_dir.ok_or_else(|| {
        CliError::Usage(format!(
            "split produced {} files; pass --output-dir",
            outputs.len()
        ))
    })?;
    fs::create_dir_all(dir)?;
    for (i, bytes) in outputs.iter().enumerate() {
        write_file(&dir.join(split_name(input, i + 1)), bytes)?;
    }
    Ok(())
}

fn split_name(input: &Path, n: usize) -> String {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "split".to_string());
    format!("{stem}-part-{n:03}.pdf")
}

/// `--password` semantics: a literal value is used as-is; `-` or an
/// absent value prompts interactively.
fn resolve_password(opt: Option<&str>, label: &str) -> Result<String, CliError> {
    match opt {
        Some("-") | None => prompt_password(label),
        Some(v) => Ok(v.to_string()),
    }
}

/// Optional credential: absent → `None`, `-` → prompt, value → `Some`.
fn optional_password(opt: Option<&str>, label: &str) -> Result<Option<String>, CliError> {
    match opt {
        None => Ok(None),
        Some("-") => prompt_password(label).map(Some),
        Some(v) => Ok(Some(v.to_string())),
    }
}

fn prompt_password(label: &str) -> Result<String, CliError> {
    Ok(rpassword::prompt_password(format!("{label}: "))?)
}

fn parse_position(s: &str) -> Result<MarkPosition, CliError> {
    match s.to_ascii_lowercase().as_str() {
        "center" => Ok(MarkPosition::Center),
        "top-left" => Ok(MarkPosition::TopLeft),
        "top-right" => Ok(MarkPosition::TopRight),
        "bottom-left" => Ok(MarkPosition::BottomLeft),
        "bottom-right" => Ok(MarkPosition::BottomRight),
        "top-center" => Ok(MarkPosition::TopCenter),
        "bottom-center" => Ok(MarkPosition::BottomCenter),
        other => Err(CliError::Usage(format!(
            "unknown position {other:?}; expected center|top-left|top-right|bottom-left|bottom-right|top-center|bottom-center"
        ))),
    }
}

fn parse_color(s: &str) -> Result<[u8; 3], CliError> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 3 {
        return Err(CliError::Usage("--color must be r,g,b (0-255 each)".into()));
    }
    let mut out = [0u8; 3];
    for (i, p) in parts.iter().enumerate() {
        out[i] = p
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|&n| n <= 255)
            .map(|n| n as u8)
            .ok_or_else(|| CliError::Usage(format!("--color component {p:?} is not 0-255")))?;
    }
    Ok(out)
}

fn parse_box(s: Option<&str>) -> Result<[f32; 4], CliError> {
    let s = s.ok_or_else(|| CliError::Usage("crop requires --bx0y0x1y1 x0,y0,x1,y1".into()))?;
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        return Err(CliError::Usage("--bx0y0x1y1 must be x0,y0,x1,y1".into()));
    }
    let mut out = [0f32; 4];
    for (i, p) in parts.iter().enumerate() {
        out[i] = p
            .trim()
            .parse::<f32>()
            .map_err(|_| CliError::Usage(format!("--bx0y0x1y1 component {p:?} is not a number")))?;
    }
    Ok(out)
}

fn resize_spec(
    scale: Option<f64>,
    width: Option<f64>,
    height: Option<f64>,
) -> Result<ResizeSpec, CliError> {
    match (scale, width, height) {
        (Some(s), None, None) => Ok(ResizeSpec::Scale(s)),
        (None, Some(w), Some(h)) => Ok(ResizeSpec::Size {
            width_pt: w,
            height_pt: h,
        }),
        _ => Err(CliError::Usage(
            "resize requires --scale, or both --width and --height".into(),
        )),
    }
}

fn parse_compare_mode(s: &str) -> Result<CompareMode, CliError> {
    match s.to_ascii_lowercase().as_str() {
        "text" => Ok(CompareMode::Text),
        "structure" => Ok(CompareMode::Structure),
        "bytes" => Ok(CompareMode::Bytes),
        other => Err(CliError::Usage(format!(
            "unknown compare mode {other:?}; expected text|structure|bytes"
        ))),
    }
}

fn parse_render_format(s: &str) -> Result<RenderFormat, CliError> {
    match s.to_ascii_lowercase().as_str() {
        "png" => Ok(RenderFormat::Png),
        "jpeg" | "jpg" => Ok(RenderFormat::Jpeg),
        other => Err(CliError::Usage(format!(
            "unknown format {other:?}; expected png|jpeg"
        ))),
    }
}

fn parse_convert_target(s: &str) -> Result<ConvertTarget, CliError> {
    match s.to_ascii_lowercase().as_str() {
        "text" => Ok(ConvertTarget::Text),
        "markdown" | "md" => Ok(ConvertTarget::Markdown),
        other => Err(CliError::Usage(format!(
            "unknown target {other:?}; expected text|markdown"
        ))),
    }
}

/// Reports are JSON-first: structured formats get the typed value;
/// table mode falls back to pretty JSON (these are data reports, not
/// human tables).
fn emit_report<T: Serialize>(ctx: &Context, report: &T) -> Result<(), CliError> {
    if ctx.emit_structured(report)? {
        return Ok(());
    }
    println!("{}", serde_json::to_string_pretty(report)?);
    Ok(())
}

/// Format `Vec<PageText>` as plain text: pages separated by a blank
/// line, each headed by a `--- Page N ---` marker only when the
/// document has more than one page.
fn format_page_text(pages: &[crate::crates::pdf_engine::PageText]) -> String {
    let multi = pages.len() > 1;
    let mut out = String::new();
    for p in pages {
        if multi {
            out.push_str(&format!("--- Page {} ---\n", p.page));
        }
        out.push_str(&p.text);
        out.push_str("\n\n");
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out.push('\n');
    out
}
