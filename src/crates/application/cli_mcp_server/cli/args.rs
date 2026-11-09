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
pub enum SessionArgs {
    /// List the current user's active sessions.
    List,

    /// Revoke one of the current user's sessions by ID.
    Revoke {
        /// Session ID from `kaladoc auth session list`.
        id: i64,
    },
}
