//! Deployment and release packaging modules.
//!
//! - [`require`] — OS/distro detection and dependency checks.
//! - [`package`] — per-target staging and `.tar.gz`/`.zip` archives.
//! - [`release`] — build orchestration, checksums, manifest, publish.
//! - [`arch`] — per-OS generated assets (install scripts).
//! - [`archive`] — dependency-free ZIP writer.
//!
//! Container deployment lives outside this module as repo-root
//! artifacts: [`Dockerfile`](../../Dockerfile),
//! [`docker-compose.yml`](../../docker-compose.yml), and the
//! [`deploy/`](../../deploy/) directory (Kubernetes manifests + docs).

pub mod arch;
pub mod archive;
pub mod package;
pub mod release;
pub mod require;
