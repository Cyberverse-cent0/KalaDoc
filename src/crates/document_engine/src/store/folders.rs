//! Folder CRUD and hierarchy operations.

use sqlx::{Row, SqlitePool, sqlite::SqliteRow};

use crate::crates::document_engine::ids::FolderId;
use crate::crates::document_engine::organization::Folder;

use super::{StoreError, StoreResult, gen_id, now_rfc3339};

fn folder_from_row(row: &SqliteRow) -> Folder {
    Folder {
        id: FolderId::new(row.get::<String, _>("id")),
        name: row.get("name"),
        parent_id: row.get::<Option<String>, _>("parent_id").map(FolderId::new),
        owner_id: row.get("owner_id"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

const FOLDER_COLS: &str = "id, name, parent_id, owner_id, created_at, updated_at";

async fn fetch_folder(pool: &SqlitePool, id: &FolderId) -> StoreResult<Option<Folder>> {
    let row = sqlx::query(&format!(
        "SELECT {FOLDER_COLS} FROM folders_tbl WHERE id = ?"
    ))
    .bind(id.as_str())
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| folder_from_row(&r)))
}

/// Creates a folder, optionally nested under `parent_id`. Returns
/// `NotFound` when the parent does not exist.
pub async fn create_folder(
    pool: &SqlitePool,
    name: &str,
    parent_id: Option<&FolderId>,
    owner_id: &str,
) -> StoreResult<Folder> {
    if name.trim().is_empty() {
        return Err(StoreError::validation("name", "must not be empty"));
    }
    if let Some(parent) = parent_id
        && fetch_folder(pool, parent).await?.is_none()
    {
        return Err(StoreError::not_found("folder", parent));
    }

    let now = now_rfc3339();
    let folder = Folder {
        id: FolderId::new(gen_id("fld")),
        name: name.trim().to_string(),
        parent_id: parent_id.cloned(),
        owner_id: owner_id.to_string(),
        created_at: now.clone(),
        updated_at: now,
    };
    sqlx::query(
        "INSERT INTO folders_tbl (id, name, parent_id, owner_id, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(folder.id.as_str())
    .bind(&folder.name)
    .bind(folder.parent_id.as_ref().map(|p| p.as_str()))
    .bind(&folder.owner_id)
    .bind(&folder.created_at)
    .bind(&folder.updated_at)
    .execute(pool)
    .await?;
    Ok(folder)
}

/// Fetches a folder by id. Returns `None` when no such folder exists.
pub async fn get_folder(pool: &SqlitePool, id: &FolderId) -> StoreResult<Option<Folder>> {
    fetch_folder(pool, id).await
}

/// Lists folders, optionally filtered by owner. `parent_id` selects
/// direct children of that folder; `None` lists only root-level
/// folders. Callers build deeper trees by walking `parent_id` links.
pub async fn list_folders(
    pool: &SqlitePool,
    owner_id: Option<&str>,
    parent_id: Option<&FolderId>,
) -> StoreResult<Vec<Folder>> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
        "SELECT {FOLDER_COLS} FROM folders_tbl WHERE 1=1"
    ));
    if let Some(owner) = owner_id {
        qb.push(" AND owner_id = ").push_bind(owner);
    }
    if let Some(parent) = parent_id {
        qb.push(" AND parent_id = ").push_bind(parent.as_str());
    } else {
        qb.push(" AND parent_id IS NULL");
    }
    qb.push(" ORDER BY name");
    let rows = qb.build().fetch_all(pool).await?;
    Ok(rows.iter().map(folder_from_row).collect())
}

/// Renames a folder. Returns `NotFound` when it does not exist.
pub async fn rename_folder(pool: &SqlitePool, id: &FolderId, name: &str) -> StoreResult<Folder> {
    if name.trim().is_empty() {
        return Err(StoreError::validation("name", "must not be empty"));
    }
    let done = sqlx::query("UPDATE folders_tbl SET name = ?, updated_at = ? WHERE id = ?")
        .bind(name.trim())
        .bind(now_rfc3339())
        .bind(id.as_str())
        .execute(pool)
        .await?;
    if done.rows_affected() == 0 {
        return Err(StoreError::not_found("folder", id));
    }
    fetch_folder(pool, id)
        .await?
        .ok_or_else(|| StoreError::not_found("folder", id))
}

