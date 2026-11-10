//! Clap command tree. Only commands backed by real services exist —
//! per spec, no placeholders or fake success responses.

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
    /// video engine (probe, geometry, audio, frames, subtitles,
    /// analysis, packaging, and runtime inspection).
    #[command(subcommand)]
    Video(VideoArgs),

    /// Serve the Model Context Protocol (MCP) endpoint for AI clients.
    /// Exposes read-only system, engine, and media tools over the
    /// configured transport (spec §10).
    #[command(subcommand)]
    Mcp(McpArgs),
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
    // Validation / chapters / HDR / segmentation
    // ------------------------------------------------------------------
    /// Validate a media file: probe it, decode-check streams, and
    /// print a structured report of errors, warnings, and info.
    Validate {
        /// Path to the input media file.
        file: std::path::PathBuf,
    },

    /// List, set, remove, or shift embedded chapters.
    #[command(subcommand)]
    Chapters(ChaptersArgs),

    /// Inspect HDR signalling or tone-map HDR content to SDR.
    #[command(subcommand)]
    Hdr(HdrArgs),

    /// Split a video into segments or extract a time range.
    #[command(subcommand)]
    Segment(SegmentArgs),

    /// Render a still audio-visualization image (waveform,
    /// spectrogram, frequency spectrum, vectorscope, or volume
    /// meter) as PNG.
    Waveform {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Visualization kind.
        #[arg(long, value_enum, default_value_t = WaveformKindArg::Waveform)]
        kind: WaveformKindArg,
        /// Image width in pixels (default: engine default).
        #[arg(long)]
        width: Option<u32>,
        /// Image height in pixels (default: engine default).
        #[arg(long)]
        height: Option<u32>,
        /// Color override: `name|name|...` channel list for
        /// waveform/freq-spectrum, palette name for spectrogram.
        #[arg(long)]
        color: Option<String>,
        /// Scale override accepted by the chosen filter.
        #[arg(long)]
        scale: Option<String>,
        /// Output PNG path (default: `<stem>.<kind>.png`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Measure or correct audio/video sync offsets.
    #[command(subcommand)]
    Sync(SyncArgs),

    /// Stabilize shaky footage (vidstab when available, deshake
    /// fallback, or `--algo auto` to pick the best backend).
    Stabilize {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Stabilization backend.
        #[arg(long, value_enum, default_value_t = StabilizeAlgoArg::Auto)]
        algo: StabilizeAlgoArg,
        /// Shakiness 1–10 (default: 5).
        #[arg(allow_negative_numbers = true, long)]
        shakiness: Option<f64>,
        /// Smoothing 1–100 (default: 15; vidstab only).
        #[arg(allow_negative_numbers = true, long)]
        smoothing: Option<f64>,
        /// Output path (default: `<stem>.stabilized<same-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Render a short animated preview of the first seconds of a
    /// video (GIF or animated WebP).
    Preview {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Preview format.
        #[arg(long, value_enum, default_value_t = PreviewFormatArg::Gif)]
        format: PreviewFormatArg,
        /// Maximum seconds of source to animate (default: 5).
        #[arg(long)]
        duration: Option<f64>,
        /// Output frame rate (default: 10).
        #[arg(long)]
        fps: Option<f64>,
        /// Output width in pixels; height keeps aspect (default:
        /// 320).
        #[arg(long)]
        width: Option<u32>,
        /// Output path (default: `<stem>.preview.<ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Generate a mezzanine (edit master), proxy (small H.264), or
    /// archival (lossless) derivative of a video.
    Proxy {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Which derivative to produce.
        #[arg(long, value_enum, default_value_t = ProxyKindArg::Proxy)]
        kind: ProxyKindArg,
        /// Maximum output height for `--kind proxy` (default: 720).
        #[arg(long)]
        height: Option<u32>,
        /// H.264 CRF for `--kind proxy` when libx264 is the encoder
        /// (default: 23).
        #[arg(long)]
        crf: Option<u32>,
        /// x264 preset for `--kind proxy` (default: `veryfast`).
        #[arg(long)]
        preset: Option<String>,
        /// Output path (default: `<stem>.<kind>.<sniffed-ext>` —
        /// the container is detected from the produced bytes).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Print the structured metadata report for a media file
    /// (container tags, duration, bitrate, languages).
    MetadataReport {
        /// Path to the input media file.
        file: std::path::PathBuf,
    },

    /// Export a media file's metadata as a document
    /// (`json`, `xml`, `csv`, or `text`).
    MetadataExport {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Export format.
        #[arg(long, value_enum, default_value_t = MetadataFormatArg::Json)]
        format: MetadataFormatArg,
        /// Output path (default: `<stem>.metadata.<ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Diff the metadata of two media files.
    MetadataCompare {
        /// Path to the first media file.
        first: std::path::PathBuf,
        /// Path to the second media file.
        second: std::path::PathBuf,
    },

    /// Concatenate heterogeneous media files into one container,
    /// normalizing codecs via re-encode (unlike `concat`, which
    /// stream-copies same-codec inputs).
    ConcatHetero {
        /// Input media files, in playback order (at least two).
        files: Vec<std::path::PathBuf>,
        /// Target container/extension for the output, e.g. `mp4`.
        #[arg(long)]
        ext: String,
        /// Output path (default: `concat.<ext>` next to the first
        /// input).
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

/// Chapter operations for `kaladoc video chapters`.
#[derive(Subcommand)]
pub enum ChaptersArgs {
    /// List embedded chapters with millisecond bounds.
    List {
        /// Path to the input media file.
        file: std::path::PathBuf,
    },

    /// Replace the chapter table. Each `--chapter` is a
    /// `start_ms,end_ms,title` triple (title may be empty).
    /// Output containers are limited to mp4/mkv by the engine.
    Set {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// One chapter per flag: `start_ms,end_ms,title`.
        #[arg(long = "chapter", required = true)]
        chapters: Vec<String>,
        /// Output path (default: `<stem>.chapters.<sniffed-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Remove every embedded chapter.
    Remove {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Output path (default: `<stem>.nochaps.<sniffed-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Shift every chapter time by an offset in milliseconds.
    Shift {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Offset in milliseconds (negative shifts earlier).
        #[arg(allow_negative_numbers = true, long)]
        offset_ms: i64,
        /// Output path (default: `<stem>.shifted.<sniffed-ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },
}

/// HDR operations for `kaladoc video hdr`.
#[derive(Subcommand)]
pub enum HdrArgs {
    /// Detect HDR signalling on the primary video stream
    /// (transfer, primaries, colour space, bit depth, pix_fmt).
    Detect {
        /// Path to the input media file.
        file: std::path::PathBuf,
    },

    /// Tone-map HDR content to SDR (Hable curve) and re-encode to
    /// MP4/H.264.
    ToneMap {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Tone-mapper desaturation strength (optional; ffmpeg
        /// default applies when omitted).
        #[arg(allow_negative_numbers = true, long)]
        desat: Option<f64>,
        /// Signal peak override in nits (optional).
        #[arg(long)]
        peak: Option<u32>,
        /// Output path (default: `<stem>.tonemap.mp4`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },
}

/// How `kaladoc video segment split` divides the input.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SplitByArg {
    /// Fixed-length segments (`--seconds`).
    Duration,
    /// Cut at detected scene changes (`--threshold`).
    Scenes,
    /// Equal-duration parts (`--parts`).
    Equal,
}

/// Segment operations for `kaladoc video segment`.
#[derive(Subcommand)]
pub enum SegmentArgs {
    /// Split a video into multiple segment files written to a
    /// directory.
    Split {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Split strategy.
        #[arg(long, value_enum)]
        by: SplitByArg,
        /// Segment length in seconds (`--by duration`).
        #[arg(long)]
        seconds: Option<f64>,
        /// Scene-change threshold 0.0+ (`--by scenes`).
        #[arg(allow_negative_numbers = true, long)]
        threshold: Option<f64>,
        /// Number of equal parts, 2–100 (`--by equal`).
        #[arg(long)]
        parts: Option<u32>,
        /// Segment container: `mp4`, `mkv`, `webm`, or `avi`.
        #[arg(long, default_value = "mp4")]
        format: String,
        /// Output directory (default: `<stem>-split/` next to the
        /// input).
        #[arg(long = "output-dir")]
        output_dir: Option<std::path::PathBuf>,
    },

    /// Extract the `[start_ms, end_ms)` range of a video into one
    /// file.
    Extract {
        /// Path to the input video file.
        file: std::path::PathBuf,
        /// Range start in milliseconds.
        #[arg(long)]
        start_ms: u64,
        /// Range end in milliseconds (exclusive).
        #[arg(long)]
        end_ms: u64,
        /// Output container: `mp4`, `mkv`, `webm`, or `avi`.
        #[arg(long, default_value = "mp4")]
        format: String,
        /// Output path (default: `<stem>.segment.<ext>`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },
}

/// A/V sync operations for `kaladoc video sync`.
#[derive(Subcommand)]
pub enum SyncArgs {
    /// Report the probed start times of the primary video and audio
    /// streams (read-only).
    Report {
        /// Path to the input media file.
        file: std::path::PathBuf,
    },

    /// Apply a manual A/V sync correction and re-encode to MP4.
    /// Positive offsets delay a stream; negative offsets advance it.
    Fix {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Audio shift in milliseconds.
        #[arg(allow_negative_numbers = true, long, default_value_t = 0)]
        audio_offset_ms: i64,
        /// Video shift in milliseconds.
        #[arg(allow_negative_numbers = true, long, default_value_t = 0)]
        video_offset_ms: i64,
        /// Output path (default: `<stem>.synced.mp4`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },

    /// Automatically align the audio start to the video start and
    /// re-encode to MP4. Fails when either stream lacks a reported
    /// start time.
    Auto {
        /// Path to the input media file.
        file: std::path::PathBuf,
        /// Output path (default: `<stem>.synced.mp4`).
        #[arg(long = "output-file")]
        output_file: Option<std::path::PathBuf>,
    },
}

/// Visualization kinds for `kaladoc video waveform --kind`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum WaveformKindArg {
    /// Oscilloscope-style amplitude waveform.
    Waveform,
    /// Frequency-vs-time spectrogram.
    Spectrogram,
    /// Real-time FFT bar display.
    FreqSpectrum,
    /// Stereo Lissajous vectorscope (needs ≥2 audio channels).
    Vectorscope,
    /// Per-channel volume meter.
    Volume,
}

/// Stabilization backends for `kaladoc video stabilize --algo`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum StabilizeAlgoArg {
    /// Pick the best available backend (vidstab → deshake).
    Auto,
    /// Two-pass libvidstab pipeline (requires libvidstab filters).
    Vidstab,
    /// Single-pass `deshake` filter.
    Deshake,
}

/// Preview formats for `kaladoc video preview --format`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PreviewFormatArg {
    /// Animated GIF.
    Gif,
    /// Animated WebP.
    Webp,
}

/// Derivative kinds for `kaladoc video proxy --kind`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ProxyKindArg {
    /// Small long-GOP H.264 editing proxy.
    Proxy,
    /// Intra-frame editing master (ProRes/ffv1).
    Mezzanine,
    /// Lossless preservation master (ffv1/ProRes).
    Archival,
}

/// Export formats for `kaladoc video metadata-export --format`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum MetadataFormatArg {
    /// Pretty-printed JSON.
    Json,
    /// XML document.
    Xml,
    /// `field,value` CSV rows.
    Csv,
    /// `key: value` lines.
    Text,
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
pub enum McpArgs {
    /// Run the MCP server. Speaks newline-delimited JSON-RPC 2.0 on
    /// stdin/stdout for local MCP clients; runs until the client
    /// disconnects.
    Serve {
        /// Transport to listen on. Only `stdio` exists — the server
        /// never opens a network listener (spec §10, §12).
        #[arg(long, value_enum, default_value_t = McpTransport::Stdio)]
        transport: McpTransport,
    },
}

/// MCP transports. `stdio` is the only implemented transport; remote
/// Streamable HTTP is reserved for a future authenticated deployment.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum McpTransport {
    /// Newline-delimited JSON-RPC over stdin/stdout.
    Stdio,
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
