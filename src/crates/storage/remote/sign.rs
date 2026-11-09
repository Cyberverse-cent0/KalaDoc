//! AWS Signature Version 4 request signing.
//!
//! Minimal, dependency-free implementation covering exactly what the
//! remote backend needs: `host`, `x-amz-date`, and
//! `x-amz-content-sha256` as signed headers, path-style S3 URLs, and
//! an optional query string. HMAC-SHA256 is built on top of the
//! workspace's existing `sha2` dependency.

use sha2::{Digest, Sha256};

/// Signs outgoing requests with AWS Signature Version 4.
#[derive(Debug, Clone)]
pub(crate) struct SigV4Signer {
    access_key: String,
    secret_key: String,
    region: String,
}

impl SigV4Signer {
    pub(crate) fn new(access_key: &str, secret_key: &str, region: &str) -> Self {
        Self {
            access_key: access_key.to_string(),
            secret_key: secret_key.to_string(),
            region: region.to_string(),
        }
    }

    /// Signs `method`/`url` (which already carries the percent-encoded
    /// canonical path and optional `?query`) with `payload` as the body.
    ///
    /// Adds `x-amz-date` and `x-amz-content-sha256` headers to `headers`
    /// and returns the `Authorization` header value. The caller must
    /// send exactly the headers present afterwards — the signed header
    /// set is `host;x-amz-content-sha256;x-amz-date`.
    pub(crate) fn sign(
        &self,
        method: &str,
        url: &str,
        headers: &mut Vec<(String, String)>,
        payload: &[u8],
    ) -> String {
        let (amz_date, date_stamp) = amz_timestamp();
        let payload_hash = sha256_hex(payload);

        headers.retain(|(k, _)| {
            !k.eq_ignore_ascii_case("x-amz-date")
                && !k.eq_ignore_ascii_case("x-amz-content-sha256")
                && !k.eq_ignore_ascii_case("authorization")
        });
        headers.push(("x-amz-date".to_string(), amz_date.clone()));
        headers.push((
            "x-amz-content-sha256".to_string(),
            payload_hash.clone(),
        ));

        let (host, canonical_uri, canonical_query) = split_url(url);
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";

        // Canonical headers: sorted, trimmed, lowercased names.
        let mut canonical: Vec<(String, String)> = vec![
            ("host".to_string(), host.clone()),
            ("x-amz-content-sha256".to_string(), payload_hash.clone()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        canonical.sort_by(|a, b| a.0.cmp(&b.0));
        let canonical_headers: String = canonical
            .iter()
            .map(|(k, v)| format!("{k}:{v}\n"))
            .collect();

        let canonical_request = format!(
            "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
        );

        let scope = format!("{date_stamp}/{}/{}/aws4_request", self.region, "s3");
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );

        let k_date = hmac_sha256(
            format!("AWS4{}", self.secret_key).as_bytes(),
            date_stamp.as_bytes(),
        );
        let k_region = hmac_sha256(&k_date, self.region.as_bytes());
        let k_service = hmac_sha256(&k_region, b"s3");
        let k_signing = hmac_sha256(&k_service, b"aws4_request");
        let signature = hex_encode(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));

        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.access_key
        )
    }
}

/// Splits a URL into `(host_header, canonical_uri, canonical_query)`.
///
/// `url` must already be percent-encoded (the backend encodes object
/// keys before signing). The canonical URI keeps the encoding as-is —
/// S3 expects single encoding. Query pairs are sorted by encoded key
/// per SigV4 rules.
fn split_url(url: &str) -> (String, String, String) {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let (authority, path_query) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let host = authority.to_string();

    let (path, query) = match path_query.find('?') {
        Some(i) => (&path_query[..i], &path_query[i + 1..]),
        None => (path_query, ""),
    };
    let canonical_uri = if path.is_empty() { "/" } else { path }.to_string();

    let canonical_query = if query.is_empty() {
        String::new()
    } else {
        let mut pairs: Vec<&str> = query.split('&').filter(|p| !p.is_empty()).collect();
        pairs.sort_unstable();
        pairs.join("&")
    };

    (host, canonical_uri, canonical_query)
}

/// Percent-encodes one URL path segment per RFC 3986 unreserved set.
/// `/` is not allowed inside a segment — callers split on it first.
pub(crate) fn encode_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for &b in seg.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn sha256_hex(data: &[u8]) -> String {
    hex_encode(&Sha256::digest(data))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xF) as usize] as char);
    }
    s
}

/// HMAC-SHA256 (RFC 2104) built on `sha2` — avoids pulling in the
/// `hmac` crate for a single use site.
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        let h = Sha256::digest(key);
        k[..32].copy_from_slice(&h);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Vec::with_capacity(64 + msg.len());
    inner.extend(k.iter().map(|b| b ^ 0x36));
    inner.extend_from_slice(msg);
    let inner_hash = Sha256::digest(&inner);

    let mut outer = Vec::with_capacity(64 + 32);
    outer.extend(k.iter().map(|b| b ^ 0x5c));
    outer.extend_from_slice(&inner_hash);
    Sha256::digest(&outer).into()
}

/// Returns `(amz_date, date_stamp)` for the current UTC time, e.g.
/// `("20261109T120000Z", "20261109")`. Implemented without chrono —
/// the civil-from-days algorithm keeps this dependency-free.
fn amz_timestamp() -> (String, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;

    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }

    let hh = rem / 3_600;
    let mm = (rem % 3_600) / 60;
    let ss = rem % 60;
    let date_stamp = format!("{y:04}{m:02}{d:02}");
    let amz_date = format!("{date_stamp}T{hh:02}{mm:02}{ss:02}Z");
    (amz_date, date_stamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        // RFC 4231 test case 2: key = "Jefe", data = "what do ya want for nothing?"
        let out = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex_encode(&out),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn sha256_hex_known() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn encode_segment_keeps_unreserved() {
        assert_eq!(encode_segment("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(encode_segment("a b/c"), "a%20b%2Fc");
        assert_eq!(encode_segment("100%"), "100%25");
    }

    #[test]
    fn split_url_extracts_host_and_sorts_query() {
        let (host, uri, query) =
            split_url("https://s3.example.com:9000/bucket/prefix/k?b=2&a=1");
        assert_eq!(host, "s3.example.com:9000");
        assert_eq!(uri, "/bucket/prefix/k");
        assert_eq!(query, "a=1&b=2");
    }

    #[test]
    fn sign_produces_authorization_header() {
        let signer = SigV4Signer::new("AKID", "secret", "us-east-1");
        let mut headers = vec![("host".to_string(), "s3.example.com".to_string())];
        let auth = signer.sign(
            "PUT",
            "https://s3.example.com/bucket/key",
            &mut headers,
            b"hello",
        );
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKID/"));
        assert!(auth.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date"));
        assert!(headers.iter().any(|(k, _)| k == "x-amz-date"));
        assert!(headers
            .iter()
            .any(|(k, v)| k == "x-amz-content-sha256" && v.len() == 64));
    }
}
