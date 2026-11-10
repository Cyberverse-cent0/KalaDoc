//! Per-document settings persistence — a JSON settings blob plus the
//! `is_locked` flag, stored as one row per document in
//! `document_settings_tbl`.
//!
//! Unlike [`super::metadata`], which holds descriptive fields
//! (title/author/keywords/custom fields), settings are operational:
//! view preferences, integration toggles, lock state. Rows are created
//! lazily on first write; `get_document_settings` returns `None` until
//! then.

use serde_json::{Map, Value};
use sqlx::{Row, SqlitePool, sqlite::SqliteRow};

use crate::crates::document_engine::audit::DocumentAuditEventType;
use crate::crates::document_engine::ids::DocumentId;
use crate::crates::document_engine::settings::{
    DocumentSettings, MAX_SETTINGS_BYTES, MAX_SETTINGS_KEY_LEN,
};

use super::documents::fetch_document;
use super::history::record_event;
use super::{StoreError, StoreResult, now_rfc3339};

const SETTINGS_COLS: &str = "document_id, settings, is_locked, created_at, updated_at";

fn settings_from_row(row: &SqliteRow) -> StoreResult<DocumentSettings> {
    let settings_raw: String = row.get("settings");
    let settings: Map<String, Value> = serde_json::from_str(&settings_raw)?;
    Ok(DocumentSettings {
        document_id: DocumentId::new(row.get::<String, _>("document_id")),
        settings,
        is_locked: row.get::<i64, _>("is_locked") != 0,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

fn default_settings(document_id: &DocumentId) -> DocumentSettings {
    DocumentSettings {
        document_id: document_id.clone(),
        settings: Map::new(),
        is_locked: false,
        created_at: now_rfc3339(),
        updated_at: now_rfc3339(),
    }
}

/// Validates a settings key: non-empty, at most
/// [`MAX_SETTINGS_KEY_LEN`] chars, no control characters.
fn validate_settings_key(key: &str) -> StoreResult<()> {
    if key.trim().is_empty() {
        return Err(StoreError::validation("key", "must not be empty"));
    }
    if key.chars().count() > MAX_SETTINGS_KEY_LEN {
        return Err(StoreError::validation(
            "key",
            format!("must be at most {MAX_SETTINGS_KEY_LEN} characters"),
        ));
    }
    if key.chars().any(|c| c.is_control()) {
        return Err(StoreError::validation(
            "key",
            "must not contain control characters",
        ));
    }
    Ok(())
}

/// Validates the serialized size of a settings object.
fn validate_settings_size(settings: &Map<String, Value>) -> StoreResult<()> {
    for key in settings.keys() {
        validate_settings_key(key)?;
    }
    let len = serde_json::to_string(settings)?.len();
    if len > MAX_SETTINGS_BYTES {
        return Err(StoreError::validation(
            "settings",
            format!("serialized size {len} exceeds limit of {MAX_SETTINGS_BYTES} bytes"),
        ));
    }
    Ok(())
}

/// Writes the settings row without recording an audit event. Callers
/// record their own operation-specific event so a single mutation
/// produces exactly one audit entry.
async fn write_settings_row(pool: &SqlitePool, settings: &DocumentSettings) -> StoreResult<()> {
    validate_settings_size(&settings.settings)?;

    sqlx::query(
        "INSERT INTO document_settings_tbl
         (document_id, settings, is_locked, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(document_id) DO UPDATE SET
            settings = excluded.settings,
            is_locked = excluded.is_locked,
            updated_at = excluded.updated_at",
    )
    .bind(settings.document_id.as_str())
    .bind(serde_json::to_string(&settings.settings)?)
    .bind(i64::from(settings.is_locked))
    .bind(&settings.created_at)
    .bind(now_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}

/// Inserts or fully replaces the settings row for a document.
/// `settings` and the `is_locked` flag are stored as given.
/// Returns `StoreError::NotFound` when the document does not exist and
/// records a [`DocumentAuditEventType::SettingsUpdated`] event.
pub async fn upsert_document_settings(
    pool: &SqlitePool,
    settings: &DocumentSettings,
) -> StoreResult<()> {
    fetch_document(pool, &settings.document_id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", &settings.document_id))?;

    write_settings_row(pool, settings).await?;

    record_event(
        pool,
        &settings.document_id,
        None,
        DocumentAuditEventType::SettingsUpdated,
        Some(&serde_json::json!({ "operation": "upsert" })),
    )
    .await?;
    Ok(())
}

/// Fetches the settings row for a document. Returns `None` when the
/// document has no settings stored yet.
pub async fn get_document_settings(
    pool: &SqlitePool,
    document_id: &DocumentId,
) -> StoreResult<Option<DocumentSettings>> {
    let row = sqlx::query(&format!(
        "SELECT {SETTINGS_COLS} FROM document_settings_tbl WHERE document_id = ?"
    ))
    .bind(document_id.as_str())
    .fetch_optional(pool)
    .await?;
    row.map(|r| settings_from_row(&r)).transpose()
}

/// Merges `patch` into the document's settings object — existing keys
/// not present in `patch` are preserved; keys in `patch` overwrite.
/// Creates the settings row with defaults when none exists yet.
/// `is_locked` is left unchanged. Returns the updated
/// [`DocumentSettings`].
pub async fn update_document_settings(
    pool: &SqlitePool,
    document_id: &DocumentId,
    patch: &Map<String, Value>,
) -> StoreResult<DocumentSettings> {
    fetch_document(pool, document_id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", document_id))?;

    let mut settings = get_document_settings(pool, document_id)
        .await?
        .unwrap_or_else(|| default_settings(document_id));
    for (key, value) in patch {
        validate_settings_key(key)?;
        settings.settings.insert(key.clone(), value.clone());
    }

    write_settings_row(pool, &settings).await?;
    record_event(
        pool,
        document_id,
        None,
        DocumentAuditEventType::SettingsUpdated,
        Some(&serde_json::json!({
            "operation": "update",
            "keys": patch.keys().collect::<Vec<_>>(),
        })),
    )
    .await?;
    Ok(settings)
}

/// Sets (or overwrites) the `is_locked` flag, creating the settings
/// row with defaults when none exists yet. Returns the updated
/// [`DocumentSettings`].
pub async fn set_document_locked(
    pool: &SqlitePool,
    document_id: &DocumentId,
    locked: bool,
) -> StoreResult<DocumentSettings> {
    fetch_document(pool, document_id)
        .await?
        .ok_or_else(|| StoreError::not_found("document", document_id))?;

    let mut settings = get_document_settings(pool, document_id)
        .await?
        .unwrap_or_else(|| default_settings(document_id));
    settings.is_locked = locked;
    write_settings_row(pool, &settings).await?;
    record_event(
        pool,
        document_id,
        None,
        DocumentAuditEventType::SettingsUpdated,
        Some(&serde_json::json!({ "operation": "set_locked", "locked": locked })),
    )
    .await?;
    Ok(settings)
}

/// Deletes the settings row for a document. Returns `true` when a row
/// was removed. The document itself is not touched.
pub async fn delete_document_settings(
    pool: &SqlitePool,
    document_id: &DocumentId,
) -> StoreResult<bool> {
    let done = sqlx::query("DELETE FROM document_settings_tbl WHERE document_id = ?")
        .bind(document_id.as_str())
        .execute(pool)
        .await?;
    if done.rows_affected() > 0 {
        record_event(
            pool,
            document_id,
            None,
            DocumentAuditEventType::SettingsUpdated,
            Some(&serde_json::json!({ "operation": "delete" })),
        )
        .await?;
    }
    Ok(done.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::document_engine::document::{DocumentOrigin, DocumentSource, DocumentType};
    use crate::crates::document_engine::store::documents::{NewDocument, create_document};
    use crate::crates::document_engine::store::schema::init_document_store;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite pool");
        init_document_store(&pool).await.expect("init store");
        pool
    }

    async fn new_doc(pool: &SqlitePool, name: &str) -> DocumentId {
        create_document(
            pool,
            &NewDocument {
                name: name.to_string(),
                document_type: DocumentType::Pdf,
                owner_id: "user-1".to_string(),
                source: DocumentSource::new(DocumentOrigin::Upload),
            },
        )
        .await
        .expect("create document")
        .id
    }

    fn sample_settings(document_id: &DocumentId) -> DocumentSettings {
        let mut settings = Map::new();
        settings.insert("default_view".to_string(), Value::from("continuous"));
        settings.insert("retention_days".to_string(), Value::from(365));
        settings.insert(
            "integration".to_string(),
            serde_json::json!({"enabled": true, "tags": ["a", "b"]}),
        );
        DocumentSettings {
            document_id: document_id.clone(),
            settings,
            is_locked: true,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
        }
    }

    #[tokio::test]
    async fn settings_roundtrip() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;

        assert!(
            get_document_settings(&pool, &doc_id)
                .await
                .unwrap()
                .is_none()
        );

        let s = sample_settings(&doc_id);
        upsert_document_settings(&pool, &s).await.unwrap();

        let fetched = get_document_settings(&pool, &doc_id)
            .await
            .unwrap()
            .expect("settings row");
        assert_eq!(fetched.settings, s.settings);
        assert!(fetched.is_locked);
        // created_at survives the upsert's `updated_at` refresh.
        assert_eq!(fetched.created_at, s.created_at);
    }

    #[tokio::test]
    async fn settings_upsert_replaces() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;

        upsert_document_settings(&pool, &sample_settings(&doc_id))
            .await
            .unwrap();

        let mut replacement = default_settings(&doc_id);
        replacement
            .settings
            .insert("only_key".to_string(), Value::from(true));
        upsert_document_settings(&pool, &replacement).await.unwrap();

        let fetched = get_document_settings(&pool, &doc_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.settings.len(), 1);
        assert!(fetched.settings.contains_key("only_key"));
        assert!(!fetched.is_locked);

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM document_settings_tbl WHERE document_id = ?")
                .bind(doc_id.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn settings_update_merges_and_preserves_lock() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;

        // Lock first so the row exists before any settings blob.
        let locked = set_document_locked(&pool, &doc_id, true).await.unwrap();
        assert!(locked.is_locked);
        assert!(locked.settings.is_empty());

        let mut patch = Map::new();
        patch.insert("view".to_string(), Value::from("grid"));
        let updated = update_document_settings(&pool, &doc_id, &patch)
            .await
            .unwrap();
        assert_eq!(updated.settings["view"], Value::from("grid"));
        // is_locked is untouched by settings updates.
        assert!(updated.is_locked);

        // A second patch merges rather than replaces.
        let mut patch2 = Map::new();
        patch2.insert("zoom".to_string(), Value::from(150));
        let updated = update_document_settings(&pool, &doc_id, &patch2)
            .await
            .unwrap();
        assert_eq!(updated.settings.len(), 2);
        assert_eq!(updated.settings["view"], Value::from("grid"));

        let unlocked = set_document_locked(&pool, &doc_id, false).await.unwrap();
        assert!(!unlocked.is_locked);
        assert_eq!(unlocked.settings.len(), 2);
    }

    #[tokio::test]
    async fn settings_validation() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;

        // Empty key rejected.
        let mut patch = Map::new();
        patch.insert("".to_string(), Value::from(1));
        assert!(matches!(
            update_document_settings(&pool, &doc_id, &patch).await,
            Err(StoreError::Validation { .. })
        ));

        // Oversized key rejected.
        let mut patch = Map::new();
        patch.insert("k".repeat(MAX_SETTINGS_KEY_LEN + 1), Value::from(1));
        assert!(matches!(
            update_document_settings(&pool, &doc_id, &patch).await,
            Err(StoreError::Validation { .. })
        ));

        // Oversized payload rejected.
        let mut big = Map::new();
        big.insert(
            "blob".to_string(),
            Value::from("x".repeat(MAX_SETTINGS_BYTES)),
        );
        let mut s = default_settings(&doc_id);
        s.settings = big;
        assert!(matches!(
            upsert_document_settings(&pool, &s).await,
            Err(StoreError::Validation { .. })
        ));

        // Missing document rejected everywhere.
        let missing = DocumentId::new("doc-missing");
        assert!(matches!(
            get_document_settings(&pool, &missing).await,
            Ok(None)
        ));
        assert!(matches!(
            set_document_locked(&pool, &missing, true).await,
            Err(StoreError::NotFound { .. })
        ));
        assert!(matches!(
            update_document_settings(&pool, &missing, &Map::new()).await,
            Err(StoreError::NotFound { .. })
        ));
        let mut s = default_settings(&missing);
        s.settings.insert("k".to_string(), Value::from(1));
        assert!(matches!(
            upsert_document_settings(&pool, &s).await,
            Err(StoreError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn settings_delete_and_cascade() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;
        upsert_document_settings(&pool, &sample_settings(&doc_id))
            .await
            .unwrap();

        assert!(delete_document_settings(&pool, &doc_id).await.unwrap());
        assert!(!delete_document_settings(&pool, &doc_id).await.unwrap());
        assert!(
            get_document_settings(&pool, &doc_id)
                .await
                .unwrap()
                .is_none()
        );

        // ON DELETE CASCADE removes settings with the document.
        upsert_document_settings(&pool, &sample_settings(&doc_id))
            .await
            .unwrap();
        sqlx::query("DELETE FROM documents_tbl WHERE id = ?")
            .bind(doc_id.as_str())
            .execute(&pool)
            .await
            .unwrap();
        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM document_settings_tbl WHERE document_id = ?")
                .bind(doc_id.as_str())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(remaining, 0);
    }

    #[tokio::test]
    async fn settings_changes_record_audit_events() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "report.pdf").await;

        upsert_document_settings(&pool, &sample_settings(&doc_id))
            .await
            .unwrap();
        let mut patch = Map::new();
        patch.insert("k".to_string(), Value::from(1));
        update_document_settings(&pool, &doc_id, &patch)
            .await
            .unwrap();
        set_document_locked(&pool, &doc_id, false).await.unwrap();
        delete_document_settings(&pool, &doc_id).await.unwrap();

        let events = super::super::history::get_document_history(&pool, &doc_id)
            .await
            .unwrap();
        let settings_events: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == DocumentAuditEventType::SettingsUpdated)
            .collect();
        assert_eq!(settings_events.len(), 4);
        let ops: Vec<_> = settings_events
            .iter()
            .filter_map(|e| {
                e.details
                    .as_ref()
                    .and_then(|d| d.get("operation"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(ops, ["upsert", "update", "set_locked", "delete"]);
    }
}
