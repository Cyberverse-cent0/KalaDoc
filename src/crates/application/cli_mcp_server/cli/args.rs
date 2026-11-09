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

    /// Discover and inspect the built-in processing engines
    /// (convert, image, pdf, pipeline, security).
    #[command(subcommand)]
    Engine(EngineArgs),

    /// Inspect, resize, and convert raster images.
    #[command(subcommand)]
    Image(ImageArgs),

    /// List and run built-in processing pipelines.
    #[command(subcommand)]
    Pipeline(PipelineArgs),

    /// Probe and process video/audio files with the ffmpeg-backed
    /// video engine (probe, thumbnail, trim, transcode, concat,
    /// extract-audio).
    #[command(subcommand)]
    Video(VideoArgs),
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
    /// List registered engines with their advertised capabilities.
    List,

    /// Show detailed capabilities for one engine.
    Capabilities {
        /// Engine name (e.g. `convert`, `image`, `pdf`, `pipeline`,
        /// `security`, `video`).
        name: String,
    },

    /// Run health checks for every registered engine.
    Health,
}

#[derive(Subcommand)]
pub enum ImageArgs {
    /// Print image metadata (format, dimensions, pixel count, size).
    Inspect {
        /// Path to the image file.
        file: std::path::PathBuf,
    },

    /// Resize an image to fit within a bounding box, preserving
    /// aspect ratio. The output keeps the input format.
    Resize {
        /// Path to the image file.
        file: std::path::PathBuf,
        /// Maximum output width in pixels.
        #[arg(long)]
        width: Option<u32>,
        /// Maximum output height in pixels.
        #[arg(long)]
        height: Option<u32>,
        /// Output path (default: `<stem>.resized.<ext>` next to the
        /// input).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Convert an image to another format
    /// (png, jpeg/jpg, gif, webp, bmp).
    Convert {
        /// Path to the image file.
        file: std::path::PathBuf,
        /// Target format: `png`, `jpeg`/`jpg`, `gif`, `webp`, or `bmp`.
        target: String,
        /// Output path (default: the input path with the target
        /// extension).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
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
        #[arg(long)]
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
}

#[derive(Subcommand)]
pub enum PipelineArgs {
    /// List built-in pipeline definitions.
    List,

    /// Run a built-in pipeline against a file and print the step
    /// reports.
    Run {
        /// Pipeline name (e.g. `pdf.intake`, `pdf.extract-text`).
        name: String,
        /// Path to the input file.
        file: std::path::PathBuf,
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
