//! Remote storage backend (planned).
//!
//! Will hold object-store / networked storage implementations (S3,
//! GCS, WebDAV, ...) behind the same interface as
//! [`crate::crates::storage::local`].
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
//! A remote backend should provide its own health-check / retry
//! policy instead of [`FaultInjectingFs`]'s simulated faults.
//!
//! [`StorageResult`]: crate::crates::storage::local::debug_when_full::StorageResult
//! [`StorageError`]: crate::crates::storage::local::debug_when_full::StorageError
//! [`FaultInjectingFs`]: crate::crates::storage::local::debug_when_full::FaultInjectingFs
