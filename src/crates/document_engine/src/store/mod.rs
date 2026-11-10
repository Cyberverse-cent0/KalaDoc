//! SQLite-backed persistence for the document engine.
//!
//! Tables mirror the domain objects in the parent module: documents,
//! folders, artifacts, versions, tags, permissions, and audit events.
//! The generic job queue lives in the sibling `job_engine` crate
//! (`jobs_tbl` is owned by it — do not create a second jobs table here).
//!
//! Conventions follow `application::Organization`: functions take
//! `&SqlitePool`, return [`StoreResult`], and never `unwrap()` on
//! fallible I/O. Timestamps are RFC 3339 `TEXT`; enums are stored as
//! their `snake_case` serde names.

pub mod artifacts;
pub mod documents;
pub mod folders;
pub mod history;
pub mod metadata;
pub mod permissions;
pub mod relationships;
pub mod schema;
pub mod settings;
pub mod tags;
pub mod versions;

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

pub type StoreResult<T> = Result<T, StoreError>;

/// Errors from document-store operations.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
    #[error(transparent)]
    Document(#[from] crate::crates::document_engine::error::DocumentError),
    #[error("{entity} not found: {id}")]
    NotFound { entity: &'static str, id: String },
    #[error("invalid {field}: {reason}")]
    Validation { field: &'static str, reason: String },
}

impl StoreError {
    pub(crate) fn validation(field: &'static str, reason: impl Into<String>) -> Self {
        Self::Validation {
            field,
            reason: reason.into(),
        }
    }

    pub(crate) fn not_found(entity: &'static str, id: impl ToString) -> Self {
        Self::NotFound {
            entity,
            id: id.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Internals shared by all store modules
// ---------------------------------------------------------------------------

/// Generates an opaque string id with the given prefix, e.g. `doc-1a2b…`.
/// Suitable for TEXT primary keys; not cryptographically unique, but
/// nanosecond timestamps make collisions within one process impossible
/// and across processes vanishingly unlikely.
pub fn gen_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{nanos:x}")
}

/// Current UTC time as an RFC 3339 timestamp (`YYYY-MM-DDTHH:MM:SSZ`).
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// Days-from-civil algorithm (Howard Hinnant) — converts a day count
/// relative to 1970-01-01 into a `(year, month, day)` tuple.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Serializes a domain enum to its `snake_case` string form for TEXT columns.
pub(crate) fn enum_str<T: Serialize>(value: &T) -> StoreResult<String> {
    match serde_json::to_value(value)? {
        Value::String(s) => Ok(s),
        _ => Err(StoreError::validation(
            "enum",
            "did not serialize to a string",
        )),
    }
}

/// Deserializes an enum from its stored `snake_case` string. Unknown
/// values map to the enum's `#[serde(other)]` variant when present.
pub(crate) fn enum_parse<T: DeserializeOwned>(s: &str) -> StoreResult<T> {
    Ok(serde_json::from_value(Value::String(s.to_string()))?)
}
