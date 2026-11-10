//! Schema creation for the document store. Idempotent — safe to call on
//! every startup, like `create_users_table` in `application::Organization`.

use sqlx::{Executor, SqlitePool};

/// Creates all document-engine tables and indexes. Returns `sqlx::Error`
/// on failure; callers wrap it in their own error type.
pub async fn init_document_store(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    create_folders_table(pool).await?;
    create_documents_table(pool).await?;
    create_artifacts_table(pool).await?;
    create_versions_table(pool).await?;
    create_tags_tables(pool).await?;
    create_permissions_table(pool).await?;
    create_metadata_table(pool).await?;
    create_settings_table(pool).await?;
    create_relationships_table(pool).await?;
    create_audit_table(pool).await?;
    Ok(())
}

async fn create_folders_table(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS folders_tbl (
            id         TEXT PRIMARY KEY,
            name       TEXT NOT NULL,
            parent_id  TEXT REFERENCES folders_tbl(id) ON DELETE CASCADE,
            owner_id   TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        "#,
    )
    .await?;
    pool.execute("CREATE INDEX IF NOT EXISTS idx_folders_parent ON folders_tbl(parent_id);")
        .await?;
    pool.execute("CREATE INDEX IF NOT EXISTS idx_folders_owner ON folders_tbl(owner_id);")
        .await?;
    Ok(())
}

async fn create_documents_table(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS documents_tbl (
            id                  TEXT PRIMARY KEY,
            name                TEXT NOT NULL,
            document_type       TEXT NOT NULL,
            status              TEXT NOT NULL DEFAULT 'active',
            lifecycle_status    TEXT NOT NULL DEFAULT 'created',
            origin              TEXT NOT NULL,
            source_uri          TEXT,
            source_external_ref TEXT,
            source_imported_at  TEXT,
            folder_id           TEXT REFERENCES folders_tbl(id) ON DELETE SET NULL,
            owner_id            TEXT NOT NULL,
            created_at          TEXT NOT NULL,
            updated_at          TEXT NOT NULL,
            deleted_at          TEXT
        );
        "#,
    )
    .await?;
    pool.execute("CREATE INDEX IF NOT EXISTS idx_documents_owner ON documents_tbl(owner_id);")
        .await?;
    pool.execute("CREATE INDEX IF NOT EXISTS idx_documents_folder ON documents_tbl(folder_id);")
        .await?;
    pool.execute("CREATE INDEX IF NOT EXISTS idx_documents_status ON documents_tbl(status);")
        .await?;
    Ok(())
}

async fn create_artifacts_table(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS artifacts_tbl (
            id                 TEXT PRIMARY KEY,
            document_id        TEXT NOT NULL REFERENCES documents_tbl(id) ON DELETE CASCADE,
            storage_uri        TEXT NOT NULL,
            file_name          TEXT NOT NULL,
            mime_type          TEXT,
            size_bytes         INTEGER NOT NULL DEFAULT 0,
            checksum_algorithm TEXT,
            checksum_value     TEXT,
            created_at         TEXT NOT NULL
        );
        "#,
    )
    .await?;
    pool.execute(
        "CREATE INDEX IF NOT EXISTS idx_artifacts_document ON artifacts_tbl(document_id);",
    )
    .await?;
    Ok(())
}

async fn create_versions_table(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS revisions_tbl (
            id          TEXT PRIMARY KEY,
            document_id TEXT NOT NULL REFERENCES documents_tbl(id) ON DELETE CASCADE,
            summary     TEXT NOT NULL,
            author_id   TEXT NOT NULL,
            created_at  TEXT NOT NULL
        );
        "#,
    )
    .await?;
    pool.execute(
        "CREATE INDEX IF NOT EXISTS idx_revisions_document ON revisions_tbl(document_id);",
    )
    .await?;

    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS versions_tbl (
            document_id     TEXT NOT NULL REFERENCES documents_tbl(id) ON DELETE CASCADE,
            version_number  INTEGER NOT NULL,
            artifact_id     TEXT REFERENCES artifacts_tbl(id) ON DELETE SET NULL,
            revision_ids    TEXT NOT NULL DEFAULT '[]',
            created_by      TEXT NOT NULL,
            note            TEXT,
            is_current      INTEGER NOT NULL DEFAULT 0,
            created_at      TEXT NOT NULL,
            PRIMARY KEY (document_id, version_number)
        );
        "#,
    )
    .await?;
    pool.execute(
        "CREATE INDEX IF NOT EXISTS idx_versions_current ON versions_tbl(document_id, is_current);",
    )
    .await?;
    Ok(())
}

