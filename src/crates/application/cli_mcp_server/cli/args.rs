//! Clap command tree. Only commands backed by real services exist —
//! per spec, no placeholders or fake success responses.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use super::global::GlobalOpts;

#[derive(Parser)]
#[command(
    name = "kaladoc",
    version,
    about = "KalaDoc document platform CLI",
    long_about = "Command-line interface for the KalaDoc document platform. \
                  Runs against the local application database; remote \
                  server mode is not implemented yet."
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalOpts,

    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Initialize the database and exit. Hidden legacy alias for
    /// `kaladoc init`.
    #[arg(long, hide = true)]
    pub init_db: bool,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Print version and build information.
    Version,

    /// Check configuration, database, storage, and environment health.
    Doctor,

    /// Create or migrate the local database.
    Init,

    /// Inspect and validate configuration.
    #[command(subcommand)]
    Config(ConfigArgs),

    /// Manage authentication sessions.
    #[command(subcommand)]
    Auth(AuthArgs),

    /// Manage user accounts.
    #[command(subcommand)]
    User(UserArgs),

    /// Manage the durable job queue: run a worker, list/enqueue jobs,
    /// cancel, retry, and recover interrupted work.
    #[command(subcommand)]
    Jobs(JobsArgs),

    /// Inspect processing engines: list capabilities and health.
    #[command(subcommand)]
    Engine(EngineArgs),

    /// Manage documents in the local document store.
    #[command(subcommand)]
    Doc(DocArgs),

    /// Process PDF files on local disk with the pdf engine.
    #[command(subcommand)]
    Pdf(PdfArgs),

    /// Process image files on local disk with the image engine.
    #[command(subcommand)]
    Image(ImageArgs),

    /// List built-in pipeline definitions and run them on files.
    #[command(subcommand)]
    Pipeline(PipelineArgs),

    /// Inspect and manage local storage: backend status, streaming
    /// upload limits, and abandoned-upload cleanup.
    #[command(subcommand)]
    Storage(StorageArgs),

    /// Manage recurring job schedules (fixed-interval, not cron).
    #[command(subcommand)]
    Schedule(ScheduleArgs),

    /// Run multimedia-engine ops (ffmpeg/ffprobe) on local media
    /// files: probe, transcode, trim, thumbnails, subtitles, and more.
    #[command(subcommand)]
    Media(MediaArgs),

    /// Convert a local file between document formats via the
    /// conversion engine (pdf, office formats, markup, images).
    Convert {
        /// Input file path.
        input: PathBuf,
        /// Source format: pdf, txt/text, markdown/md, html, png,
        /// jpeg/jpg, docx, odt, xlsx, xls, pptx, ppt, epub.
        #[arg(long)]
        from: String,
        /// Target format (same names as `--from`).
        #[arg(long)]
        to: String,
        /// JSON options object: `dpi`, `quality`, `page_size`,
        /// `params` (backend-specific escape hatch).
        #[arg(long)]
        options: Option<String>,
        /// Output file path (default: raw bytes to stdout).
        #[arg(long)]
        output_file: Option<PathBuf>,
    },

    /// Start the HTTP API server (binds `api.addr` unless overridden).
    /// `--worker` also runs the durable job worker in-process.
    Serve {
        /// Bind address override, e.g. `0.0.0.0:8080`.
        #[arg(long)]
        addr: Option<String>,
        /// Run the job worker loop alongside the HTTP server.
        #[arg(long, alias = "embedded-worker")]
        worker: bool,
    },

    /// Run the MCP server that exposes application services to MCP
    /// clients (document tools, jobs, engines).
    #[command(subcommand)]
    Mcp(McpArgs),
}

/// `kaladoc mcp <op>` — MCP server transports. Only stdio is
/// implemented; remote transports are intentionally absent (spec §10:
/// no unauthenticated network listener).
#[derive(Subcommand)]
pub enum McpArgs {
    /// Serve MCP over standard input/output for local MCP clients.
    Serve {
        /// Transport to use. Only `stdio` is supported.
        #[arg(long, default_value = "stdio")]
        transport: String,
    },
}

#[derive(Subcommand)]
pub enum ConfigArgs {
    /// Show the resolved configuration (secrets are redacted).
    Show,

