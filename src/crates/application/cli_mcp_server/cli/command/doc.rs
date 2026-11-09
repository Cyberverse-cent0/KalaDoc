//! `kaladoc doc list|get|create|rename|move|delete|restore|purge|folders …`
//!
//! Thin adapter over [`AppServices`]/`DocumentService`: every command
//! opens the configured database through `AppServices::init` (strict
//! config — required secrets must be present), calls the document
//! service, and closes the pool before returning. Upload/download of
//! artifact bytes is intentionally not exposed yet — that needs
//! multipart transfer and belongs to a follow-up.

use serde::Serialize;

use crate::crates::application::services::app::AppServices;
use crate::crates::application::services::documents::NewDocument;
use crate::crates::application::services::error::ServiceError;
use crate::crates::document_engine::artifact::DocumentArtifact;
use crate::crates::document_engine::document::{
    Document, DocumentOrigin, DocumentSource, DocumentType,
};
use crate::crates::document_engine::ids::{DocumentId, FolderId};
use crate::crates::document_engine::organization::Folder;
use crate::crates::document_engine::store::StoreError;
use crate::crates::document_engine::version::DocumentVersion;

use super::super::args::{DocArgs, FolderArgs};
use super::{CliError, Context};

pub async fn run(ctx: &Context, args: &DocArgs) -> Result<i32, CliError> {
    match args {
        DocArgs::List {
            owner,
            folder,
            all,
            limit,
        } => list(ctx, owner.as_deref(), folder.as_deref(), *all, *limit).await,
        DocArgs::Get { id } => get(ctx, id).await,
        DocArgs::Create {
            name,
            owner,
            folder,
            document_type,
        } => {
            create(
                ctx,
                name,
                owner,
                folder.as_deref(),
                document_type.as_deref(),
            )
            .await
        }
        DocArgs::Rename { id, name } => rename(ctx, id, name).await,
        DocArgs::Move { id, folder } => move_doc(ctx, id, folder).await,
        DocArgs::Delete { id } => trash(ctx, id).await,
        DocArgs::Restore { id } => restore(ctx, id).await,
        DocArgs::Purge { id } => purge(ctx, id).await,
        DocArgs::Folders(f) => folders(ctx, f).await,
    }
}

// ---------------------------------------------------------------------------
// Service plumbing
// ---------------------------------------------------------------------------

/// Opens the application services from the strict config (required
/// secrets must be present — same rule as the `jobs`/`user` commands).
async fn services(ctx: &Context) -> Result<AppServices, CliError> {
    let config = ctx.config()?;
    AppServices::init(&config).await.map_err(svc_err)
}

/// Maps [`ServiceError`] onto the CLI's stable error contract without
/// adding a `CliError` variant: store "not found"/"validation" keep
/// their dedicated exit codes, everything else is internal/io/config/db.
fn svc_err(e: ServiceError) -> CliError {
    match e {
        ServiceError::Store(StoreError::NotFound { entity, id }) => {
            CliError::NotFound(format!("{entity} {id}"))
        }
        ServiceError::Store(StoreError::Validation { field, reason }) => {
            CliError::Validation(format!("{field}: {reason}"))
        }
        ServiceError::Store(StoreError::Document(d)) => CliError::Validation(d.to_string()),
        ServiceError::Store(StoreError::Serde(s)) => CliError::Json(s),
        ServiceError::Store(StoreError::Sqlx(s)) => {
            CliError::Internal(format!("database error: {s}"))
        }
        ServiceError::Config(c) => CliError::Config(c),
        ServiceError::Db(d) => CliError::Db(d),
        ServiceError::Io(i) => CliError::Io(i),
        ServiceError::Storage(s) => CliError::Internal(s.to_string()),
    }
}

fn doc_id(id: &str) -> DocumentId {
    DocumentId::new(id)
}

fn folder_id(id: &str) -> FolderId {
    FolderId::new(id)
}

/// Renders a `snake_case` serde enum as its stable string form — the
/// same spelling used in JSON output, for table cells.
fn snake<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