async fn create_tags_tables(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS tags_tbl (
            id         TEXT PRIMARY KEY,
            name       TEXT NOT NULL UNIQUE COLLATE NOCASE,
            color      TEXT,
            created_at TEXT NOT NULL
        );
        "#,
    )
    .await?;

    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS tag_assignments_tbl (
            document_id TEXT NOT NULL REFERENCES documents_tbl(id) ON DELETE CASCADE,
            tag_id      TEXT NOT NULL REFERENCES tags_tbl(id) ON DELETE CASCADE,
            assigned_by TEXT,
            assigned_at TEXT NOT NULL,
            PRIMARY KEY (document_id, tag_id)
        );
        "#,
    )
    .await?;
    pool.execute(
        "CREATE INDEX IF NOT EXISTS idx_tag_assignments_tag ON tag_assignments_tbl(tag_id);",
    )
    .await?;
    Ok(())
}

async fn create_permissions_table(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS document_permissions_tbl (
            document_id  TEXT NOT NULL REFERENCES documents_tbl(id) ON DELETE CASCADE,
            subject_id   TEXT NOT NULL,
            access_level TEXT NOT NULL,
            granted_by   TEXT,
            granted_at   TEXT NOT NULL,
            PRIMARY KEY (document_id, subject_id)
        );
        "#,
    )
    .await?;
    pool.execute(
        "CREATE INDEX IF NOT EXISTS idx_doc_permissions_subject ON document_permissions_tbl(subject_id);",
    )
    .await?;
    Ok(())
}

async fn create_metadata_table(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS document_metadata_tbl (
            document_id   TEXT PRIMARY KEY REFERENCES documents_tbl(id) ON DELETE CASCADE,
            title         TEXT,
            author        TEXT,
            subject       TEXT,
            language      TEXT,
            keywords      TEXT NOT NULL DEFAULT '[]',
            custom_fields TEXT NOT NULL DEFAULT '{}',
            created_at    TEXT NOT NULL,
            updated_at    TEXT NOT NULL
        );
        "#,
    )
    .await?;
    Ok(())
}

async fn create_settings_table(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS document_settings_tbl (
            document_id TEXT PRIMARY KEY REFERENCES documents_tbl(id) ON DELETE CASCADE,
            settings    TEXT NOT NULL DEFAULT '{}',
            is_locked   INTEGER NOT NULL DEFAULT 0,
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL
        );
        "#,
    )
    .await?;
    Ok(())
}

async fn create_relationships_table(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS document_relationships_tbl (
            id                  TEXT PRIMARY KEY,
            source_document_id  TEXT NOT NULL REFERENCES documents_tbl(id) ON DELETE CASCADE,
            target_document_id  TEXT NOT NULL REFERENCES documents_tbl(id) ON DELETE CASCADE,
            relationship_type   TEXT NOT NULL,
            created_by          TEXT,
            created_at          TEXT NOT NULL,
            UNIQUE (source_document_id, target_document_id, relationship_type)
        );
        "#,
    )
    .await?;
    pool.execute(
        "CREATE INDEX IF NOT EXISTS idx_doc_relationships_source ON document_relationships_tbl(source_document_id);",
    )
    .await?;
    pool.execute(
        "CREATE INDEX IF NOT EXISTS idx_doc_relationships_target ON document_relationships_tbl(target_document_id);",
    )
    .await?;
    Ok(())
}

async fn create_audit_table(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    pool.execute(
        r#"
        CREATE TABLE IF NOT EXISTS document_audit_tbl (
            id          TEXT PRIMARY KEY,
            document_id TEXT NOT NULL REFERENCES documents_tbl(id) ON DELETE CASCADE,
            actor_id    TEXT,
            event_type  TEXT NOT NULL,
            occurred_at TEXT NOT NULL,
            details     TEXT
        );
        "#,
    )
    .await?;
    pool.execute(
        "CREATE INDEX IF NOT EXISTS idx_document_audit_doc ON document_audit_tbl(document_id);",
    )
    .await?;
    Ok(())
}

