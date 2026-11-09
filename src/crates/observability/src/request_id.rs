//! Request correlation IDs.
//!
//! [`RequestId`] is a validated UUID v4 string used to correlate log
//! lines, errors, and (later) job payloads for a single request. The
//! crate does not depend on the `uuid` crate; IDs are generated with
//! `rand_core::OsRng` and formatted/validated by hand.

use std::fmt;

use rand_core::RngCore;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A request correlation ID: a lowercase UUID v4 string.
///
/// Construct with [`RequestId::new`] (fresh random ID) or
/// [`RequestId::parse`] (validate an incoming string). IDs serialize
/// as a plain JSON string.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RequestId(String);

impl RequestId {
    /// Generate a fresh random UUID v4 request ID.
    pub fn new() -> Self {
        let mut bytes = [0u8; 16];
        rand_core::OsRng.fill_bytes(&mut bytes);
        // RFC 4122: version 4 in the high nibble of byte 6,
        // variant 10 in the high bits of byte 8.
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Self(format_uuid(&bytes))
    }

    /// Validate `s` as a UUID string (8-4-4-4-12 hex with hyphens) and
    /// wrap it. Returns `None` for any malformed input. Any UUID
    /// version is accepted — callers may receive IDs minted elsewhere.
    pub fn parse(s: &str) -> Option<Self> {
        let b = s.as_bytes();
        if b.len() != 36 {
            return None;
        }
        for (i, &c) in b.iter().enumerate() {
            match i {
                8 | 13 | 18 | 23 => {
                    if c != b'-' {
                        return None;
                    }
                }
                _ => {
                    if !c.is_ascii_hexdigit() {
                        return None;
                    }
                }
            }
        }
        Some(Self(s.to_string()))
    }

    /// The UUID string form.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RequestId({})", self.0)
    }
}

/// Format 16 bytes as a lowercase hyphenated UUID string.
fn format_uuid(bytes: &[u8; 16]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(36);
    for (i, byte) in bytes.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        let _ = write!(out, "{byte:02x}");
    }
    out
}

impl Serialize for RequestId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RequestId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).ok_or_else(|| serde::de::Error::custom("invalid request id"))
    }
}

/// Return a tracing field value for recording the ID on a span or
/// event. `RequestId::as_str()` also implements `tracing::Value`
/// directly, so `request_id = id.as_str()` works in `info_span!` /
/// `info!` macros without this helper.
///
/// ```ignore
/// use kaladoc::crates::observability::request_id;
/// let id = request_id::RequestId::new();
/// tracing::info_span!("request", request_id = request_id::field(&id));
/// ```
pub fn field(id: &RequestId) -> tracing::field::DisplayValue<&str> {
    tracing::field::display(id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_produces_parseable_uuid_v4() {
        let id = RequestId::new();
        let s = id.as_str();
        assert_eq!(s.len(), 36);
        assert_eq!(s.chars().nth(14), Some('4'), "version nibble");
        assert!(matches!(s.chars().nth(19), Some('8' | '9' | 'a' | 'b')));
        assert_eq!(RequestId::parse(s), Some(id.clone()));
    }

    #[test]
    fn new_ids_are_unique() {
        assert_ne!(RequestId::new(), RequestId::new());
    }

    #[test]
    fn parse_rejects_malformed_input() {
        for bad in [
            "",
            "not-a-uuid",
            "550e8400-e29b-41d4-a716-44665544000", // too short
            "550e8400-e29b-41d4-a716-4466554400000", // too long
            "550e8400-e29b-41d4-a716-44665544000g", // non-hex
            "550e8400e29b41d4a716446655440000",    // no hyphens
            "550e8400-e29b-41d4-a716_446655440000", // wrong separator
        ] {
            assert_eq!(RequestId::parse(bad), None, "accepted {bad:?}");
        }
    }

    #[test]
    fn parse_accepts_uppercase_and_any_version() {
        let upper = "550E8400-E29B-41D4-A716-446655440000";
        assert!(RequestId::parse(upper).is_some());
        // v1-shaped ID still parses — shape, not version, is validated.
        assert!(RequestId::parse("550e8400-e29b-11d4-a716-446655440000").is_some());
    }

    #[test]
    fn display_and_serde_roundtrip() {
        let id = RequestId::new();
        assert_eq!(id.to_string(), id.as_str());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{}\"", id.as_str()));
        let back: RequestId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn serde_rejects_invalid_string() {
        assert!(serde_json::from_str::<RequestId>("\"bogus\"").is_err());
    }
}
