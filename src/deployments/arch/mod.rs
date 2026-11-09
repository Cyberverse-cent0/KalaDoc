//! Per-OS release assets.
//!
//! [`init`] provides the generated files that differ per target —
//! currently install scripts (`install.sh` for unix-likes,
//! `install.ps1` for Windows) dropped at the package root.

pub mod init;