/// Resolves the document type: `--type` when given, otherwise inferred
/// from the file extension of `name`.
fn resolve_document_type(name: &str, explicit: Option<&str>) -> Result<DocumentType, CliError> {
    if let Some(t) = explicit {
        return match t {
            "pdf" => Ok(DocumentType::Pdf),
            "image" => Ok(DocumentType::Image),
            "spreadsheet" => Ok(DocumentType::Spreadsheet),
            "presentation" => Ok(DocumentType::Presentation),
            "text" => Ok(DocumentType::Text),
            "archive" => Ok(DocumentType::Archive),
            "other" => Ok(DocumentType::Other),
            other => Err(CliError::Usage(format!(
                "invalid --type {other:?}; expected pdf, image, spreadsheet, \
                 presentation, text, archive, or other"
            ))),
        };
    }
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    Ok(match ext.as_str() {
        "pdf" => DocumentType::Pdf,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tiff" | "svg" => DocumentType::Image,
        "xls" | "xlsx" | "ods" | "csv" | "tsv" => DocumentType::Spreadsheet,
        "ppt" | "pptx" | "odp" | "key" => DocumentType::Presentation,
        "txt" | "md" | "markdown" | "log" | "rtf" => DocumentType::Text,
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" => DocumentType::Archive,
        _ => DocumentType::Other,
    })
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

async fn list(
    ctx: &Context,
    owner: Option<&str>,
    folder: Option<&str>,
    all: bool,
    limit: i64,
) -> Result<i32, CliError> {
    if limit <= 0 {
        return Err(CliError::Usage("--limit must be positive".to_string()));
    }
    let folder = folder.map(folder_id);

    let services = services(ctx).await?;
    let docs = services
        .documents()
        .list_documents(owner, folder.as_ref(), all, limit)
        .await
        .map_err(svc_err)?;
    services.pool().close().await;

    if ctx.emit_structured(&docs)? {
        return Ok(0);
    }
    println!(
        "{:<26} {:<28} {:<12} {:<9} {:<20} {:<20} updated_at",
        "id", "name", "type", "status", "owner", "folder"
    );
    for d in &docs {
        println!(
            "{:<26} {:<28} {:<12} {:<9} {:<20} {:<20} {}",
            d.id.as_str(),
            d.name.as_str(),
            snake(&d.document_type),
            snake(&d.status),
            d.owner_id,
            d.folder_id.as_ref().map(|f| f.as_str()).unwrap_or(""),
            d.updated_at
        );
    }
    if docs.is_empty() {
        println!("(no documents)");
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// get
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct DocumentDetail {
    document: Document,
    artifacts: Vec<DocumentArtifact>,
    versions: Vec<DocumentVersion>,
}

async fn get(ctx: &Context, id: &str) -> Result<i32, CliError> {
    let services = services(ctx).await?;
    let docs = services.documents();
    let id = doc_id(id);

    let document = docs
        .get_document(&id)
        .await
        .map_err(svc_err)?
        .ok_or_else(|| CliError::NotFound(format!("document {id}")))?;
    let artifacts = docs.list_artifacts(&id).await.map_err(svc_err)?;
    let versions = docs.list_versions(&id).await.map_err(svc_err)?;
    services.pool().close().await;

    let detail = DocumentDetail {
        document,
        artifacts,
        versions,
    };

    if ctx.emit_structured(&detail)? {
        return Ok(0);
    }

    let d = &detail.document;
    println!("id:          {}", d.id.as_str());
    println!("name:        {}", d.name.as_str());
    println!("type:        {}", snake(&d.document_type));
    println!("status:      {}", snake(&d.status));
    println!("owner:       {}", d.owner_id);
    println!(
        "folder:      {}",
        d.folder_id.as_ref().map(|f| f.as_str()).unwrap_or("(root)")
    );
    println!("origin:      {}", snake(&d.source.origin));
    println!("created_at:  {}", d.created_at);
    println!("updated_at:  {}", d.updated_at);

    println!("\nartifacts ({}):", detail.artifacts.len());
    if detail.artifacts.is_empty() {
        println!("  (none)");
    }
    for a in &detail.artifacts {
        println!(
            "  {}  {:<12} {:<40} {} bytes",
            a.id,
            a.mime_type.as_deref().unwrap_or("-"),
            a.file_name,
            a.size_bytes
        );
    }

    println!("\nversions ({}):", detail.versions.len());
    if detail.versions.is_empty() {
        println!("  (none)");
    }
    for v in &detail.versions {
        println!(
            "  v{:<4} {:<10} {} {}",
            v.version_number,
            if v.is_current { "current" } else { "" },
            v.created_at,
            v.note.as_deref().unwrap_or("")
        );
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// create
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct CreateReport {
    document: Document,
}

async fn create(
    ctx: &Context,
    name: &str,
    owner: &str,
    folder: Option<&str>,
    document_type: Option<&str>,
) -> Result<i32, CliError> {
    let dtype = resolve_document_type(name, document_type)?;
    let folder = folder.map(folder_id);

    let services = services(ctx).await?;
    let docs = services.documents();

    let new = NewDocument {
        name: name.to_string(),
        document_type: dtype,
        owner_id: owner.to_string(),
        source: DocumentSource::new(DocumentOrigin::Upload),
    };
    let document = docs.create_document(&new).await.map_err(svc_err)?;

    // NewDocument carries no folder; place it afterwards when requested.
    let document = match &folder {
        Some(f) => docs
            .move_document(&document.id, Some(f))
            .await
            .map_err(svc_err)?,
        None => document,
    };
    services.pool().close().await;

    let report = CreateReport { document };
    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    println!(
        "created document {} ({})",
        report.document.id.as_str(),
        report.document.name.as_str()
    );
    Ok(0)
}

// ---------------------------------------------------------------------------
// rename / move / delete / restore / purge
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct DocumentActionReport {
    document: Document,
}

async fn rename(ctx: &Context, id: &str, name: &str) -> Result<i32, CliError> {
    let services = services(ctx).await?;
    let document = services
        .documents()
        .rename_document(&doc_id(id), name)
        .await
        .map_err(svc_err)?;
    services.pool().close().await;

    let report = DocumentActionReport { document };
    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    println!(
        "renamed document {} to {}",
        report.document.id.as_str(),
        report.document.name.as_str()
    );
    Ok(0)
}

async fn move_doc(ctx: &Context, id: &str, folder: &str) -> Result<i32, CliError> {
    // `none` (any case) means the document root.
    let folder = if folder.eq_ignore_ascii_case("none") {
        None
    } else {
        Some(folder_id(folder))
    };

    let services = services(ctx).await?;
    let document = services
        .documents()
        .move_document(&doc_id(id), folder.as_ref())
        .await
        .map_err(svc_err)?;
    services.pool().close().await;

    let report = DocumentActionReport { document };
    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    println!(
        "moved document {} to {}",
        report.document.id.as_str(),
        report
            .document
            .folder_id
            .as_ref()
            .map(|f| f.as_str())
            .unwrap_or("(root)")
    );
    Ok(0)
}

async fn trash(ctx: &Context, id: &str) -> Result<i32, CliError> {
    let services = services(ctx).await?;
    let document = services
        .documents()
        .delete_document(&doc_id(id))
        .await
        .map_err(svc_err)?;
    services.pool().close().await;

    let report = DocumentActionReport { document };
    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    println!(
        "trashed document {} (restore with `kaladoc doc restore`)",
        report.document.id.as_str()
    );
    Ok(0)
}

async fn restore(ctx: &Context, id: &str) -> Result<i32, CliError> {
    let services = services(ctx).await?;
    let document = services
        .documents()
        .restore_document(&doc_id(id))
        .await
        .map_err(svc_err)?;
    services.pool().close().await;

    let report = DocumentActionReport { document };
    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    println!("restored document {}", report.document.id.as_str());
    Ok(0)
}

#[derive(Debug, Serialize)]
struct PurgeReport {
    id: String,
    purged: bool,
}

async fn purge(ctx: &Context, id: &str) -> Result<i32, CliError> {
    let services = services(ctx).await?;
    let purged = services
        .documents()
        .purge_document(&doc_id(id))
        .await
        .map_err(svc_err)?;
    services.pool().close().await;

    if !purged {
        return Err(CliError::NotFound(format!("document {id}")));
    }
    let report = PurgeReport {
        id: id.to_string(),
        purged,
    };
    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    println!("permanently deleted document {id}");
    Ok(0)
}

// ---------------------------------------------------------------------------
// folders
// ---------------------------------------------------------------------------

async fn folders(ctx: &Context, args: &FolderArgs) -> Result<i32, CliError> {
    match args {
        FolderArgs::List { owner, parent } => {
            let parent = parent.as_deref().map(folder_id);
            let services = services(ctx).await?;
            let folders = services
                .documents()
                .list_folders(owner.as_deref(), parent.as_ref())
                .await
                .map_err(svc_err)?;
            services.pool().close().await;

            if ctx.emit_structured(&folders)? {
                return Ok(0);
            }
            println!(
                "{:<26} {:<30} {:<26} {:<20} updated_at",
                "id", "name", "parent", "owner"
            );
            for f in &folders {
                println!(
                    "{:<26} {:<30} {:<26} {:<20} {}",
                    f.id.as_str(),
                    f.name,
                    f.parent_id.as_ref().map(|p| p.as_str()).unwrap_or(""),
                    f.owner_id,
                    f.updated_at
                );
            }
            if folders.is_empty() {
                println!("(no folders)");
            }
            Ok(0)
        }
        FolderArgs::Create {
            name,
            owner,
            parent,
        } => {
            let parent = parent.as_deref().map(folder_id);
            let services = services(ctx).await?;
            let folder = services
                .documents()
                .create_folder(name, parent.as_ref(), owner)
                .await
                .map_err(svc_err)?;
            services.pool().close().await;

            #[derive(Serialize)]
            struct FolderReport {
                folder: Folder,
            }
            let report = FolderReport { folder };
            if ctx.emit_structured(&report)? {
                return Ok(0);
            }
            println!(
                "created folder {} ({})",
                report.folder.id.as_str(),
                report.folder.name
            );
            Ok(0)
        }
    }
}
