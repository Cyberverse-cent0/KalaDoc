//! Base64 helpers so [`PipelineArtifact`](crate::crates::pipeline_engine::PipelineArtifact)
//! byte payloads encode as compact strings in JSON output.
//!
//! `serde`'s `with` attribute cannot annotate the `Vec<u8>` nested
//! inside `Vec<Vec<u8>>`, so `PipelineArtifact` implements
//! `Serialize`/`Deserialize` manually and shares this module's codec.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// Encode bytes as a base64 string.
pub(crate) fn encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Decode a base64 string into bytes.
pub(crate) fn decode(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    STANDARD.decode(s)
}