    /// Validate configuration and required secrets are present.
    Validate,

    /// Print a single configuration value by dotted key
    /// (e.g. `database.path`, `runtime.log_level`).
    Get {
        /// Dotted configuration key.
        key: String,
    },
}

#[derive(Subcommand)]
pub enum AuthArgs {
    /// Log in with a user name and password (prompted, never echoed).
    Login {
        /// User name; prompted interactively when omitted.
        #[arg(long)]
        username: Option<String>,
    },

    /// Log out: revoke the stored session and remove credentials.
    Logout,

    /// Report whether a valid session is stored.
    Status,

    /// Print the authenticated user's identity.
    Whoami,

    /// Manage the current user's sessions.
    #[command(subcommand)]
    Session(SessionArgs),
}

#[derive(Subcommand)]
pub enum UserArgs {
    /// Create a user account. The password is prompted without echo, or
    /// read from stdin with `--password-stdin`.
    Create {
        /// Login name (3–32 chars: letters, digits, `_`, `-`, `.`).
        #[arg(long)]
        username: String,
        /// Email address (unique).
        #[arg(long)]
        email: String,
        #[arg(long)]
        first_name: String,
        #[arg(long)]
        last_name: String,
        /// Read the password as a single line from stdin instead of
        /// prompting — for scripts. Prefer prompting interactively.
        #[arg(long)]
        password_stdin: bool,
    },

    /// List user accounts. Never includes password hashes.
    List,

    /// Delete a user account and their sessions.
    Delete {
        /// Login name of the user to delete.
        username: String,
    },
}

#[derive(Subcommand)]
pub enum JobsArgs {
    /// Run the job worker loop: claims due jobs from the queue and
    /// dispatches them to the registered handlers. Runs until the
    /// process is interrupted (Ctrl+C).
    Worker,

    /// List jobs in the queue, newest first.
    List {
        /// Filter by status: pending, running, succeeded, failed, cancelled.
        #[arg(long)]
        status: Option<String>,
        /// Maximum number of jobs to show.
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },

    /// Enqueue a job for a worker to pick up.
    Enqueue {
        /// Job kind — the handler registry key (e.g. `document.ingest`).
        #[arg(long)]
        kind: String,
        /// JSON payload handed to the handler (default: `{}`).
        #[arg(long)]
        payload: Option<String>,
        /// Scheduling priority; higher runs first among due jobs.
        #[arg(long, default_value_t = 0)]
        priority: i64,
        /// Unix seconds before which the job must not run (default: now).
        #[arg(long)]
        run_at: Option<i64>,
        /// Maximum attempts before the job is marked failed
        /// (default: the configured `jobs.default_max_attempts`).
        #[arg(long)]
        max_attempts: Option<u32>,
    },

    /// Cancel a pending or running job.
    Cancel {
        /// Job ID from `kaladoc jobs list`.
        id: i64,
    },

    /// Requeue a failed job for another attempt.
    Retry {
        /// Job ID from `kaladoc jobs list`.
        id: i64,
    },

    /// Reclaim jobs left in `running` state by a crashed worker.
    Recover,
}

#[derive(Subcommand)]
pub enum EngineArgs {
    /// List registered engines with their advertised operations and
    /// input/output formats.
    List,

    /// Report per-engine health status.
    Health,
}

#[derive(Subcommand)]
pub enum DocArgs {
    /// List documents, newest first.
    List {
        /// Only documents owned by this user/organization id.
        #[arg(long)]
        owner: Option<String>,
        /// Only documents in this folder.
        #[arg(long)]
        folder: Option<String>,
        /// Include trashed documents.
        #[arg(long)]
        all: bool,
        /// Maximum number of documents to show.
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },

    /// Show a document with its artifacts and versions.
    Get {
        /// Document ID from `kaladoc doc list`.
        id: String,
    },

    /// Register a new document (metadata only — no file bytes).
    Create {
        /// Document display name.
        #[arg(long)]
        name: String,
        /// Owning user/organization id.
        #[arg(long)]
        owner: String,
        /// Folder to place the document in.
        #[arg(long)]
        folder: Option<String>,
        /// Document type: pdf, image, spreadsheet, presentation, text,
        /// archive, or other. Inferred from the file extension when
        /// omitted.
        #[arg(long = "type")]
        document_type: Option<String>,
    },

