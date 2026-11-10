//! `kaladoc doc list|get|create|upload|download|process|rename|move|delete|restore|purge|folders …`
//!
//! Thin adapter over [`AppServices`]/`DocumentService`: every command
//! opens the configured database through `AppServices::init` (strict
//! config — required secrets must be present), calls the document
//! service, and closes the pool before returning. `upload` streams a
//! local file through [`DocumentIngestionService::ingest`]; `download`
//! streams artifact bytes through [`StorageService::open_reader`];
//! `process` enqueues a `document.pdf_op` job for a worker.

use std::io::Write;
use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

use crate::crates::application::Organization::init_db;
use crate::crates::application::job_handlers::{self, PDF_OPS};
use crate::crates::application::services::app::AppServices;
use crate::crates::application::services::documents::{DocumentService, NewDocument};
use crate::crates::application::services::error::ServiceError;
use crate::crates::application::services::ingestion::IngestRequest;
use crate::crates::application::services::storage::StorageService;
use crate::crates::document_engine::artifact::DocumentArtifact;
use crate::crates::document_engine::document::{
    Document, DocumentOrigin, DocumentSource, DocumentType,
};
use crate::crates::document_engine::ids::{DocumentId, FolderId};
use crate::crates::document_engine::organization::Folder;
use crate::crates::document_engine::store::StoreError;
use crate::crates::document_engine::version::DocumentVersion;

use super::super::args::{DocArgs, FolderArgs, MetadataArgs};
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
        DocArgs::Upload {
            file,
            owner,
            name,
            folder,
            tags,
            mime_type,
        } => {
            upload(
                ctx,
                file,
                owner,
                name.as_deref(),
                folder.as_deref(),
                tags,
                mime_type.as_deref(),
            )
            .await
        }
        DocArgs::Download {
            id,
            artifact,
            output_file,
        } => download(ctx, id, artifact, output_file.as_ref()).await,
        DocArgs::Process {
            id,
            op,
            params,
            artifact,
            list_ops,
        } => {
            process(
                ctx,
                id.as_deref(),
                op.as_deref(),
                params.as_deref(),
                artifact.as_deref(),
                *list_ops,
            )
            .await
        }
        DocArgs::Rename { id, name } => rename(ctx, id, name).await,
        DocArgs::Move { id, folder } => move_doc(ctx, id, folder).await,
        DocArgs::Delete { id } => trash(ctx, id).await,
        DocArgs::Restore { id } => restore(ctx, id).await,
        DocArgs::Purge { id } => purge(ctx, id).await,
        DocArgs::Folders(f) => folders(ctx, f).await,
        DocArgs::Metadata(m) => metadata(ctx, m).await,
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

// ---------------------------------------------------------------------------
// metadata (alias: settings)
// ---------------------------------------------------------------------------

async fn metadata(ctx: &Context, args: &MetadataArgs) -> Result<i32, CliError> {
    match args {
        MetadataArgs::Get { id } => metadata_get(ctx, id).await,
        MetadataArgs::Set { id, key, value } => metadata_set(ctx, id, key, value).await,
        MetadataArgs::Unset { id, key } => metadata_unset(ctx, id, key).await,
        MetadataArgs::Delete { id } => metadata_delete(ctx, id).await,
    }
}

async fn metadata_get(ctx: &Context, id: &str) -> Result<i32, CliError> {
    let services = services(ctx).await?;
    let docs = services.documents();
    let id = doc_id(id);

    // Ensure the document exists so a typo'd id is a not-found, not a
    // silent "no metadata".
    if docs.get_document(&id).await.map_err(svc_err)?.is_none() {
        services.pool().close().await;
        return Err(CliError::NotFound(format!("document {id}")));
    }
    let meta = docs.get_document_metadata(&id).await.map_err(svc_err)?;
    services.pool().close().await;

    if ctx.emit_structured(&meta)? {
        return Ok(0);
    }
    match &meta {
        None => println!("(no metadata for document {id})"),
        Some(m) => {
            println!("document:   {}", m.document_id.as_str());
            println!("title:      {}", m.title.as_deref().unwrap_or(""));
            println!("author:     {}", m.author.as_deref().unwrap_or(""));
            println!("subject:    {}", m.subject.as_deref().unwrap_or(""));
            println!("language:   {}", m.language.as_deref().unwrap_or(""));
            println!("keywords:   {}", m.keywords.join(", "));
            if m.custom_fields.is_empty() {
                println!("custom:     (none)");
            } else {
                println!("custom fields:");
                for (k, v) in &m.custom_fields {
                    println!("  {k}: {}", serde_json::to_string(v).unwrap_or_default());
                }
            }
        }
    }
    Ok(0)
}

