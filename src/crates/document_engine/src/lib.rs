//! Document Engine — core domain objects for KalaDoc.
//!
//! This module defines the primary domain types for documents: identity,
//! artifacts, versions, metadata, organization (folders/tags), lifecycle,
//! permissions, and audit events. These are pure domain types; persistence
//! and authorization decisions live in other crates/services.

pub mod artifact;
pub mod audit;
pub mod document;
pub mod error;
pub mod ids;
pub mod lifecycle;
pub mod metadata;
pub mod organization;
pub mod relationship;
pub mod store;
pub mod version;
