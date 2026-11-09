//! Resolved runtime context shared by all commands: global options plus
//! helpers to load configuration and open the database.

use serde::Serialize;

use crate::config::AppConfig;
use crate::crates::application::Organization::init_db::DbConfig;

use super::args::Cli;
use super::error::CliError;
use super::global::GlobalOpts;
use super::output::OutputFormat;

/// Everything a command needs that isn't command-specific.
pub struct Context {
    pub globals: GlobalOpts,
    pub output: OutputFormat,
}

impl Context {
    /// Builds a context, rejecting flags for features that don't exist
    /// yet rather than silently ignoring them.
    pub fn new(cli: &Cli) -> Result<Self, CliError> {
        let g = &cli.global;
        if g.server.is_some() || g.profile.is_some() || g.request_timeout.is_some() {
            return Err(CliError::Unsupported(
                "remote server mode, profiles, and request timeouts are not \
                 implemented yet; the CLI runs against the local database"
                    .to_string(),
            ));
        }
        Ok(Self {
            globals: g.clone(),
            output: g.output,
        })
    }

    pub fn json(&self) -> bool {
        self.output.is_json()
    }

    /// Emit a JSON document on stdout (pretty-printed). Only call this
    /// when [`Context::json`] is true — table output is printed by the
    /// command itself.
    pub fn emit_json<T: Serialize>(&self, value: &T) -> Result<(), CliError> {
        println!("{}", serde_json::to_string_pretty(value)?);
        Ok(())
    }

    /// Emit a YAML document on stdout when `--output yaml` was selected.
    pub fn emit_yaml<T: Serialize>(&self, value: &T) -> Result<(), CliError> {
        println!("{}", serde_yaml::to_string(value).map_err(CliError::Yaml)?);
        Ok(())
    }

    /// Emit `value` in the selected structured format. Returns `true`
    /// when JSON/YAML output was written; `false` means the command
    /// should render its table output.
    pub fn emit_structured<T: Serialize>(&self, value: &T) -> Result<bool, CliError> {
        match self.output {
            OutputFormat::Json => {
                self.emit_json(value)?;
                Ok(true)
            }
            OutputFormat::Yaml => {
                self.emit_yaml(value)?;
                Ok(true)
            }
            OutputFormat::Table => Ok(false),
        }
    }

    /// Strict config load — fails when required secret env vars
    /// (`APP_SECRET_KEY`, `APP_DB_USER`, `APP_DB_PASSWORD`) are missing.
    pub fn config(&self) -> Result<AppConfig, CliError> {
        match &self.globals.config {
            Some(path) => Ok(AppConfig::load_with(path)?),
            None => Ok(AppConfig::load()?),
        }
    }

    /// Lenient config load for display/diagnostic commands: missing
    /// secrets come back empty instead of failing.
    pub fn config_lenient(&self) -> Result<AppConfig, CliError> {
        match &self.globals.config {
            Some(path) => Ok(AppConfig::load_lenient_with(path)?),
            None => Ok(AppConfig::load_lenient()?),
        }
    }

    /// Resolved [`DbConfig`] (includes session settings).
    pub fn db_config(&self) -> Result<DbConfig, CliError> {
        Ok(self.config()?.to_db_config())
    }
}
