//! Document-level authorization — the allow/deny decisions the HTTP
//! API (and any future adapter) applies on top of
//! `document_engine::store::permissions`.
//!
//! # Access model
//!
//! Every authenticated principal is an [`crate::crates::api::auth::AuthUser`]
//! whose `actor_id`/`owner_id` is either a session user id (`"42"`) or a
//! static API-key actor (`"apikey:<name>"`). There is **no admin role
//! and no wildcard**: a principal may act on a document when
//!
//! 1. `document.owner_id == actor_id` — ownership grants every level
//!    (implicit `Manage`), or
//! 2. an active row in `document_permissions_tbl` names the actor as
//!    `subject_id` with an `access_level` at least the required one
//!    (`read` < `write` < `manage`; see
//!    [`DocumentAccessLevel`](crate::crates::document_engine::lifecycle::DocumentAccessLevel)).
//!
//! API-key actors follow exactly the same rules — they own the
//! documents they upload (`owner_id = "apikey:<name>"`) and can be
//! granted explicit permission rows like any other subject. The CLI
//! and other in-process callers bypass this layer entirely by design:
//! they hold a [`DocumentService`] and call the store directly.
//!
//! Folders have no permission table; folder mutations require
//! `folder.owner_id == caller` (see [`check_folder_access`]).
//!
//! Denials distinguish `NotFound` (no such document → `404`) from
//! `Forbidden` (document exists but the caller lacks the level →
//! `403`). `403` is deliberate: ids are unguessable (`doc-…` random
//! ids), and hiding existence would make legitimate "shared with me"
//! workflows harder to debug.

use sqlx::SqlitePool;

use crate::crates::document_engine::document::Document;
use crate::crates::document_engine::ids::{DocumentId, FolderId};
use crate::crates::document_engine::lifecycle::DocumentAccessLevel;
use crate::crates::document_engine::organization::Folder;
use crate::crates::document_engine::store::{documents, folders, permissions};

use super::error::ServiceError;

/// Outcome of [`check_document_access`].
#[derive(Debug)]
pub enum AccessCheck {
    /// Access granted; carries the already-loaded document so callers
    /// don't fetch it twice.
    Allow(Document),
    /// No document with this id exists → map to `404`.
    NotFound,
    /// The document exists but the actor lacks `required` → `403`.
    Forbidden,
}

/// Decides whether `actor_id` may exercise `required` on the document.
///
/// `actor_id` is the caller's actor identity — the same string used as
/// `owner_id`/`actor_id` on [`AuthUser`](crate::crates::api::auth::AuthUser)
/// and as `subject_id` in permission grants. Ownership short-circuits
/// before any permission lookup.
///
/// # Errors
/// [`ServiceError::Store`] on database failures.
pub async fn check_document_access(
    pool: &SqlitePool,
    document_id: &DocumentId,
    actor_id: &str,
    required: DocumentAccessLevel,
) -> Result<AccessCheck, ServiceError> {
    let Some(document) = documents::get_document(pool, document_id).await? else {
        return Ok(AccessCheck::NotFound);
    };

    // Ownership is implicit full access.
    if document.owner_id == actor_id {
        return Ok(AccessCheck::Allow(document));
    }

    let grants = permissions::get_document_permissions(pool, document_id).await?;
    let granted = grants
        .iter()
        .find(|p| p.subject_id == actor_id)
        .map(|p| p.access_level);

    match granted {
        Some(level) if level >= required => Ok(AccessCheck::Allow(document)),
        _ => Ok(AccessCheck::Forbidden),
    }
}

/// Folder counterpart of [`check_document_access`]: `Some(folder)`
/// when the caller owns it, `None` when it is missing **or** owned by
/// someone else (folders have no share mechanism, so both cases map to
/// `404` — there is nothing to enumerate).
///
/// # Errors
/// [`ServiceError::Store`] on database failures.
pub async fn check_folder_access(
    pool: &SqlitePool,
    folder_id: &FolderId,
    owner_id: &str,
) -> Result<Option<Folder>, ServiceError> {
    let Some(folder) = folders::get_folder(pool, folder_id).await? else {
        return Ok(None);
    };
    Ok((folder.owner_id == owner_id).then_some(folder))
}