/// Reparents a folder (`None` moves it to the root). Rejects moves that
/// would create a cycle — the new parent must not be the folder itself
/// or one of its descendants. Child folders are *not* reparented
/// implicitly; they keep their `parent_id` and simply move with the
/// subtree.
pub async fn move_folder(
    pool: &SqlitePool,
    id: &FolderId,
    new_parent: Option<&FolderId>,
) -> StoreResult<Folder> {
    if fetch_folder(pool, id).await?.is_none() {
        return Err(StoreError::not_found("folder", id));
    }
    if let Some(parent) = new_parent {
        if parent == id {
            return Err(StoreError::validation(
                "parent_id",
                "a folder cannot be its own parent",
            ));
        }
        if fetch_folder(pool, parent).await?.is_none() {
            return Err(StoreError::not_found("folder", parent));
        }
        // Cycle check: is `parent` inside `id`'s subtree?
        let descendant: i64 = sqlx::query_scalar(
            "WITH RECURSIVE descendants(id) AS (
                 SELECT id FROM folders_tbl WHERE parent_id = ?
                 UNION ALL
                 SELECT f.id FROM folders_tbl f
                 JOIN descendants d ON f.parent_id = d.id
             )
             SELECT COUNT(*) FROM descendants WHERE id = ?",
        )
        .bind(id.as_str())
        .bind(parent.as_str())
        .fetch_one(pool)
        .await?;
        if descendant > 0 {
            return Err(StoreError::validation(
                "parent_id",
                "cannot move a folder into its own subtree",
            ));
        }
    }

    sqlx::query("UPDATE folders_tbl SET parent_id = ?, updated_at = ? WHERE id = ?")
        .bind(new_parent.map(|p| p.as_str()))
        .bind(now_rfc3339())
        .bind(id.as_str())
        .execute(pool)
        .await?;

    fetch_folder(pool, id)
        .await?
        .ok_or_else(|| StoreError::not_found("folder", id))
}