async fn metadata_set(ctx: &Context, id: &str, key: &str, value: &str) -> Result<i32, CliError> {
    if key.trim().is_empty() {
        return Err(CliError::Usage("--key must not be empty".to_string()));
    }
    // JSON when it parses; otherwise store the raw text as a string.
    let value: serde_json::Value = serde_json::from_str(value)
        .unwrap_or_else(|_| serde_json::Value::String(value.to_string()));

    let services = services(ctx).await?;
    let docs = services.documents();
    let id = doc_id(id);

    let meta = docs
        .set_custom_field(&id, key, &value)
        .await
        .map_err(svc_err)?;
    services.pool().close().await;

    if ctx.emit_structured(&meta)? {
        return Ok(0);
    }
    println!("set custom field {key:?} on document {}", id.as_str());
    Ok(0)
}

async fn metadata_unset(ctx: &Context, id: &str, key: &str) -> Result<i32, CliError> {
    let services = services(ctx).await?;
    let removed = services
        .documents()
        .remove_custom_field(&doc_id(id), key)
        .await
        .map_err(svc_err)?;
    services.pool().close().await;

    if ctx.emit_structured(&removed)? {
        return Ok(0);
    }
    if removed {
        println!("removed custom field {key:?} from document {id}");
    } else {
        println!("no custom field {key:?} on document {id}");
    }
    Ok(0)
}

async fn metadata_delete(ctx: &Context, id: &str) -> Result<i32, CliError> {
    let services = services(ctx).await?;
    let deleted = services
        .documents()
        .delete_document_metadata(&doc_id(id))
        .await
        .map_err(svc_err)?;
    services.pool().close().await;

    if ctx.emit_structured(&deleted)? {
        return Ok(0);
    }
    if deleted {
        println!("deleted metadata for document {id}");
    } else {
        println!("document {id} had no metadata row");
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// upload
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct UploadReport {
    document: Document,
    artifact: DocumentArtifact,
    storage_uri: String,
    size_bytes: u64,
    sha256: String,
    detected_mime_type: String,
}

async fn upload(
    ctx: &Context,
    file: &PathBuf,
    owner: &str,
    name: Option<&str>,
    folder: Option<&str>,
    tags: &[String],
    mime_type: Option<&str>,
) -> Result<i32, CliError> {
    let meta = tokio::fs::metadata(file).await.map_err(|e| {
        CliError::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", file.display()),
        ))
    })?;
    if !meta.is_file() {
        return Err(CliError::Usage(format!(
            "{}: not a regular file",
            file.display()
        )));
    }

    // `--name` becomes the document name *and* the stored file name
    // (ingest derives both from `file_name`); default to the input
    // file's own name.
    let file_name = name
        .map(str::to_string)
        .or_else(|| file.file_name().map(|n| n.to_string_lossy().into_owned()))
        .ok_or_else(|| {
            CliError::Usage(format!("{}: cannot determine file name", file.display()))
        })?;

    // `--tags a,b --tags c` — comma-splitting keeps both spellings
    // working.
    let tags: Vec<String> = tags
        .iter()
        .flat_map(|t| t.split(',').map(str::trim).map(str::to_string))
        .filter(|t| !t.is_empty())
        .collect();

    let mut request = IngestRequest::new(owner, file_name);
    request.mime_type = mime_type.map(str::to_string);
    request.folder_id = folder.map(folder_id);
    request.tags = tags;

    let handle = tokio::fs::File::open(file).await.map_err(|e| {
        CliError::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", file.display()),
        ))
    })?;

    let services = services(ctx).await?;
    let outcome = services
        .ingestion()
        .ingest(handle, request)
        .await
        .map_err(svc_err)?;
    services.pool().close().await;

    let report = UploadReport {
        document: outcome.document,
        artifact: outcome.artifact,
        storage_uri: outcome.storage_uri,
        size_bytes: outcome.size_bytes,
        sha256: outcome.sha256,
        detected_mime_type: outcome.detected_mime_type,
    };

    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    println!(
        "uploaded {} as document {} ({} bytes, {})",
        file.display(),
        report.document.id.as_str(),
        report.size_bytes,
        report.detected_mime_type
    );
    Ok(0)
}

// ---------------------------------------------------------------------------
// download
// ---------------------------------------------------------------------------

/// Resolves the artifact to download: `current` picks the current
/// version's artifact (falling back to the newest artifact when no
/// version points at one); anything else is treated as an artifact
/// id and must belong to `id`.
async fn resolve_artifact(
    docs: &DocumentService,
    document_id: &DocumentId,
    artifact: &str,
) -> Result<DocumentArtifact, CliError> {
    if artifact.eq_ignore_ascii_case("current") {
        let versions = docs.list_versions(document_id).await.map_err(svc_err)?;
        if let Some(v) = versions.iter().find(|v| v.is_current)
            && let Some(aid) = &v.artifact_id
            && let Some(a) = docs.download_artifact(aid).await.map_err(svc_err)?
        {
            return Ok(a);
        }
        // Fallback: the newest artifact (list is oldest-first).
        let artifacts = docs.list_artifacts(document_id).await.map_err(svc_err)?;
        return artifacts
            .into_iter()
            .next_back()
            .ok_or_else(|| CliError::NotFound(format!("artifact for document {document_id}")));
    }

    let a = docs
        .download_artifact(artifact)
        .await
        .map_err(svc_err)?
        .ok_or_else(|| CliError::NotFound(format!("artifact {artifact}")))?;
    if a.document_id != *document_id {
        return Err(CliError::NotFound(format!("artifact {artifact}")));
    }
    Ok(a)
}

