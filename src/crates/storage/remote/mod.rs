//! Remote storage backend: S3-compatible object stores behind
//! [`StorageBackend`].
//!
//! See [`backend::RemoteBackend`] for the implementation and its
//! design notes. In short:
//!
//! - **Protocol**: S3 path-style URLs (`{endpoint}/{bucket}/{key}`)
//!   signed with AWS Signature Version 4 — covers AWS S3 and
//!   S3-compatible stores (MinIO, Ceph RGW, Garage, LocalStack).
//! - **Transport**: synchronous `ureq` (rustls) behind the
//!   `transport::HttpTransport` trait object, so tests inject a mock
//!   and never touch the network. Matches the synchronous trait.
//! - **Configuration**: [`config::RemoteStorageConfig`], populated
//!   from `KALADOC_REMOTE_*` environment variables via
//!   [`config::RemoteStorageConfig::from_env`] — credentials are never
//!   hardcoded.
//!
//! Any implementation here must satisfy
//! [`crate::crates::storage::backend::StorageBackend`]:
//!
//! - `write` / `read` / `delete` / `exists` operate on relative names
//!   confined to the backend's configured root (bucket + prefix), with
//!   the same [`StorageResult`]/[`StorageError`] contract as the local
//!   backend — including `NotFound`, `QuotaExceeded`, and
//!   `PathOutsideRoot` semantics where they apply.
//! - `used_bytes` reports usage when the backend can account for it
//!   (object stores typically can't cheaply, so `None` is acceptable
//!   unless a quota is enforced client-side).
//! - `prepare` performs one-time, idempotent provisioning: verifying
//!   credentials/connectivity and ensuring the target bucket/prefix
//!   exists.
//!
//! Device-level concerns from the local backend — probing a target
//! ([`crate::crates::storage::local::create_fs::create_fs_storage`]),
//! formatting disks, and deterministic fault injection — have no
//! remote analogue today and are intentionally not part of the trait.
//! [`RemoteBackend`](backend::RemoteBackend) provides its own retry
//! policy (bounded exponential backoff on transient failures) instead
//! of [`FaultInjectingFs`]'s simulated faults.
//!
//! [`StorageResult`]: crate::crates::storage::local::debug_when_full::StorageResult
//! [`StorageError`]: crate::crates::storage::local::debug_when_full::StorageError
//! [`FaultInjectingFs`]: crate::crates::storage::local::debug_when_full::FaultInjectingFs

pub mod backend;
pub mod config;
mod sign;
mod transport;

pub use backend::RemoteBackend;
pub use config::RemoteStorageConfig;
