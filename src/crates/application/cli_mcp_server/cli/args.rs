//! Clap command tree. Only commands backed by real services exist —
//! per spec, no placeholders or fake success responses.

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

    /// Start the HTTP API server (binds `api.addr` unless overridden).
    Serve {
        /// Bind address override, e.g. `0.0.0.0:8080`.
        #[arg(long)]
        addr: Option<String>,
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

    /// Register a new document (metadata only — file upload is not
    /// supported by the CLI yet).
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

    /// Manage document folders.
    #[command(subcommand)]
    Folders(FolderArgs),
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
