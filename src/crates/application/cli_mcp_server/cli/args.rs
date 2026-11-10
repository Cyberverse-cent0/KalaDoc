//! Clap command tree. Only commands backed by real services exist —
//! per spec, no placeholders or fake success responses.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

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

    /// Probe and process video/audio files with the ffmpeg-backed
    /// video engine (probe, geometry, audio, frames, subtitles,
    /// analysis, packaging, and runtime inspection).
    #[command(subcommand)]
    Video(VideoArgs),
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

    /// Show the full capability set of one engine: operations,
    /// input/output formats, and resource limits.
    Capabilities {
        /// Engine name (see `kaladoc engine list`).
        name: String,
    },
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


#[derive(Subcommand)]
pub enum VideoArgs {
    /// Print video metadata (duration, dimensions, fps, codecs,
    /// container format, size, audio presence).
    Probe {
        /// Path to the video file.
        file: std::path::PathBuf,
    },

    /// Extract a single PNG frame from a video at a given position.
    Thumbnail {
        /// Path to the video file.
        file: std::path::PathBuf,
        /// Timestamp in seconds to grab the frame from
        /// (default: the engine picks a representative frame).
        #[arg(allow_negative_numbers = true, long)]
        at: Option<f64>,
        /// Output PNG path (default: `<stem>.thumb.png` next to the
        /// input).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Trim a video to a time range. Stream-copies by default;
    /// `--reencode` re-encodes for frame-accurate cuts.
    Trim {
        /// Path to the video file.
        file: std::path::PathBuf,
        /// Start time in seconds.
        #[arg(long)]
        start: f64,
        /// End time in seconds (default: end of file).
        #[arg(long)]
        end: Option<f64>,
        /// Re-encode instead of stream-copying for frame-accurate
        /// cuts (slower).
        #[arg(long)]
        reencode: bool,
        /// Output path (default: `<stem>.trimmed<same-ext>` next to
        /// the input).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Transcode a video to another container/codec
    /// (e.g. `mkv`, `webm`, `mp4`).
    Transcode {
        /// Path to the video file.
        file: std::path::PathBuf,
        /// Target container/extension, e.g. `mkv` or `webm`.
        target_ext: String,
        /// Output path (default: the input path with the target
        /// extension).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Concatenate two or more videos into a single file.
    Concat {
        /// Input video files, in playback order.
        files: Vec<std::path::PathBuf>,
        /// Target container/extension for the output, e.g. `mp4`.
        #[arg(long)]
        target_ext: String,
        /// Output path (default: `concat.<ext>` next to the first
        /// input).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Extract the audio track from a video file
    /// (mp3, wav, aac, m4a, ogg).
    ExtractAudio {
        /// Path to the video file.
        file: std::path::PathBuf,
        /// Target audio extension: `mp3`, `wav`, `aac`, `m4a`, or
        /// `ogg`.
        ext: String,
        /// Output path (default: the input path with the target
        /// extension).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    // ------------------------------------------------------------------
    // Containers / remux / encode profiles
    // ------------------------------------------------------------------
    /// Remux a file into another container, copying all streams
    /// without re-encoding.
    Remux {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Target container/extension, e.g. `mkv` or `mp4`.
        target_ext: String,
        /// Output path (default: `<stem>.<ext>` next to the input).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Transcode with a named encode preset (see `kaladoc video
    /// presets`).
    Encode {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Encode preset name (e.g. `h264-crf23`, `prores`, ...).
        profile: String,
        /// Target container/extension (default: `mp4`).
        ext: Option<String>,
        /// Output path (default: `<stem>.<ext>` next to the input).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// List the named encode presets this engine can honor.
    Presets,

    // ------------------------------------------------------------------
    // Geometry
    // ------------------------------------------------------------------
    /// Resize the video to a target box.
    Resize {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Target width in pixels.
        #[arg(long)]
        width: u32,
        /// Target height in pixels.
        #[arg(long)]
        height: u32,
        /// Fit mode: `fit` (inside, keep AR), `cover` (fill, crop),
        /// `contain` (inside, pad), `stretch` (exact box).
        #[arg(long, value_enum, default_value_t = FitArg::Fit)]
        fit: FitArg,
        /// Output path (default: `<stem>.resized<same-ext>` next to
        /// the input).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Crop the video to a pixel rectangle.
    Crop {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Crop origin X in pixels.
        #[arg(long)]
        x: u32,
        /// Crop origin Y in pixels.
        #[arg(long)]
        y: u32,
        /// Crop width in pixels.
        #[arg(long)]
        width: u32,
        /// Crop height in pixels.
        #[arg(long)]
        height: u32,
        /// Output path (default: `<stem>.cropped<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Pad the video frame by the given pixel amounts.
    Pad {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Pixels added on the left.
        #[arg(long, default_value_t = 0)]
        left: u32,
        /// Pixels added on the right.
        #[arg(long, default_value_t = 0)]
        right: u32,
        /// Pixels added on the top.
        #[arg(long, default_value_t = 0)]
        top: u32,
        /// Pixels added on the bottom.
        #[arg(long, default_value_t = 0)]
        bottom: u32,
        /// Pad color (ffmpeg color name or `0xRRGGBB`).
        #[arg(long)]
        color: Option<String>,
        /// Output path (default: `<stem>.padded<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Rotate the video by an arbitrary angle (re-encoded).
    Rotate {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Rotation angle in degrees.
        #[arg(allow_negative_numbers = true, long)]
        degrees: f64,
        /// Output path (default: `<stem>.rotated<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Mirror the video horizontally (default) or vertically.
    Flip {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Flip vertically instead of horizontally.
        #[arg(long)]
        vertical: bool,
        /// Output path (default: `<stem>.flipped<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Letterbox the video into a target box (black bars top/bottom).
    Letterbox {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Target width in pixels.
        #[arg(long)]
        width: u32,
        /// Target height in pixels.
        #[arg(long)]
        height: u32,
        /// Output path (default: `<stem>.letterbox<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Pillarbox the video into a target box (black bars left/right).
    Pillarbox {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Target width in pixels.
        #[arg(long)]
        width: u32,
        /// Target height in pixels.
        #[arg(long)]
        height: u32,
        /// Output path (default: `<stem>.pillarbox<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Auto-detect and crop black borders.
    Autocrop {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Output path (default: `<stem>.autocrop<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Rotate 90 degrees (transpose axes).
    Transpose {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Rotate counter-clockwise instead of clockwise.
        #[arg(long)]
        counter_clockwise: bool,
        /// Output path (default: `<stem>.transpose<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Force landscape or portrait orientation.
    Orient {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Target orientation.
        #[arg(long, value_enum)]
        orientation: OrientArg,
        /// Output path (default: `<stem>.orient<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    // ------------------------------------------------------------------
    // Tempo / time ops
    // ------------------------------------------------------------------
    /// Change playback speed (`--factor 2` = twice as fast).
    Speed {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Speed factor (> 0; 1.0 = unchanged).
        #[arg(allow_negative_numbers = true, long)]
        factor: f64,
        /// Output path (default: `<stem>.speed<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Reverse the clip.
    Reverse {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Output path (default: `<stem>.reversed<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Loop the clip `--count` additional times.
    Loop {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Additional loop count (0 = no change).
        #[arg(long)]
        count: u32,
        /// Output path (default: `<stem>.looped<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Freeze the frame at `--at` for `--duration` seconds.
    Freeze {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Timestamp in seconds of the frame to freeze.
        #[arg(allow_negative_numbers = true, long)]
        at: f64,
        /// Freeze duration in seconds.
        #[arg(long)]
        duration: f64,
        /// Output path (default: `<stem>.freeze<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    // ------------------------------------------------------------------
    // Audio processing
    // ------------------------------------------------------------------
    /// Apply a gain in dB to the audio track.
    Volume {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Gain in decibels (negative attenuates).
        #[arg(allow_negative_numbers = true, long)]
        gain_db: f64,
        /// Output path (default: `<stem>.volume<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// EBU R128 loudness normalization.
    Normalize {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Target loudness in LUFS (default: engine's -23 LUFS).
        #[arg(allow_negative_numbers = true, long)]
        lufs: Option<f64>,
        /// Output path (default: `<stem>.normalized<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Mix the audio tracks of several files into one.
    AudioMerge {
        /// Input media files (audio is extracted and mixed).
        files: Vec<std::path::PathBuf>,
        /// Output path (default: `<stem>.merged<same-ext>` next to
        /// the first input).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Concatenate audio files into one output container.
    AudioConcat {
        /// Input audio files, in playback order.
        files: Vec<std::path::PathBuf>,
        /// Target container/extension, e.g. `mp3` or `m4a`.
        ext: String,
        /// Output path (default: `concat.<ext>` next to the first
        /// input).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Fade the audio in and/or out.
    Fade {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Fade-in duration in seconds.
        #[arg(allow_negative_numbers = true, long = "in", default_value_t = 0.0)]
        fade_in: f64,
        /// Fade-out duration in seconds.
        #[arg(allow_negative_numbers = true, long = "out", default_value_t = 0.0)]
        fade_out: f64,
        /// Output path (default: `<stem>.fade<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Remove all audio streams.
    RemoveAudio {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Output path (default: `<stem>.noaudio<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Convert to a different channel count/layout.
    Channels {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Target channel count (1 = mono, 2 = stereo, ...).
        #[arg(long)]
        count: u32,
        /// Output path (default: `<stem>.channels<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// 3-band equalizer (bass/mid/treble in dB).
    Eq {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Bass gain in dB.
        #[arg(allow_negative_numbers = true, long)]
        bass: f64,
        /// Treble gain in dB.
        #[arg(allow_negative_numbers = true, long)]
        treble: f64,
        /// Mid gain in dB (optional).
        #[arg(allow_negative_numbers = true, long)]
        mid: Option<f64>,
        /// Output path (default: `<stem>.eq<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Spectral noise reduction.
    Denoise {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Denoise strength (0.0–1.0+; higher removes more).
        #[arg(allow_negative_numbers = true, long)]
        strength: f64,
        /// Output path (default: `<stem>.denoise<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Dynamic-range compression.
    CompressAudio {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Compression threshold in dB.
        #[arg(allow_negative_numbers = true, long)]
        threshold: f64,
        /// Compression ratio (e.g. 4 = 4:1).
        #[arg(allow_negative_numbers = true, long)]
        ratio: f64,
        /// Output path (default: `<stem>.compress<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Pitch-shift the audio by semitones (duration preserved).
    Pitch {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Shift in semitones (negative lowers pitch).
        #[arg(allow_negative_numbers = true, long)]
        semitones: f64,
        /// Output path (default: `<stem>.pitch<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Render a spectrogram PNG of the audio track.
    Spectrogram {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Image width in pixels (default: engine default).
        #[arg(long)]
        width: Option<u32>,
        /// Image height in pixels (default: engine default).
        #[arg(long)]
        height: Option<u32>,
        /// Output PNG path (required).
        #[arg(long = "output-file")]
        output_file: std::path::PathBuf,
    },

    /// Weighted mix of several audio-bearing files.
    Mix {
        /// Input media files to mix.
        files: Vec<std::path::PathBuf>,
        /// Comma-separated per-input weights, e.g. `0.5,0.5`
        /// (must match the number of inputs).
        #[arg(long)]
        weights: String,
        /// Do not normalize the mixed levels.
        #[arg(long)]
        no_normalize: bool,
        /// Output path (default: `<stem>.mix<same-ext>` next to the
        /// first input).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Crossfade between two or more files.
    Crossfade {
        /// Input media files, in playback order (at least two).
        files: Vec<std::path::PathBuf>,
        /// Crossfade duration in seconds.
        #[arg(long)]
        duration: f64,
        /// Output path (default: `<stem>.crossfade<same-ext>` next
        /// to the first input).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Delay the audio track by milliseconds (negative = earlier).
    Delay {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Delay offset in milliseconds.
        #[arg(allow_negative_numbers = true, long)]
        ms: i64,
        /// Output path (default: `<stem>.delay<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    // ------------------------------------------------------------------
    // Frames / images
    // ------------------------------------------------------------------
    /// Extract still frames from a video into a directory.
    ///
    /// Select frames with exactly one of `--every`, `--fps`,
    /// `--all`, or `--keyframes`. `--output-dir` defaults to
    /// `<stem>-frames/` next to the input; files are written with
    /// sanitized basenames.
    Frames {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// One frame every N seconds.
        #[arg(long)]
        every: Option<f64>,
        /// Frame sampling rate (frames per second).
        #[arg(long)]
        fps: Option<f64>,
        /// Extract every frame.
        #[arg(long)]
        all: bool,
        /// Extract only keyframes (I-frames).
        #[arg(long)]
        keyframes: bool,
        /// Frame image format: `png` or `jpg`.
        #[arg(long, default_value = "png")]
        format: String,
        /// Output directory (default: `<stem>-frames/` next to the
        /// input).
        #[arg(long = "output-dir")]
        output_dir: Option<std::path::PathBuf>,
    },

    /// Extract every frame as a numbered image sequence.
    ImageSequence {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Image format: `png` or `jpg`.
        #[arg(long, default_value = "png")]
        format: String,
        /// Output directory (default: `<stem>-seq/` next to the
        /// input).
        #[arg(long = "output-dir")]
        output_dir: Option<std::path::PathBuf>,
    },

    /// Build a contact-sheet grid PNG from sampled frames.
    ContactSheet {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Grid columns.
        #[arg(long)]
        cols: u32,
        /// Grid rows.
        #[arg(long)]
        rows: u32,
        /// Thumbnail width in pixels (default: engine default).
        #[arg(long)]
        thumb_width: Option<u32>,
        /// Sample interval in seconds (default: engine picks).
        #[arg(long)]
        interval: Option<f64>,
        /// Output PNG path (default: `<stem>.sheet.png`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Extract one frame per detected scene change into a
    /// directory.
    SceneFrames {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Scene-change threshold (default: engine default).
        #[arg(allow_negative_numbers = true, long)]
        threshold: Option<f64>,
        /// Output directory (default: `<stem>-scenes/` next to the
        /// input).
        #[arg(long = "output-dir")]
        output_dir: Option<std::path::PathBuf>,
    },

    /// Turn still images into a video slideshow.
    Slideshow {
        /// Input image files, in playback order.
        files: Vec<std::path::PathBuf>,
        /// Frames per second (also sets per-image duration).
        #[arg(long)]
        fps: f64,
        /// Per-image duration in seconds (overrides `--fps`).
        #[arg(long)]
        duration: Option<f64>,
        /// Target container/extension, e.g. `mp4` or `webm`.
        ext: String,
        /// Output path (default: `slideshow.<ext>` next to the
        /// first input).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Convert a video to an animated GIF.
    ToGif {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Output frame rate (default: engine default).
        #[arg(long)]
        fps: Option<f64>,
        /// Output width in pixels (height scaled; default: engine
        /// default).
        #[arg(long)]
        width: Option<u32>,
        /// Output path (default: `<stem>.gif`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Convert a video to an animated WebP.
    ToWebp {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Output frame rate (default: engine default).
        #[arg(long)]
        fps: Option<f64>,
        /// Output path (default: `<stem>.webp`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Convert a video to an APNG.
    ToApng {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Output frame rate (default: engine default).
        #[arg(long)]
        fps: Option<f64>,
        /// Output path (default: `<stem>.apng`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Turn an audio file into a video with a waveform/spectrum
    /// visualization.
    AudioToVideo {
        /// Path to the input audio file.
        file: std::path::PathBuf,
        /// Visualization style.
        #[arg(long, value_enum, default_value_t = VisualStyleArg::Both)]
        style: VisualStyleArg,
        /// Video width in pixels.
        #[arg(long)]
        width: u32,
        /// Video height in pixels.
        #[arg(long)]
        height: u32,
        /// Output path (default: `<stem>.audiovid.mp4`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Convert an animated GIF to a real video container.
    GifToVideo {
        /// Path to the input GIF file.
        file: std::path::PathBuf,
        /// Target container/extension, e.g. `mp4` or `webm`.
        ext: String,
        /// Output path (default: the input path with the target
        /// extension).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Replace the audio track of a video without re-encoding the
    /// video stream.
    ReplaceAudio {
        /// Path to the video file.
        video: std::path::PathBuf,
        /// Path to the audio file to mux in.
        audio: std::path::PathBuf,
        /// Output path (default: `<stem>.replaced<same-ext>` next
        /// to the video).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    // ------------------------------------------------------------------
    // Extraction / subtitles
    // ------------------------------------------------------------------
    /// Extract embedded streams (subtitles, attachments, cover
    /// art, chapters, data) into a directory.
    ExtractStreams {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Extract subtitle streams (as `--subtitle-format` text).
        #[arg(long)]
        subtitles: bool,
        /// Extract binary attachments.
        #[arg(long)]
        attachments: bool,
        /// Extract the attached cover art.
        #[arg(long)]
        cover: bool,
        /// Export chapters as a JSON sidecar.
        #[arg(long)]
        chapters: bool,
        /// Extract data/other streams (best-effort).
        #[arg(long)]
        data: bool,
        /// Subtitle text format: `srt`, `ass`, or `vtt`.
        #[arg(long, default_value = "srt")]
        subtitle_format: String,
        /// Output directory (default: `<stem>-streams/` next to
        /// the input). `--output-dir` is recommended.
        #[arg(long = "output-dir")]
        output_dir: Option<std::path::PathBuf>,
    },

    /// Extract a subtitle track as a text file.
    Subtitles {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Subtitle track number (0-based across subtitle streams;
        /// default: first).
        #[arg(long)]
        track: Option<u32>,
        /// Output subtitle format: `srt`, `ass`, or `vtt`.
        #[arg(long, default_value = "srt")]
        format: String,
        /// Output path (default: `<stem>.<format>` next to the
        /// input).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Convert a subtitle file between `srt`, `ass`, and `vtt`.
    SubsConvert {
        /// Path to the subtitle file.
        file: std::path::PathBuf,
        /// Source format (default: guessed from the extension).
        #[arg(long)]
        from: Option<String>,
        /// Target format: `srt`, `ass`, or `vtt`.
        #[arg(long)]
        to: String,
        /// Output path (default: `<stem>.<to>` next to the input).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Shift all subtitle cue times by an offset.
    SubsShift {
        /// Path to the subtitle file.
        file: std::path::PathBuf,
        /// Time offset in seconds (negative shifts earlier).
        #[arg(allow_negative_numbers = true, long)]
        offset: f64,
        /// Subtitle format: `srt`, `ass`, or `vtt`.
        #[arg(long, default_value = "srt")]
        format: String,
        /// Output path (default: `<stem>.shifted.<format>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Burn a subtitle file into the video.
    BurnSubs {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Path to the subtitle file (srt/ass/vtt).
        subs_file: std::path::PathBuf,
        /// Optional styling as a JSON object with keys
        /// `font_name`, `font_size`, `primary_color`,
        /// `outline_color`, `outline_width`, `shadow_offset`,
        /// `margin_v`, `alignment`.
        #[arg(long)]
        style: Option<String>,
        /// Output path (default: `<stem>.burned<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    // ------------------------------------------------------------------
    // Filters / overlays / stream ops
    // ------------------------------------------------------------------
    /// Apply raw ffmpeg `-vf`/`-af` filter chains (re-encodes).
    /// Prefer `kaladoc video filters` presets for common effects.
    Filter {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Video filter chain (`-vf`), e.g. `hflip,eq=brightness=0.1`.
        #[arg(long)]
        vf: Option<String>,
        /// Audio filter chain (`-af`), e.g. `volume=2.0`.
        #[arg(long)]
        af: Option<String>,
        /// Output path (default: `<stem>.filtered<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// List the named filter presets the engine advertises.
    Filters,

    /// Overlay an image onto the video at a position.
    Overlay {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Path to the overlay image file.
        image: std::path::PathBuf,
        /// X position in pixels.
        #[arg(allow_negative_numbers = true, long)]
        x: i32,
        /// Y position in pixels.
        #[arg(allow_negative_numbers = true, long)]
        y: i32,
        /// Overlay opacity 0.0–1.0 (default: fully opaque).
        #[arg(long)]
        opacity: Option<f64>,
        /// Output path (default: `<stem>.overlay<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Draw text onto the video.
    Text {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Text to draw.
        #[arg(long)]
        text: String,
        /// X position in pixels.
        #[arg(allow_negative_numbers = true, long)]
        x: i32,
        /// Y position in pixels.
        #[arg(allow_negative_numbers = true, long)]
        y: i32,
        /// Font size in pixels (default: 24).
        #[arg(long)]
        size: Option<u32>,
        /// Text color `0xRRGGBB` (default: engine default).
        #[arg(long)]
        color: Option<String>,
        /// Output path (default: `<stem>.text<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Chroma-key composite a foreground video over a background.
    ChromaKey {
        /// Path to the foreground (keyed) video file.
        foreground: std::path::PathBuf,
        /// Path to the background video file.
        background: std::path::PathBuf,
        /// Key color `0xRRGGBB` or ffmpeg color name.
        #[arg(long)]
        color: String,
        /// Color-match similarity threshold (default: 0.3).
        #[arg(allow_negative_numbers = true, long)]
        similarity: Option<f64>,
        /// Edge blend amount (default: 0.1).
        #[arg(allow_negative_numbers = true, long)]
        blend: Option<f64>,
        /// Output path (default: `<stem>.keyed<same-ext>` next to
        /// the foreground input).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Select which streams are kept and remux into a container.
    SelectStreams {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Target container/extension, e.g. `mp4` or `mkv`.
        ext: String,
        /// Drop the video stream.
        #[arg(long)]
        no_video: bool,
        /// Audio stream ordinals to keep, comma-separated
        /// (e.g. `0,1`; default: keep all).
        #[arg(long)]
        audio: Option<String>,
        /// Subtitle stream ordinals to keep, comma-separated
        /// (default: keep all).
        #[arg(long)]
        subs: Option<String>,
        /// Output path (default: `<stem>.<ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Merge an audio file into a video container.
    MergeAv {
        /// Path to the video file.
        video: std::path::PathBuf,
        /// Path to the audio file to merge in.
        audio: std::path::PathBuf,
        /// Target container/extension (default: the video's
        /// extension).
        ext: Option<String>,
        /// Output path (default: `<stem>.merged<same-ext>` next to
        /// the video).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Set container metadata tags (`--tag key=value`, repeatable).
    SetMetadata {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Tags as `key=value` pairs (repeatable).
        #[arg(long = "tag", required = true)]
        tags: Vec<String>,
        /// Output path (default: `<stem>.tagged<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Strip all container metadata.
    StripMetadata {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Target container/extension (default: the input's
        /// extension).
        ext: Option<String>,
        /// Output path (default: `<stem>.stripped.<ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Attach/replace cover art on an audio container.
    CoverArt {
        /// Path to the audio file.
        file: std::path::PathBuf,
        /// Path to the cover image file.
        image: std::path::PathBuf,
        /// Output path (default: `<stem>.cover<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    // ------------------------------------------------------------------
    // Analysis / loudness / quality
    // ------------------------------------------------------------------
    /// Run the full media analysis report (black/freeze/silence
    /// segments, scenes, loudness, metrics).
    Analyze {
        /// Path to the input media file.
        file: std::path::PathBuf,
    },

    /// List detected scene-change timestamps and scores.
    Scenes {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Scene-change threshold (default: engine default).
        #[arg(allow_negative_numbers = true, long)]
        threshold: Option<f64>,
    },

    /// Detect black-frame intervals.
    BlackFrames {
        /// Path to the input video file.
        file: std::path::PathBuf,
    },

    /// Detect freeze-frame intervals.
    FreezeFrames {
        /// Path to the input video file.
        file: std::path::PathBuf,
    },

    /// Detect silence intervals in the audio track.
    Silences {
        /// Path to the input media file.
        file: std::path::PathBuf,
    },

    /// Measure EBU R128 loudness of the audio track.
    Loudness {
        /// Path to the input media file.
        file: std::path::PathBuf,
    },

    /// Normalize loudness to a target LUFS value.
    NormalizeLufs {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Target loudness in LUFS (default: engine's -23 LUFS).
        #[arg(allow_negative_numbers = true, long)]
        lufs: Option<f64>,
        /// Output path (default: `<stem>.loudnorm<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Print audio statistics (levels, DC offset, etc.).
    AudioStats {
        /// Path to the input media file.
        file: std::path::PathBuf,
    },

    /// Objective quality comparison between a reference and a
    /// distorted file (PSNR/SSIM/VMAF).
    Quality {
        /// Path to the reference (original) file.
        reference: std::path::PathBuf,
        /// Path to the distorted (processed) file.
        distorted: std::path::PathBuf,
        /// Comma-separated metrics: `psnr`, `ssim`, `vmaf`
        /// (default: all supported).
        #[arg(long)]
        metrics: Option<String>,
    },

    // ------------------------------------------------------------------
    // Packaging
    // ------------------------------------------------------------------
    /// Package the video as an HLS stream (`index.m3u8` +
    /// segments) into a directory.
    Hls {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Output directory (default: `<stem>-hls/` next to the
        /// input).
        #[arg(long = "output-dir")]
        output_dir: Option<std::path::PathBuf>,
    },

    /// Package the video as MPEG-DASH (`manifest.mpd` + segments)
    /// into a directory.
    Dash {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Output directory (default: `<stem>-dash/` next to the
        /// input).
        #[arg(long = "output-dir")]
        output_dir: Option<std::path::PathBuf>,
    },

    /// Remux an MP4 for faststart (moov atom up front).
    Faststart {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Output path (default: `<stem>.faststart.mp4`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    // ------------------------------------------------------------------
    // Runtime inspection
    // ------------------------------------------------------------------
    /// List capture devices/sources known to this ffmpeg build.
    Devices,

    /// List input/output protocols.
    Protocols,

    /// List hardware accelerations.
    Hwaccels,

    /// Print a JSON capability snapshot of the installed
    /// ffmpeg/ffprobe build (versions, detected hwaccel, device,
    /// protocol, and hwaccel lists).
    Capabilities,

    /// Print ffmpeg/ffprobe versions and the detected hardware
    /// accelerator.
    Versions,
}

/// Fit modes for `kaladoc video resize`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum FitArg {
    /// Preserve aspect ratio; fit entirely inside the box.
    Fit,
    /// Preserve aspect ratio; fill the box, cropping overflow.
    Cover,
    /// Preserve aspect ratio; fit inside and pad the remainder.
    Contain,
    /// Ignore aspect ratio; stretch to exactly the box.
    Stretch,
}

/// Orientations for `kaladoc video orient`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OrientArg {
    /// Width >= height orientation.
    Landscape,
    /// Height > width orientation.
    Portrait,
}

/// Visualization styles for `kaladoc video audio-to-video`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum VisualStyleArg {
    /// Oscilloscope waveform.
    Waveform,
    /// Frequency spectrum.
    Spectrum,
    /// Waveform and spectrum stacked.
    Both,
}