async fn download(
    ctx: &Context,
    id: &str,
    artifact: &str,
    output_file: Option<&PathBuf>,
) -> Result<i32, CliError> {
    let services = services(ctx).await?;
    let document_id = doc_id(id);

    // Verify the document exists so a typo'd id is a not-found.
    if services
        .documents()
        .get_document(&document_id)
        .await
        .map_err(svc_err)?
        .is_none()
    {
        services.pool().close().await;
        return Err(CliError::NotFound(format!("document {id}")));
    }

    let artifact_row = resolve_artifact(services.documents(), &document_id, artifact).await?;
    let object_name = artifact_row
        .storage_uri
        .strip_prefix("local://")
        .filter(|n| !n.is_empty())
        .ok_or_else(|| {
            CliError::Unsupported(format!(
                "artifact {} has unsupported storage_uri scheme",
                artifact_row.id
            ))
        })?
        .to_string();

    // Stream the object on the blocking pool: open_reader + copy are
    // synchronous filesystem work. A dedicated StorageService is moved
    // into the closure so the pool stays usable for close() after.
    let pool = services.pool().clone();
    let storage_config = ctx.config()?.to_storage_config();
    drop(services);

    let destination = output_file.cloned();
    let name_for_error = object_name.clone();
    let bytes = tokio::task::spawn_blocking(move || -> Result<u64, CliError> {
        let mut storage = StorageService::from_config(&storage_config).map_err(svc_err)?;
        let mut reader = storage.open_reader(&name_for_error).map_err(svc_err)?;
        match destination {
            Some(path) => {
                if let Some(parent) = path.parent()
                    && !parent.as_os_str().is_empty()
                {
                    std::fs::create_dir_all(parent)?;
                }
                let mut out = std::fs::File::create(&path)?;
                let n = std::io::copy(&mut reader, &mut out)?;
                Ok(n)
            }
            None => {
                let stdout = std::io::stdout();
                let mut lock = stdout.lock();
                let n = std::io::copy(&mut reader, &mut lock)?;
                lock.flush()?;
                Ok(n)
            }
        }
    })
    .await
    .map_err(|e| CliError::Internal(format!("download worker task failed: {e}")))??;

    pool.close().await;

    if let Some(path) = output_file {
        #[derive(Serialize)]
        struct DownloadReport {
            document_id: String,
            artifact_id: String,
            file_name: String,
            bytes: u64,
            output_file: String,
        }
        let report = DownloadReport {
            document_id: id.to_string(),
            artifact_id: artifact_row.id.clone(),
            file_name: artifact_row.file_name.clone(),
            bytes,
            output_file: path.display().to_string(),
        };
        if ctx.emit_structured(&report)? {
            return Ok(0);
        }
        println!(
            "downloaded artifact {} ({} bytes) -> {}",
            report.artifact_id, report.bytes, report.output_file
        );
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// process (document.pdf_op job)
// ---------------------------------------------------------------------------

async fn process(
    ctx: &Context,
    id: Option<&str>,
    op: Option<&str>,
    params: Option<&str>,
    artifact: Option<&str>,
    list_ops: bool,
) -> Result<i32, CliError> {
    if list_ops {
        let ops: Vec<&str> = PDF_OPS.to_vec();
        if ctx.emit_structured(&ops)? {
            return Ok(0);
        }
        for op in ops {
            println!("{op}");
        }
        return Ok(0);
    }

    let (Some(id), Some(op)) = (id, op) else {
        return Err(CliError::Usage(
            "doc process requires a document id and --op (or --list-ops)".to_string(),
        ));
    };
    let params: Value = match params {
        Some(p) => serde_json::from_str(p)?,
        None => serde_json::json!({}),
    };

    let db = ctx.db_config()?;
    let pool = init_db::init_db_with(&db).await?;
    let document_id = doc_id(id);

    // Verify the document exists so a typo'd id fails now instead of
    // surfacing later as a handler error.
    let exists = DocumentService::new(pool.clone())
        .get_document(&document_id)
        .await
        .map_err(svc_err)?
        .is_some();
    if !exists {
        pool.close().await;
        return Err(CliError::NotFound(format!("document {id}")));
    }

    let job = job_handlers::enqueue_pdf_op(&pool, &document_id, artifact, op, params).await?;
    pool.close().await;

    #[derive(Serialize)]
    struct ProcessReport {
        job_id: i64,
        kind: String,
        document_id: String,
        op: String,
    }
    let report = ProcessReport {
        job_id: job.id,
        kind: job.kind.clone(),
        document_id: id.to_string(),
        op: op.to_string(),
    };

    if ctx.emit_structured(&report)? {
        return Ok(0);
    }
    println!(
        "enqueued {} job {} for document {} (op {})",
        report.kind, report.job_id, report.document_id, report.op
    );
    Ok(0)
}