// `jobs_tbl` is owned by `crate::crates::job_engine` — its
// `create_jobs_table` is called from `init_db_with` alongside this
// function. Do not define a second jobs table here.

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::{Row, SqlitePool};

    async fn memory_pool() -> SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite pool")
    }

    async fn object_names(pool: &SqlitePool, kind: &str) -> Vec<String> {
        sqlx::query("SELECT name FROM sqlite_master WHERE type = ? ORDER BY name")
            .bind(kind)
            .fetch_all(pool)
            .await
            .expect("query sqlite_master")
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect()
    }

    const EXPECTED_TABLES: &[&str] = &[
        "artifacts_tbl",
        "document_audit_tbl",
        "document_metadata_tbl",
        "document_permissions_tbl",
        "document_relationships_tbl",
        "document_settings_tbl",
        "documents_tbl",
        "folders_tbl",
        "revisions_tbl",
        "tag_assignments_tbl",
        "tags_tbl",
        "versions_tbl",
    ];

    const EXPECTED_INDEXES: &[&str] = &[
        "idx_artifacts_document",
        "idx_doc_permissions_subject",
        "idx_doc_relationships_source",
        "idx_doc_relationships_target",
        "idx_document_audit_doc",
        "idx_documents_folder",
        "idx_documents_owner",
        "idx_documents_status",
        "idx_folders_owner",
        "idx_folders_parent",
        "idx_revisions_document",
        "idx_tag_assignments_tag",
        "idx_versions_current",
    ];

    #[tokio::test]
    async fn init_document_store_is_idempotent() {
        let pool = memory_pool().await;
        init_document_store(&pool).await.expect("first init");
        init_document_store(&pool).await.expect("second init");
        init_document_store(&pool).await.expect("third init");
    }

    #[tokio::test]
    async fn init_document_store_creates_all_tables() {
        let pool = memory_pool().await;
        init_document_store(&pool).await.expect("init");

        let tables = object_names(&pool, "table").await;
        for expected in EXPECTED_TABLES {
            assert!(
                tables.iter().any(|t| t == expected),
                "missing table {expected}; have {tables:?}"
            );
        }
    }

    #[tokio::test]
    async fn init_document_store_creates_all_indexes() {
        let pool = memory_pool().await;
        init_document_store(&pool).await.expect("init");

        let indexes = object_names(&pool, "index").await;
        for expected in EXPECTED_INDEXES {
            assert!(
                indexes.iter().any(|i| i == expected),
                "missing index {expected}; have {indexes:?}"
            );
        }
    }

    #[tokio::test]
    async fn init_document_store_enables_foreign_keys() {
        let pool = memory_pool().await;
        init_document_store(&pool).await.expect("init");
        // The store relies on ON DELETE CASCADE / SET NULL semantics;
        // sqlx enables `PRAGMA foreign_keys` by default.
        let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&pool)
            .await
            .expect("pragma");
        assert_eq!(fk, 1);
    }

    #[tokio::test]
    async fn init_document_store_adds_missing_tables_to_existing_db() {
        let pool = memory_pool().await;

        // Simulate an older database that only has the folders table.
        pool.execute(
            "CREATE TABLE folders_tbl (
                id         TEXT PRIMARY KEY,
                name       TEXT NOT NULL,
                parent_id  TEXT,
                owner_id   TEXT NOT NULL,
                created_at TEXT NOT NULL
            )",
        )
        .await
        .expect("create legacy folders_tbl");

        init_document_store(&pool)
            .await
            .expect("init over legacy db");

        let tables = object_names(&pool, "table").await;
        for expected in EXPECTED_TABLES {
            assert!(
                tables.iter().any(|t| t == expected),
                "missing table {expected} after migration"
            );
        }
        let indexes = object_names(&pool, "index").await;
        for expected in EXPECTED_INDEXES {
            assert!(
                indexes.iter().any(|i| i == expected),
                "missing index {expected} after migration"
            );
        }

        // The pre-existing table was left untouched (IF NOT EXISTS).
        let cols: Vec<String> = sqlx::query("PRAGMA table_info(folders_tbl)")
            .fetch_all(&pool)
            .await
            .expect("table_info")
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();
        assert!(!cols.contains(&"updated_at".to_string()));
    }
}
