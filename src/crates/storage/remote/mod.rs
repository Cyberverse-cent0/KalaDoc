//! Remote storage backends.
//!
//! Holds networked/object-store storage implementations behind the
//! same [`crate::crates::storage::backend::StorageBackend`] interface
//! as [`crate::crates::storage::local`].
//!
//! ## Implemented: [`s3::S3Backend`]
//!
//! An S3-compatible backend (AWS S3, MinIO, R2, GCS S3-interop, ...)
//! using path-style URLs and AWS Signature Version 4 auth. It is
//! configured exclusively through environment variables — see
//! [`config::RemoteStorageConfig`] for the `KALADOC_S3_*` variables;
//! credentials are never read from config files.
//!
//! Contract notes specific to this backend:
//!
//! - `write` / `read` / `delete` / `exists` operate on relative names
//!   confined to the configured bucket + prefix. `..` components,
//!   absolute paths, NUL bytes, and empty/`.`-only names fail with
//!   [`StorageError::PathOutsideRoot`]; backslashes are treated as
//!   path separators (Windows semantics).
//! - `read` of a missing object returns [`StorageError::NotFound`];
//!   other HTTP/transport failures map to [`StorageError::Io`].
//! - `used_bytes` returns `None` unless `max_size_bytes` is
//!   configured, in which case an in-memory counter is maintained
//!   (object stores can't cheaply report total usage; the counter may
//!   drift if objects change out-of-band).
//! - `prepare` issues `HEAD` on the bucket — an idempotent
//!   credential/connectivity check that is a no-op once it has
//!   succeeded. It does *not* create the bucket; the bucket must
//!   already exist.
//! - Transient failures (transport errors, HTTP `5xx`) are retried
//!   with exponential backoff + jitter per [`config::RetryPolicy`];
//!   `4xx` responses are never retried.
//!
//! Device-level concerns from the local backend — probing a target
//! ([`crate::crates::storage::local::create_fs::create_fs_storage`]),
//! formatting disks, and deterministic fault injection — have no
//! remote analogue and are intentionally not part of the trait. The
//! remote backend substitutes its own retry/timeout policy (see
//! [`config::RemoteStorageConfig::timeout`] and
//! [`config::RemoteStorageConfig::retry`]).
//!
//! [`StorageError::PathOutsideRoot`]: crate::crates::storage::local::debug_when_full::StorageError::PathOutsideRoot
//! [`StorageError::NotFound`]: crate::crates::storage::local::debug_when_full::StorageError::NotFound
//! [`StorageError::Io`]: crate::crates::storage::local::debug_when_full::StorageError::Io

pub mod config;
pub mod s3;

pub use config::{RemoteStorageConfig, RetryPolicy};
pub use s3::{HttpRequest, HttpResponse, S3Backend, Transport, TransportError, UreqTransport};