/// Document ids the actor can reach through explicit permission rows —
/// used to union "shared with me" documents into owner-scoped listings.
/// Callers still run [`check_document_access`] per id before trusting
/// a row (grants may target any subject string, and read-level grants
/// don't imply write).
///
/// # Errors
/// [`ServiceError::Store`] on database failures.
pub async fn granted_document_ids(
    pool: &SqlitePool,
    actor_id: &str,
) -> Result<Vec<DocumentId>, ServiceError> {
    let mut ids = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for grant in permissions::list_permissions_for_subject(pool, actor_id).await? {
        if seen.insert(grant.document_id.clone()) {
            ids.push(grant.document_id);
        }
    }
    Ok(ids)
}

/// Whether `actor_id` looks like a static API-key principal
/// (`apikey:<name>`). Currently informational — API keys follow the
/// same owner/permission rules as sessions — but handlers and future
/// policy code can branch on it (e.g. restricting schedules to key
/// actors for automation).
pub fn is_api_key_actor(actor_id: &str) -> bool {
    actor_id.starts_with("apikey:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::document_engine::document::{DocumentOrigin, DocumentSource, DocumentType};
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

    async fn new_doc(pool: &SqlitePool, owner: &str) -> DocumentId {
        documents::create_document(
            pool,
            &crate::crates::document_engine::store::documents::NewDocument {
                name: "doc.pdf".to_string(),
                document_type: DocumentType::Pdf,
                owner_id: owner.to_string(),
                source: DocumentSource::new(DocumentOrigin::Upload),
            },
        )
        .await
        .expect("create document")
        .id
    }

    #[tokio::test]
    async fn owner_is_allowed_at_every_level() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "user-1").await;
        for level in [
            DocumentAccessLevel::Read,
            DocumentAccessLevel::Write,
            DocumentAccessLevel::Manage,
        ] {
            assert!(matches!(
                check_document_access(&pool, &doc_id, "user-1", level)
                    .await
                    .unwrap(),
                AccessCheck::Allow(_)
            ));
        }
    }

    #[tokio::test]
    async fn permission_levels_gate_required_access() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "user-1").await;
        permissions::grant_permission(
            &pool,
            &doc_id,
            "user-2",
            DocumentAccessLevel::Read,
            Some("user-1"),
        )
        .await
        .expect("grant read");

        assert!(matches!(
            check_document_access(&pool, &doc_id, "user-2", DocumentAccessLevel::Read)
                .await
                .unwrap(),
            AccessCheck::Allow(_)
        ));
        // Read grant does not satisfy write/manage.
        assert!(matches!(
            check_document_access(&pool, &doc_id, "user-2", DocumentAccessLevel::Write)
                .await
                .unwrap(),
            AccessCheck::Forbidden
        ));
        assert!(matches!(
            check_document_access(&pool, &doc_id, "user-2", DocumentAccessLevel::Manage)
                .await
                .unwrap(),
            AccessCheck::Forbidden
        ));
    }

    #[tokio::test]
    async fn stranger_is_forbidden_and_missing_is_not_found() {
        let pool = test_pool().await;
        let doc_id = new_doc(&pool, "user-1").await;

        assert!(matches!(
            check_document_access(&pool, &doc_id, "user-9", DocumentAccessLevel::Read)
                .await
                .unwrap(),
            AccessCheck::Forbidden
        ));
        assert!(matches!(
            check_document_access(
                &pool,
                &DocumentId::new("doc-missing"),
                "user-1",
                DocumentAccessLevel::Read,
            )
            .await
            .unwrap(),
            AccessCheck::NotFound
        ));
    }

    #[tokio::test]
    async fn folder_access_is_owner_only() {
        let pool = test_pool().await;
        let folder = folders::create_folder(&pool, "inbox", None, "user-1")
            .await
            .expect("create folder");

        assert!(
            check_folder_access(&pool, &folder.id, "user-1")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            check_folder_access(&pool, &folder.id, "user-2")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            check_folder_access(&pool, &FolderId::new("fld-missing"), "user-1")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn granted_document_ids_lists_subject_grants() {
        let pool = test_pool().await;
        let a = new_doc(&pool, "user-1").await;
        let b = new_doc(&pool, "user-1").await;
        permissions::grant_permission(&pool, &a, "user-2", DocumentAccessLevel::Read, None)
            .await
            .expect("grant a");
        permissions::grant_permission(&pool, &b, "user-2", DocumentAccessLevel::Write, None)
            .await
            .expect("grant b");
        permissions::grant_permission(&pool, &a, "user-3", DocumentAccessLevel::Read, None)
            .await
            .expect("grant a to other");

        let ids = granted_document_ids(&pool, "user-2").await.unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&a) && ids.contains(&b));
        assert!(
            granted_document_ids(&pool, "user-9")
                .await
                .unwrap()
                .is_empty()
        );
    }
}
