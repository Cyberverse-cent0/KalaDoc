//! Global command-line options (spec §3), flattened into the root [`Cli`].

use clap::Args;
use std::path::PathBuf;

use super::output::OutputFormat;

/// Options accepted before or after the subcommand.
#[derive(Args, Clone, Debug)]
pub struct GlobalOpts {
    /// Use a specific configuration file instead of the default search.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Select a named profile. Reserved — profiles are not implemented yet.
    #[arg(long, global = true, value_name = "NAME")]
    pub profile: Option<String>,

    /// Use a remote KalaDoc server. Reserved — remote mode is not
    /// implemented yet; the CLI currently runs against the local database.
    #[arg(long, global = true, value_name = "URL")]
    pub server: Option<String>,

    /// Output format for command results.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,

    /// Suppress non-essential output.
    #[arg(long, global = true)]
    pub quiet: bool,

    /// Increase diagnostic output (sets log level to debug).
    #[arg(long, global = true)]
    pub verbose: bool,

    /// Disable terminal colors and styling.
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Request timeout (e.g. `30s`, `2m`). Reserved for remote mode.
    #[arg(long, global = true, value_name = "DUR")]
    pub request_timeout: Option<String>,
}
