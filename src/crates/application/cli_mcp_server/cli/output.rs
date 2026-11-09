//! Output format selection (`--output table|json`).
//!
//! Contract (spec §5.1): JSON mode emits one JSON document on **stdout**;
//! progress and diagnostics always go to **stderr**. Field names in JSON
//! output are a stable contract — do not rename casually.

use clap::ValueEnum;

/// How command results are rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable key/value lines and tables.
    #[default]
    Table,
    /// A single pretty-printed JSON document on stdout.
    Json,
}

impl OutputFormat {
    pub fn is_json(self) -> bool {
        matches!(self, Self::Json)
    }
}