    /// Rename a document.
    Rename {
        /// Document ID from `kaladoc doc list`.
        id: String,
        /// New display name.
        #[arg(long)]
        name: String,
    },

    /// Move a document into a folder (or back to the root).
    Move {
        /// Document ID from `kaladoc doc list`.
        id: String,
        /// Target folder id, or `none` for the root.
        #[arg(long)]
        folder: String,
    },

    /// Move a document to the trash (restorable via `doc restore`).
    Delete {
        /// Document ID from `kaladoc doc list`.
        id: String,
    },

    /// Restore a trashed document to active.
    Restore {
        /// Document ID from `kaladoc doc list`.
        id: String,
    },

    /// Permanently delete a trashed document and its dependents.
    Purge {
        /// Document ID from `kaladoc doc list`.
        id: String,
    },

    /// Upload a local file as a document: streams the bytes into
    /// storage and registers the artifact, version 1, and tags.
    Upload {
        /// Local file to upload.
        file: PathBuf,
        /// Owning user/organization id.
        #[arg(long)]
        owner: String,
        /// Document display name and stored file name
        /// (default: the input file's name).
        #[arg(long)]
        name: Option<String>,
        /// Folder to place the document in.
        #[arg(long)]
        folder: Option<String>,
        /// Tags to assign to the document; repeatable, comma-separated
        /// values also accepted.
        #[arg(long)]
        tags: Vec<String>,
        /// MIME type recorded on the artifact (the detected type from
        /// magic bytes is always stored in metadata).
        #[arg(long)]
        mime_type: Option<String>,
    },

    /// Download an artifact's stored bytes to stdout or a file.
    Download {
        /// Document ID from `kaladoc doc list`.
        id: String,
        /// Artifact ID from `kaladoc doc get`, or `current` for the
        /// current version's artifact (falls back to the newest
        /// artifact). Default: `current`.
        #[arg(long, default_value = "current")]
        artifact: String,
        /// Write bytes to this file instead of stdout.
        #[arg(long)]
        output_file: Option<PathBuf>,
    },

    /// Enqueue a pdf-engine operation (`document.pdf_op` job) on a
    /// stored document artifact; a worker picks it up.
    Process {
        /// Document ID from `kaladoc doc list`. Omit with `--list-ops`.
        id: Option<String>,
        /// Pdf op name — one of `--list-ops` (e.g. `page_count`,
        /// `extract_text`, `rotate`, `compress`).
        #[arg(long)]
        op: Option<String>,
        /// JSON params object forwarded to the op (default: `{}`).
        #[arg(long)]
        params: Option<String>,
        /// Artifact to operate on (default: most recent artifact).
        #[arg(long)]
        artifact: Option<String>,
        /// List the supported pdf ops and exit.
        #[arg(long)]
        list_ops: bool,
    },

    /// Manage document folders.
    #[command(subcommand)]
    Folders(FolderArgs),

    /// Read and update a document's metadata record: custom fields
    /// plus the whole-row view. Alias: `settings`.
    #[command(subcommand, alias = "settings")]
    Metadata(MetadataArgs),
}

#[derive(Subcommand)]
pub enum MetadataArgs {
    /// Show the stored metadata row for a document (title, author,
    /// subject, language, keywords, custom fields). Prints an empty
    /// record note when none exists yet.
    Get {
        /// Document ID from `kaladoc doc list`.
        id: String,
    },

    /// Set (or overwrite) a single custom metadata field. `--value`
    /// is parsed as JSON (`"text"`, `42`, `true`, `[1,2]`, `{"k":1}`);
    /// a bare word is stored as a string.
    Set {
        /// Document ID from `kaladoc doc list`.
        id: String,
        /// Custom field key (non-empty).
        #[arg(long)]
        key: String,
        /// JSON value for the field.
        #[arg(long)]
        value: String,
    },

    /// Remove a single custom metadata field.
    Unset {
        /// Document ID from `kaladoc doc list`.
        id: String,
        /// Custom field key to remove.
        #[arg(long)]
        key: String,
    },

    /// Delete the whole metadata row for a document. The document
    /// itself is not touched.
    Delete {
        /// Document ID from `kaladoc doc list`.
        id: String,
    },
}

