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
    let row = sqlx::query(&format!("SELECT {FOLDER_COLS} FROM folders_tbl WHERE id = ?"))
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
    if let Some(parent) = parent_id {
        if fetch_folder(pool, parent).await?.is_none() {
            return Err(StoreError::not_found("folder", parent));
        }
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
    let done = sqlx::query(
        "UPDATE folders_tbl SET name = ?, updated_at = ? WHERE id = ?",
    )
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