/// Deletes a folder. `ON DELETE CASCADE` removes its descendant folders;
/// documents inside any deleted folder are kept and moved to the root
/// (`folder_id` is `ON DELETE SET NULL`). Returns `true` if a row was
/// removed.
pub async fn delete_folder(pool: &SqlitePool, id: &FolderId) -> StoreResult<bool> {
    let done = sqlx::query("DELETE FROM folders_tbl WHERE id = ?")
        .bind(id.as_str())
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::document_engine::document::{DocumentOrigin, DocumentSource, DocumentType};
    use crate::crates::document_engine::ids::DocumentId;
    use crate::crates::document_engine::store::documents::{
        NewDocument, create_document, get_document, move_document,
    };
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

    #[tokio::test]
    async fn create_root_and_nested_folders() {
        let pool = test_pool().await;

        let root = create_folder(&pool, "  Projects  ", None, "user-1")
            .await
            .expect("create root");
        // Name is trimmed before storage.
        assert_eq!(root.name, "Projects");
        assert_eq!(root.parent_id, None);
        assert_eq!(root.owner_id, "user-1");
        assert!(root.id.as_str().starts_with("fld-"));
        assert_eq!(root.created_at, root.updated_at);

        let child = create_folder(&pool, "2024-Q1", Some(&root.id), "user-1")
            .await
            .expect("create child");
        assert_eq!(child.parent_id, Some(root.id.clone()));

        let fetched = get_folder(&pool, &child.id)
            .await
            .expect("get")
            .expect("child exists");
        assert_eq!(fetched.name, "2024-Q1");
        assert_eq!(fetched.parent_id, Some(root.id.clone()));

        assert!(
            get_folder(&pool, &FolderId::new("fld-missing"))
                .await
                .expect("get")
                .is_none()
        );
    }

    #[tokio::test]
    async fn create_folder_validates_name_and_parent() {
        let pool = test_pool().await;

        for bad in ["", "   "] {
            let err = create_folder(&pool, bad, None, "user-1")
                .await
                .expect_err("empty name must fail");
            assert!(matches!(err, StoreError::Validation { .. }));
        }

        let err = create_folder(
            &pool,
            "Orphan",
            Some(&FolderId::new("fld-missing")),
            "user-1",
        )
        .await
        .expect_err("missing parent must fail");
        assert!(matches!(err, StoreError::NotFound { .. }));
    }

    #[tokio::test]
    async fn list_folders_filters_by_owner_and_parent() {
        let pool = test_pool().await;

        let a = create_folder(&pool, "Alpha", None, "user-1")
            .await
            .expect("a");
        let b = create_folder(&pool, "Beta", None, "user-2")
            .await
            .expect("b");
        let a_child = create_folder(&pool, "A-Child", Some(&a.id), "user-1")
            .await
            .expect("a child");
        let b_child = create_folder(&pool, "B-Child", Some(&b.id), "user-2")
            .await
            .expect("b child");

        // parent_id = None lists only roots, ordered by name.
        let roots = list_folders(&pool, None, None).await.expect("roots");
        let root_names: Vec<&str> = roots.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(root_names, ["Alpha", "Beta"]);

        // Owner filter applies on top of the parent filter.
        let user1_roots = list_folders(&pool, Some("user-1"), None)
            .await
            .expect("user1 roots");
        assert_eq!(user1_roots.len(), 1);
        assert_eq!(user1_roots[0].id, a.id);

        // Direct children of a specific folder.
        let a_children = list_folders(&pool, None, Some(&a.id))
            .await
            .expect("a children");
        assert_eq!(a_children.len(), 1);
        assert_eq!(a_children[0].id, a_child.id);

        let b_children = list_folders(&pool, None, Some(&b.id))
            .await
            .expect("b children");
        assert_eq!(b_children[0].id, b_child.id);

        // A leaf folder has no children.
        assert!(
            list_folders(&pool, None, Some(&a_child.id))
                .await
                .expect("leaf")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn rename_folder_updates_name() {
        let pool = test_pool().await;
        let folder = create_folder(&pool, "Old", None, "user-1")
            .await
            .expect("create");

        let renamed = rename_folder(&pool, &folder.id, "  New Name  ")
            .await
            .expect("rename");
        assert_eq!(renamed.name, "New Name");
        assert!(renamed.updated_at >= folder.updated_at);

        let err = rename_folder(&pool, &folder.id, "   ")
            .await
            .expect_err("blank name must fail");
        assert!(matches!(err, StoreError::Validation { .. }));

        let err = rename_folder(&pool, &FolderId::new("fld-missing"), "X")
            .await
            .expect_err("missing folder must fail");
        assert!(matches!(err, StoreError::NotFound { .. }));
    }

    #[tokio::test]
    async fn move_folder_reparents_and_rejects_cycles() {
        let pool = test_pool().await;
        let a = create_folder(&pool, "A", None, "user-1").await.expect("a");
        let b = create_folder(&pool, "B", Some(&a.id), "user-1")
            .await
            .expect("b");
        let c = create_folder(&pool, "C", Some(&b.id), "user-1")
            .await
            .expect("c");
        let other = create_folder(&pool, "Other", None, "user-1")
            .await
            .expect("other");

        // Move C to the root.
        let moved = move_folder(&pool, &c.id, None).await.expect("move to root");
        assert_eq!(moved.parent_id, None);

        // Move C under `other`.
        let moved = move_folder(&pool, &c.id, Some(&other.id))
            .await
            .expect("move under other");
        assert_eq!(moved.parent_id, Some(other.id.clone()));

        // A folder cannot be its own parent.
        let err = move_folder(&pool, &a.id, Some(&a.id))
            .await
            .expect_err("self-parent must fail");
        assert!(matches!(err, StoreError::Validation { .. }));

        // Cannot move A into its own descendant B (chain A -> B -> ...).
        let err = move_folder(&pool, &a.id, Some(&b.id))
            .await
            .expect_err("descendant parent must fail");
        assert!(matches!(err, StoreError::Validation { .. }));

        // Rebuild the chain A -> B -> C, then a deeper cycle attempt:
        // moving A under C must fail because C sits inside A's subtree.
        move_folder(&pool, &c.id, Some(&b.id))
            .await
            .expect("re-nest C under B");
        let err = move_folder(&pool, &a.id, Some(&c.id))
            .await
            .expect_err("moving A under grandchild C must fail");
        assert!(matches!(err, StoreError::Validation { .. }));

        // Missing parent and missing folder.
        let err = move_folder(&pool, &a.id, Some(&FolderId::new("fld-missing")))
            .await
            .expect_err("missing parent must fail");
        assert!(matches!(err, StoreError::NotFound { .. }));
        let err = move_folder(&pool, &FolderId::new("fld-missing"), None)
            .await
            .expect_err("missing folder must fail");
        assert!(matches!(err, StoreError::NotFound { .. }));
    }

    #[tokio::test]
    async fn delete_folder_cascades_subtree_and_unsets_documents() {
        let pool = test_pool().await;
        let root = create_folder(&pool, "Root", None, "user-1")
            .await
            .expect("root");
        let child = create_folder(&pool, "Child", Some(&root.id), "user-1")
            .await
            .expect("child");
        let grandchild = create_folder(&pool, "Grandchild", Some(&child.id), "user-1")
            .await
            .expect("grandchild");

        // A document inside the subtree is kept but moved to the root.
        let doc_id = new_doc(&pool, "in-folder.pdf").await;
        move_document(&pool, &doc_id, Some(&grandchild.id))
            .await
            .expect("move doc");

        assert!(delete_folder(&pool, &root.id).await.expect("delete root"));

        for id in [&root.id, &child.id, &grandchild.id] {
            assert!(
                get_folder(&pool, id).await.expect("get").is_none(),
                "folder {id} should be cascade-deleted"
            );
        }
        let doc = get_document(&pool, &doc_id)
            .await
            .expect("get doc")
            .expect("doc kept");
        assert_eq!(doc.folder_id, None);

        // Deleting an already-removed folder reports false.
        assert!(!delete_folder(&pool, &root.id).await.expect("delete again"));
    }
}