#[derive(Subcommand)]
pub enum FolderArgs {
    /// List folders. Root-level folders unless `--parent` selects the
    /// children of a specific folder.
    List {
        /// Only folders owned by this user/organization id.
        #[arg(long)]
        owner: Option<String>,
        /// List only direct children of this folder.
        #[arg(long)]
        parent: Option<String>,
    },

    /// Create a folder, optionally nested under a parent.
    Create {
        /// Folder name.
        #[arg(long)]
        name: String,
        /// Owning user/organization id.
        #[arg(long)]
        owner: String,
        /// Parent folder id (omit for a root-level folder).
        #[arg(long)]
        parent: Option<String>,
    },
}

/// `kaladoc pdf <op>` — local-file PDF operations. Page numbers are
/// 1-based; `--pages` accepts the shared range grammar (`"1-3,8"`,
/// `"all"`). Passwords may be given as `--password -` to prompt
/// without echo.
#[derive(Subcommand)]
pub enum PdfArgs {
    /// Merge two or more PDFs into a single document, in input order.
    Merge {
        /// Input PDF files, in merge order. Repeatable.
        #[arg(long, required = true)]
        input: Vec<PathBuf>,
        /// Output PDF path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Split a PDF into multiple files.
    Split {
        /// Input PDF path.
        input: PathBuf,
        /// Pages per output file (fixed-size mode).
        #[arg(long, conflicts_with_all = ["ranges", "every"])]
        pages_per_file: Option<u32>,
        /// Comma-separated range spec; one output file per item
        /// (e.g. `"1-3,8,10-12"`).
        #[arg(long, conflicts_with_all = ["pages_per_file", "every"])]
        ranges: Option<String>,
        /// Strided split: output k gets pages k+1, k+1+n, ...
        #[arg(long, conflicts_with_all = ["pages_per_file", "ranges"])]
        every: Option<u32>,
        /// Directory for multiple outputs (required unless a single
        /// output file is produced).
        #[arg(long)]
        output_dir: Option<PathBuf>,
        /// Single output path; only valid when exactly one file is
        /// produced.
        #[arg(long)]
        output_file: Option<PathBuf>,
    },

    /// Rotate pages by 90/180/270 degrees.
    Rotate {
        /// Input PDF path.
        input: PathBuf,
        /// Page range spec (default: all pages).
        #[arg(long, default_value = "all")]
        pages: String,
        /// Rotation in degrees: 90, 180, or 270.
        #[arg(long)]
        degrees: u16,
        /// Output PDF path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Set an absolute crop box on selected pages.
    Crop {
        /// Input PDF path.
        input: PathBuf,
        /// Page range spec (default: all pages).
        #[arg(long, default_value = "all")]
        pages: String,
        /// Crop box as `x0,y0,x1,y1` in PDF points.
        #[arg(long)]
        bx0y0x1y1: Option<String>,
        /// Output PDF path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Resize page boxes by scale or explicit dimensions.
    Resize {
        /// Input PDF path.
        input: PathBuf,
        /// Page range spec (default: all pages).
        #[arg(long, default_value = "all")]
        pages: String,
        /// Uniform scale factor.
        #[arg(long)]
        scale: Option<f64>,
        /// Target width in points.
        #[arg(long)]
        width: Option<f64>,
        /// Target height in points.
        #[arg(long)]
        height: Option<f64>,
        /// Output PDF path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Reorder pages: `--order` is a comma-separated permutation of all
    /// page numbers, e.g. `"3,1,2"`.
    Reorder {
        /// Input PDF path.
        input: PathBuf,
        /// Full-page permutation, comma-separated.
        #[arg(long)]
        order: String,
        /// Output PDF path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Remove the pages selected by `--pages`.
    RemovePages {
        /// Input PDF path.
        input: PathBuf,
        /// Page range spec of pages to remove.
        #[arg(long)]
        pages: String,
        /// Output PDF path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Extract the pages selected by `--pages` into a new PDF.
    ExtractPages {
        /// Input PDF path.
        input: PathBuf,
        /// Page range spec, e.g. `"1-3,8,10-12"`.
        #[arg(long)]
        pages: String,
        /// Output PDF path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Recompress the PDF (object streams, xref streams).
    Compress {
        /// Input PDF path.
        input: PathBuf,
        /// Output PDF path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Repair a malformed-but-parseable PDF via normalized rewrite.
    /// Prints a repair report; writes the repaired PDF when
    /// `--output-file` is given.
    Repair {
        /// Input PDF path.
        input: PathBuf,
        /// Output PDF path for the repaired document.
        #[arg(long)]
        output_file: Option<PathBuf>,
    },

    /// Flatten supported annotation appearances into page content.
    Flatten {
        /// Input PDF path.
        input: PathBuf,
        /// Output PDF path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Remove dangerous document actions (JavaScript, embedded-file
    /// launch, rich media).
    Sanitize {
        /// Input PDF path.
        input: PathBuf,
        /// Output PDF path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Overlay another PDF's pages onto this document's pages.
    Overlay {
        /// Input PDF path.
        input: PathBuf,
        /// PDF whose pages are painted on top.
        #[arg(long)]
        overlay: PathBuf,
        /// Page range spec for the base document (default: all).
        #[arg(long, default_value = "all")]
        pages: String,
        /// Output PDF path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Add a text watermark.
    Watermark {
        /// Input PDF path.
        input: PathBuf,
        /// Watermark text.
        #[arg(long)]
        text: String,
        /// Position: center, top-left, top-right, bottom-left,
        /// bottom-right, top-center, bottom-center.
        #[arg(long, default_value = "center")]
        position: String,
        /// Font size in points (default: engine default).
        #[arg(long)]
        font_size: Option<f32>,
        /// Opacity 0.0-1.0.
        #[arg(long)]
        opacity: Option<f32>,
        /// Counter-clockwise rotation in degrees.
        #[arg(long)]
        rotation: Option<f32>,
        /// Page range spec (default: all pages).
        #[arg(long)]
        pages: Option<String>,
        /// RGB color as `r,g,b` (0-255 each).
        #[arg(long)]
        color: Option<String>,
        /// Output PDF path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Stamp text or a PNG image onto pages.
    Stamp {
        /// Input PDF path.
        input: PathBuf,
        /// Stamp text (mutually exclusive with `--image`).
        #[arg(long, conflicts_with = "image")]
        text: Option<String>,
        /// PNG image file to stamp (mutually exclusive with `--text`).
        #[arg(long, conflicts_with = "text")]
        image: Option<PathBuf>,
        /// Position: center, top-left, top-right, bottom-left,
        /// bottom-right, top-center, bottom-center.
        #[arg(long, default_value = "center")]
        position: String,
        /// Size multiplier (default: 1.0).
        #[arg(long)]
        scale: Option<f32>,
        /// Opacity 0.0-1.0 (default: 1.0).
        #[arg(long)]
        opacity: Option<f32>,
        /// Page range spec (default: all pages).
        #[arg(long)]
        pages: Option<String>,
        /// Output PDF path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Remove text content matching the given terms (best-effort —
    /// see `kaladoc pdf redact --help` engine docs for coverage).
    Redact {
        /// Input PDF path.
        input: PathBuf,
        /// Text term to remove; repeatable.
        #[arg(long, required = true)]
        term: Vec<String>,
        /// Page range spec (default: all pages).
        #[arg(long)]
        pages: Option<String>,
        /// Output PDF path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Encrypt a PDF (AES-256) with a user password.
    Encrypt {
        /// Input PDF path.
        input: PathBuf,
        /// User (open) password; `-` prompts without echo.
        #[arg(long)]
        password: Option<String>,
        /// Owner (permissions) password; `-` prompts. Defaults to the
        /// user password when omitted.
        #[arg(long)]
        owner_password: Option<String>,
        /// Deny printing.
        #[arg(long)]
        no_print: bool,
        /// Deny copying/extracting text.
        #[arg(long)]
        no_copy: bool,
        /// Deny modifications.
        #[arg(long)]
        no_modify: bool,
        /// Deny annotations and form filling.
        #[arg(long)]
        no_annotations: bool,
        /// Output PDF path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Decrypt a PDF with its user or owner password.
    Decrypt {
        /// Input PDF path.
        input: PathBuf,
        /// Password; `-` or omitted prompts without echo.
        #[arg(long)]
        password: Option<String>,
        /// Output PDF path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Digitally sign a PDF (real CMS signature via openssl).
    Sign {
        /// Input PDF path.
        input: PathBuf,
        /// PEM certificate (or chain) file.
        #[arg(long, requires = "key")]
        cert: Option<PathBuf>,
        /// PEM private key file.
        #[arg(long, requires = "cert")]
        key: Option<PathBuf>,
        /// Passphrase for an encrypted PEM key; `-` prompts.
        #[arg(long)]
        key_password: Option<String>,
        /// PKCS#12 (.p12/.pfx) bundle (alternative to --cert/--key).
        #[arg(long, conflicts_with_all = ["cert", "key"])]
        p12: Option<PathBuf>,
        /// PKCS#12 bundle passphrase; `-` prompts.
        #[arg(long)]
        p12_password: Option<String>,
        /// Signature reason string.
        #[arg(long)]
        reason: Option<String>,
        /// Signature location string.
        #[arg(long)]
        location: Option<String>,
        /// Signature contact info.
        #[arg(long)]
        contact: Option<String>,
        /// Signer display name.
        #[arg(long)]
        signer_name: Option<String>,
        /// Output PDF path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Remove all signature fields and signature references.
    RemoveSignatures {
        /// Input PDF path.
        input: PathBuf,
        /// Output PDF path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Report digital signatures (structural by default; `--crypto`
    /// verifies digests via openssl).
    VerifySignature {
        /// Input PDF path.
        input: PathBuf,
        /// Cryptographically verify signature digests (requires
        /// openssl; `--ca-file` adds chain verification).
        #[arg(long)]
        crypto: bool,
        /// PEM file with trusted CA certificates for chain checks.
        #[arg(long, requires = "crypto")]
        ca_file: Option<PathBuf>,
    },

    /// Show document info: page count, metadata, encryption state.
    #[command(alias = "inspect")]
    Info {
        /// Input PDF path.
        input: PathBuf,
    },

    /// Print the page count.
    PageCount {
        /// Input PDF path.
        input: PathBuf,
    },

    /// Validate PDF structure and report anomalies.
    Validate {
        /// Input PDF path.
        input: PathBuf,
    },

    /// Extract extended metadata (Info dict, dates, XMP, page size).
    ExtractMetadata {
        /// Input PDF path.
        input: PathBuf,
    },

    /// Extract embedded images; raw (still-filtered) stream bytes are
    /// written under `--output-dir`.
    ExtractImages {
        /// Input PDF path.
        input: PathBuf,
        /// Page range spec (default: all pages).
        #[arg(long)]
        pages: Option<String>,
        /// Directory for extracted image streams.
        #[arg(long)]
        output_dir: Option<PathBuf>,
    },

    /// Extract the text layer (best-effort) as plain text.
    ExtractText {
        /// Input PDF path.
        input: PathBuf,
        /// Page range spec (default: all pages).
        #[arg(long)]
        pages: Option<String>,
        /// Output text file (default: stdout).
        #[arg(long)]
        output_file: Option<PathBuf>,
    },

    /// Compare two PDFs: text diff, structure, or byte equality.
    Compare {
        /// First PDF path.
        a: PathBuf,
        /// Second PDF path.
        b: PathBuf,
        /// Comparison mode: text, structure, or bytes.
        #[arg(long, default_value = "text")]
        mode: String,
    },

    /// Render a page to PNG or JPEG via pdftoppm.
    Render {
        /// Input PDF path.
        input: PathBuf,
        /// Page range spec — the FIRST selected page is rendered
        /// (default: page 1).
        #[arg(long)]
        pages: Option<String>,
        /// Render DPI (default: engine default).
        #[arg(long)]
        dpi: Option<u32>,
        /// Output format: png or jpeg.
        #[arg(long, default_value = "png")]
        format: String,
        /// Output image path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Convert the text layer to plain text or Markdown.
    Convert {
        /// Input PDF path.
        input: PathBuf,
        /// Target format: text or markdown.
        #[arg(long, default_value = "text")]
        to: String,
        /// Page range spec (default: all pages).
        #[arg(long)]
        pages: Option<String>,
        /// Output file (default: stdout).
        #[arg(long)]
        output_file: Option<PathBuf>,
    },

    /// OCR pages via pdftoppm + tesseract (requires both on PATH).
    Ocr {
        /// Input PDF path.
        input: PathBuf,
        /// Page range spec (default: all pages).
        #[arg(long)]
        pages: Option<String>,
        /// Output text file (default: stdout).
        #[arg(long)]
        output_file: Option<PathBuf>,
    },

    /// List installed tesseract OCR language codes.
    OcrLanguages,
}

/// `kaladoc image <op>` — local-file raster image operations via
/// `ImageEngine`. Supported codecs: PNG, JPEG, GIF, WebP, BMP.
#[derive(Subcommand)]
pub enum ImageArgs {
    /// Inspect an image: format, dimensions, pixel count, size.
    Inspect {
        /// Input image path.
        input: PathBuf,
    },

    /// Resize an image to fit within a bounding box, preserving
    /// aspect ratio. At least one of `--width`/`--height` is required.
    Resize {
        /// Input image path.
        input: PathBuf,
        /// Maximum output width in pixels.
        #[arg(long)]
        width: Option<u32>,
        /// Maximum output height in pixels.
        #[arg(long)]
        height: Option<u32>,
        /// Output image path (same format as the input).
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Convert an image to another format: png, jpeg/jpg, gif, webp,
    /// bmp.
    Convert {
        /// Input image path.
        input: PathBuf,
        /// Target format.
        #[arg(long)]
        to: String,
        /// Output image path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Rotate an image by a quarter turn: 90, 180, or 270 degrees.
    Rotate {
        /// Input image path.
        input: PathBuf,
        /// Rotation in degrees: 90, 180, or 270.
        #[arg(long)]
        degrees: u16,
        /// Output image path (same format as the input).
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Re-encode an image, optionally overriding the format and/or
    /// quality (1-100; meaning depends on the codec — see
    /// `kaladoc image compress --help` engine docs).
    Compress {
        /// Input image path.
        input: PathBuf,
        /// Output format override: png, jpeg/jpg, gif, webp, bmp.
        #[arg(long)]
        format: Option<String>,
        /// Quality/effort knob 1-100.
        #[arg(long)]
        quality: Option<u8>,
        /// Output image path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Generate a thumbnail fitting within a bounding box. Never
    /// upscales; output keeps the input format.
    Thumbnail {
        /// Input image path.
        input: PathBuf,
        /// Maximum thumbnail width in pixels.
        #[arg(long)]
        width: u32,
        /// Maximum thumbnail height in pixels.
        #[arg(long)]
        height: u32,
        /// Output image path.
        #[arg(long)]
        output_file: PathBuf,
    },

    /// Show descriptive image metadata (format, dimensions, color
    /// type, alpha, size).
    Metadata {
        /// Input image path.
        input: PathBuf,
    },

    /// Validate an image header and limits without decoding pixels.
    Validate {
        /// Input image path.
        input: PathBuf,
    },
}

/// `kaladoc pipeline <op>` — list built-in pipeline definitions and
/// run them on a file. Built-ins: `pdf.intake`, `pdf.extract-text`.
#[derive(Subcommand)]
pub enum PipelineArgs {
    /// List built-in pipeline definitions with their steps.
    List,

    /// Run a built-in pipeline on a file.
    Run {
        /// Built-in pipeline name (`pdf.intake`, `pdf.extract-text`).
        name: String,
        /// Input file path (PDF for the built-in pipelines).
        input: PathBuf,
        /// Where to write a single-byte-stream result artifact.
        #[arg(long)]
        output_file: Option<PathBuf>,
        /// Directory for multi-document result artifacts
        /// (`<stem>-part-NNN.pdf`).
        #[arg(long)]
        output_dir: Option<PathBuf>,
    },

    /// Run a DAG workflow definition on a file.
    ///
    /// The DAG file is a `pipeline_engine::DagDefinition` JSON object:
    /// named nodes (each a `PipelineStep` with `engine`, `operation`,
    /// `params`) plus `edges` expressing dependencies. Every node
    /// executes against the same input bytes; waves run sequentially;
    /// a failed node skips its dependents while siblings continue.
    RunDag {
        /// Path to the DAG definition JSON file.
        dag: PathBuf,
        /// Input file every DAG node executes against.
        #[arg(long)]
        input: PathBuf,
        /// Directory for per-node output artifacts — each succeeded
        /// node's changed bytes are written as
        /// `<node>-<n>.<ext>`; pass-through report steps write nothing.
        #[arg(long)]
        output_dir: Option<PathBuf>,
        /// Validate the DAG and print its execution waves without
        /// running any node.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
pub enum SessionArgs {
    /// List the current user's active sessions.
    List,

    /// Revoke one of the current user's sessions by ID.
    Revoke {
        /// Session ID from `kaladoc auth session list`.
        id: i64,
    },
}

/// `kaladoc storage <op>` — local storage backend inspection.
#[derive(Subcommand)]
pub enum StorageArgs {
    /// Show storage backend status: kind, root configured, bytes
    /// used, and quota.
    Status,

    /// Show streaming upload limits (max object size, max chunk size).
    Limits,

    /// Delete staged upload files older than the threshold — crash
    /// recovery for uploads that never reached commit/abort.
    Sweep {
        /// Minimum age of a staged file before removal, in seconds
        /// (default: 24h).
        #[arg(long, default_value_t = 86_400)]
        max_age_secs: u64,
    },
}

/// `kaladoc schedule <op>` — recurring job schedules. Recurrence is a
/// fixed `interval_secs` counted in Unix seconds (no cron, no
/// timezones); each due occurrence enqueues one job when a scheduler
/// tick runs (`kaladoc jobs worker` or `serve --worker` drives ticks
/// automatically).
#[derive(Subcommand)]
pub enum ScheduleArgs {
    /// List schedules ordered by name.
    List,

    /// Create or replace a schedule (upsert by `--name`).
    Create {
        /// Unique schedule name.
        name: String,
        /// Job kind handed to the engine, e.g. `system.ping` or
        /// `storage.gc_staging`.
        #[arg(long)]
        kind: String,
        /// JSON payload copied into every enqueued occurrence
        /// (default: `{}`).
        #[arg(long)]
        payload: Option<String>,
        /// Seconds between occurrences (must be > 0).
        #[arg(long)]
        interval_secs: u64,
        /// Priority passed to each enqueued job.
        #[arg(long)]
        priority: Option<i64>,
        /// Unix seconds of the first occurrence (default:
        /// `now + interval_secs`).
        #[arg(long)]
        next_run_at: Option<i64>,
        /// Create the schedule paused (`enabled = false`).
        #[arg(long)]
        disabled: bool,
    },

    /// Delete a schedule by name.
    Delete {
        /// Schedule name from `kaladoc schedule list`.
        name: String,
    },

    /// Run one scheduler tick immediately: enqueues a job for every
    /// due schedule and reports how many fired.
    Tick {
        /// Evaluate schedules as of this Unix time instead of now.
        #[arg(long)]
        now: Option<i64>,
    },
}

/// `kaladoc media op` — run a multimedia-engine operation on a local
/// file. Ops are synchronous ffmpeg/ffprobe calls executed on a
/// blocking thread; file outputs are written to `--output-dir` or
/// `--output-file`, report ops print JSON.
#[derive(Subcommand)]
pub enum MediaArgs {
    /// Run a media op on an input file.
    Op {
        /// Input media file (the extension should match the real
        /// container so ffmpeg probes correctly).
        #[arg(long)]
        input: PathBuf,
        /// Op name — see `--list-ops` (e.g. `probe.inspect`,
        /// `conversion.transcode`, `video.trim`, `frames.thumbnail`).
        #[arg(long)]
        op: Option<String>,
        /// JSON params object forwarded to the op's options
        /// (default: `{}`).
        #[arg(long)]
        params: Option<String>,
        /// Fill `params.output_format` when the op accepts one.
        #[arg(long)]
        output_format: Option<String>,
        /// Directory receiving produced files (required when the op
        /// yields files and `--output-file` is not used).
        #[arg(long)]
        output_dir: Option<PathBuf>,
        /// Single output file path — only valid when the op produces
        /// exactly one file.
        #[arg(long)]
        output_file: Option<PathBuf>,
        /// List the supported op names and exit.
        #[arg(long)]
        list_ops: bool,
    },
}
