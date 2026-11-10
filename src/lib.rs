// The OpenAPI spec in `crates::api::openapi` is a single large `json!`
// literal that exceeds the default macro-expansion recursion limit.
#![recursion_limit = "1024"]

pub mod config;
pub mod core;
pub mod crates;
pub mod deployments;
