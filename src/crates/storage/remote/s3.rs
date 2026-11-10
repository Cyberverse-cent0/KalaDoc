//! S3-compatible remote storage backend.
//!
//! [`S3Backend`] implements [`StorageBackend`] on top of any
//! S3-compatible object store (AWS S3, MinIO, Cloudflare R2, GCS
//! S3-interop, ...) using path-style URLs and AWS Signature Version 4
//! authentication.
//!
//! - Object names are confined to `bucket` + `prefix`: `..`
//!   components, absolute paths, NUL bytes, and empty/`.`-only names
//!   are rejected with [`StorageError::PathOutsideRoot`]. Backslashes
//!   are treated as path separators (matching Windows semantics).
//! - `write`/`read`/`delete`/`exists` map to `PUT`/`GET`/`DELETE`/`HEAD`
//!   object requests; HTTP failures become [`StorageError::Io`] and a
//!   missing object on `read` becomes [`StorageError::NotFound`].
//! - `used_bytes` reports `Some` only when `max_size_bytes` is
//!   configured (client-side accounting); otherwise `None`.
//! - `prepare` performs a `HEAD` on the bucket — an idempotent
//!   credential/connectivity check that is a no-op once it has
//!   succeeded.
//! - Transient failures (transport errors, HTTP `5xx`) are retried
//!   with exponential backoff + jitter per [`RetryPolicy`]; `4xx`
//!   responses are never retried.
//!
//! All network access goes through the [`Transport`] trait so tests
//! can inject a mock — the default [`UreqTransport`] uses `ureq`
//! (blocking, rustls).

use std::fmt;
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use super::config::{RemoteStorageConfig, RetryPolicy};
use crate::crates::storage::backend::StorageBackend;
use crate::crates::storage::local::debug_when_full::{StorageError, StorageResult};

// ---------------------------------------------------------------------------
// Transport abstraction
// ---------------------------------------------------------------------------

/// A single HTTP request handed to a [`Transport`].
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    /// Absolute URL including scheme, authority, and path.
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// A single HTTP response returned by a [`Transport`].
#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Transport-level failure (DNS, connect, TLS, timeout, reset, ...).
///
/// All variants are treated as transient and retried per the backend's
/// [`RetryPolicy`]; HTTP status codes are *not* transport errors.
#[derive(Debug)]
pub enum TransportError {
    Io(io::Error),
    Other(String),
}

impl TransportError {
    fn into_io_error(self) -> io::Error {
        match self {
            Self::Io(e) => e,
            Self::Other(msg) => io::Error::other(msg),
        }
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "transport io error: {e}"),
            Self::Other(msg) => write!(f, "transport error: {msg}"),
        }
    }
}

impl std::error::Error for TransportError {}

/// Pluggable HTTP transport. The production implementation is
/// [`UreqTransport`]; tests inject mocks.
pub trait Transport: Send + Sync {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError>;
}

impl<T: Transport + ?Sized> Transport for std::sync::Arc<T> {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        (**self).send(request)
    }
}

/// `ureq`-backed transport (blocking I/O, rustls TLS).
///
/// Redirects are disabled: a redirect would forward the SigV4
/// `Authorization` header to a different host. Non-2xx statuses are
/// returned as responses, not errors, so the backend can classify
/// them itself.
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    pub fn new(timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .max_redirects(0)
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl Transport for UreqTransport {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let mut builder = ureq::http::Request::builder()
            .method(request.method.as_str())
            .uri(request.url.as_str());
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let http_request = builder
            .body(request.body)
            .map_err(|e| TransportError::Other(format!("invalid request: {e}")))?;

        let mut response = self.agent.run(http_request).map_err(|e| match e {
            ureq::Error::Io(io) => TransportError::Io(io),
            other => TransportError::Other(other.to_string()),
        })?;

        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();

        let mut body = Vec::new();
        response
            .body_mut()
            .as_reader()
            .read_to_end(&mut body)
            .map_err(TransportError::Io)?;

        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

/// Inputs to the SigV4 `Authorization` header computation — grouped so
/// [`S3Backend::authorization_header`] stays under the argument limit.
struct SignatureParams<'a> {
    method: &'a str,
    canonical_uri: &'a str,
    canonical_query: &'a str,
    host: &'a str,
    payload_hash: &'a str,
    amz_date: &'a str,
    date_stamp: &'a str,
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

/// S3-compatible object-store backend.
///
/// Construct with [`S3Backend::new`] (validates the config and builds
/// the default [`UreqTransport`]), optionally followed by
/// [`S3Backend::with_transport`] to inject a mock in tests.
pub struct S3Backend {
    config: RemoteStorageConfig,
    endpoint_base: String,
    endpoint_host: String,
    /// URL path prefix contributed by a base path in the endpoint URL
    /// ("" or "/seg/seg", already percent-encoded).
    endpoint_base_path: String,
    /// Normalized configured prefix segments ("" prefix → empty vec).
    prefix_segments: Vec<String>,
    transport: Box<dyn Transport>,
    /// In-memory usage counter, maintained only when a quota is set.
    used_bytes: u64,
    /// `prepare` succeeded at least once; later calls are no-ops.
    prepared: bool,
}

impl S3Backend {
    /// Creates a backend from `config`.
    ///
    /// # Errors
    /// Returns [`StorageError::Io`] when the config is incomplete
    /// (missing bucket or credentials — see
    /// [`RemoteStorageConfig::is_complete`]) or the endpoint URL is
    /// malformed.
    pub fn new(config: RemoteStorageConfig) -> StorageResult<Self> {
        if !config.is_complete() {
            return Err(config_error(
                "remote storage config incomplete: bucket, access_key and \
                 secret_key are required (set KALADOC_S3_BUCKET, \
                 KALADOC_S3_ACCESS_KEY, KALADOC_S3_SECRET_KEY)",
            ));
        }
        let timeout = config.timeout;
        let parsed = parse_endpoint(&config.resolved_endpoint())?;
        let prefix_segments = clean_prefix(&config.prefix).map_err(|_| {
            config_error(format!(
                "invalid KALADOC_S3_PREFIX {:?}: must be a relative path \
                 without '..' components",
                config.prefix
            ))
        })?;

        Ok(Self {
            transport: Box::new(UreqTransport::new(timeout)),
            config,
            endpoint_base: parsed.base,
            endpoint_host: parsed.host,
            endpoint_base_path: parsed.base_path,
            prefix_segments,
            used_bytes: 0,
            prepared: false,
        })
    }

    /// Builder-style setter: replace the HTTP transport (tests/mocks).
    pub fn with_transport(mut self, transport: impl Transport + 'static) -> Self {
        self.transport = Box::new(transport);
        self
    }

    /// Builder-style setter: override the retry policy.
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.config.retry = retry;
        self
    }

    /// The resolved configuration (credentials included — do not log).
    pub fn config(&self) -> &RemoteStorageConfig {
        &self.config
    }

    // -- path confinement ----------------------------------------------------

    /// Splits `name` into normalized `/`-separated segments, rejecting
    /// anything that could escape the bucket+prefix root.
    fn confined_segments(&self, name: &str) -> StorageResult<Vec<String>> {
        let segments = clean_components(name)?;
        let mut out = Vec::with_capacity(self.prefix_segments.len() + segments.len());
        out.extend(self.prefix_segments.iter().cloned());
        out.extend(segments);
        Ok(out)
    }

    // -- request construction -------------------------------------------------

    /// Builds a signed [`HttpRequest`] for `method` on the object
    /// identified by `segments` (already prefix-inclusive, raw — not
    /// yet percent-encoded).
    fn build_request(
        &self,
        method: &str,
        segments: &[String],
        body: &[u8],
    ) -> StorageResult<HttpRequest> {
        let encoded_key = segments
            .iter()
            .map(|s| encode_segment(s))
            .collect::<Vec<_>>()
            .join("/");
        // base_path is "" or "/a/b"; "{base}/{bucket}/{key}" always
        // yields a path starting with '/'.
        let path = format!(
            "{}/{}/{}",
            self.endpoint_base_path, self.config.bucket, encoded_key
        );
        let url = format!("{}{}", self.endpoint_base, path);

        let payload_hash = sha256_hex(body);
        let (amz_date, date_stamp) = amz_timestamps(SystemTime::now());
        let authorization = self.authorization_header(SignatureParams {
            method,
            canonical_uri: &path,
            canonical_query: "",
            host: &self.endpoint_host,
            payload_hash: &payload_hash,
            amz_date: &amz_date,
            date_stamp: &date_stamp,
        });

        Ok(HttpRequest {
            method: method.to_string(),
            url,
            headers: vec![
                ("x-amz-date".to_string(), amz_date),
                ("x-amz-content-sha256".to_string(), payload_hash),
                ("Authorization".to_string(), authorization),
            ],
            body: body.to_vec(),
        })
    }

    /// Builds a signed request against the bucket itself (no object
    /// key) — used by `prepare` for the `HEAD` bucket check.
    fn build_bucket_request(&self, method: &str) -> StorageResult<HttpRequest> {
        let path = format!("{}/{}", self.endpoint_base_path, self.config.bucket);
        let url = format!("{}{}", self.endpoint_base, path);
        let payload_hash = sha256_hex(&[]);
        let (amz_date, date_stamp) = amz_timestamps(SystemTime::now());
        let authorization = self.authorization_header(SignatureParams {
            method,
            canonical_uri: &path,
            canonical_query: "",
            host: &self.endpoint_host,
            payload_hash: &payload_hash,
            amz_date: &amz_date,
            date_stamp: &date_stamp,
        });
        Ok(HttpRequest {
            method: method.to_string(),
            url,
            headers: vec![
                ("x-amz-date".to_string(), amz_date),
                ("x-amz-content-sha256".to_string(), payload_hash),
                ("Authorization".to_string(), authorization),
            ],
            body: Vec::new(),
        })
    }

    /// Computes the `Authorization` header value (SigV4).
    ///
    /// Pure function of its arguments — exposed to tests via
    /// [`S3Backend::sign_for_test`].
    fn authorization_header(&self, p: SignatureParams<'_>) -> String {
        let SignatureParams {
            method,
            canonical_uri,
            canonical_query,
            host,
            payload_hash,
            amz_date,
            date_stamp,
        } = p;
        let canonical_headers = format!(
            "host:{host}\n\
             x-amz-content-sha256:{payload_hash}\n\
             x-amz-date:{amz_date}\n"
        );
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";
        let canonical_request = format!(
            "{method}\n{canonical_uri}\n{canonical_query}\n\
             {canonical_headers}\n{signed_headers}\n{payload_hash}"
        );
        let scope = format!("{date_stamp}/{}/s3/aws4_request", self.config.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );

        // Derive the signing key: HMAC chain over date/region/service.
        let k_date = hmac_sha256(
            format!("AWS4{}", self.config.secret_key).as_bytes(),
            date_stamp,
        );
        let k_region = hmac_sha256(&k_date, &self.config.region);
        let k_service = hmac_sha256(&k_region, "s3");
        let k_signing = hmac_sha256(&k_service, "aws4_request");
        let signature = hex::encode(hmac_sha256(&k_signing, &string_to_sign));

        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, \
             SignedHeaders={signed_headers}, Signature={signature}",
            self.config.access_key
        )
    }

    /// Test hook: sign with explicit timestamps for deterministic
    /// assertions.
    #[cfg(test)]
    fn sign_for_test(&self, p: SignatureParams<'_>) -> String {
        self.authorization_header(p)
    }

    // -- transport + retry -----------------------------------------------------

    /// Sends `request`, retrying transient failures (transport errors
    /// and HTTP `5xx`) with exponential backoff + jitter. `4xx` and
    /// other non-5xx statuses are returned to the caller unchanged.
    fn send(&self, request: HttpRequest) -> StorageResult<HttpResponse> {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match self.transport.send(request.clone()) {
                Ok(response) => {
                    if is_retryable_status(response.status)
                        && attempt < self.config.retry.max_attempts
                    {
                        tracing::debug!(
                            status = response.status,
                            attempt,
                            url = %request.url,
                            "remote storage: retrying after 5xx"
                        );
                        std::thread::sleep(self.backoff(attempt));
                        continue;
                    }
                    return Ok(response);
                }
                Err(e) => {
                    if attempt < self.config.retry.max_attempts {
                        tracing::debug!(
                            error = %e,
                            attempt,
                            url = %request.url,
                            "remote storage: retrying after transport error"
                        );
                        std::thread::sleep(self.backoff(attempt));
                        continue;
                    }
                    return Err(StorageError::Io {
                        path: PathBuf::from(&request.url),
                        source: e.into_io_error(),
                    });
                }
            }
        }
    }

    /// Exponential backoff for retry `attempt` (1-based):
    /// `base * 2^(attempt-1)` capped at `max_delay`, plus uniform
    /// jitter in `[0, base_delay)`.
    fn backoff(&self, attempt: u32) -> Duration {
        let policy = &self.config.retry;
        let shift = attempt.saturating_sub(1).min(16);
        let exp = policy
            .base_delay
            .saturating_mul(1u32 << shift)
            .min(policy.max_delay);
        exp + jitter(policy.base_delay)
    }

    // -- shared op helpers -----------------------------------------------------

    /// `HEAD` on an object; returns the HTTP status (post-retry).
    fn head_status(&mut self, name: &str) -> StorageResult<u16> {
        let segments = self.confined_segments(name)?;
        let request = self.build_request("HEAD", &segments, &[])?;
        Ok(self.send(request)?.status)
    }

    /// Content-Length of the object at `name`, or 0 when absent.
    fn object_len(&mut self, name: &str) -> StorageResult<u64> {
        let segments = self.confined_segments(name)?;
        let request = self.build_request("HEAD", &segments, &[])?;
        let response = self.send(request)?;
        match response.status {
            200..=299 => Ok(content_length(&response.headers)),
            404 => Ok(0),
            status => Err(self.http_error("HEAD", name, status, &response.body)),
        }
    }

    fn http_error(&self, op: &str, name: &str, status: u16, body: &[u8]) -> StorageError {
        let snippet_len = body.len().min(256);
        let snippet = String::from_utf8_lossy(&body[..snippet_len]);
        StorageError::Io {
            path: PathBuf::from(name),
            source: io::Error::other(format!(
                "s3 {op} failed for {name}: HTTP {status}: {snippet}"
            )),
        }
    }

    fn check_quota(&self, old_len: u64, incoming: u64, name: &str) -> StorageResult<()> {
        let Some(limit) = self.config.max_size_bytes else {
            return Ok(());
        };
        let projected = self
            .used_bytes
            .saturating_sub(old_len)
            .saturating_add(incoming);
        if projected > limit {
            return Err(StorageError::QuotaExceeded {
                path: PathBuf::from(name),
                limit,
            });
        }
        Ok(())
    }

    fn track_bytes(&mut self, old_len: u64, new_len: u64) {
        if self.config.max_size_bytes.is_none() {
            return;
        }
        self.used_bytes = self
            .used_bytes
            .saturating_sub(old_len)
            .saturating_add(new_len);
    }
}

/// Only HTTP `5xx` statuses are retried; every `4xx` is final.
fn is_retryable_status(status: u16) -> bool {
    status >= 500
}

/// Splits a relative name into normalized segments, rejecting escapes.
///
/// `""`, `"."`, absolute paths (`/...`, `\...`), `..` components (on
/// either separator), and NUL bytes are rejected. Duplicate and `.`
/// separators collapse. Returns `StorageError::PathOutsideRoot` on
/// any violation.
fn clean_components(name: &str) -> StorageResult<Vec<String>> {
    let outside = || StorageError::PathOutsideRoot {
        path: PathBuf::from(name),
    };

    if name.is_empty() || name.starts_with('/') || name.starts_with('\\') {
        return Err(outside());
    }

    let mut segments = Vec::new();
    for part in name.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => return Err(outside()),
            p if p.contains('\0') => return Err(outside()),
            p => segments.push(p.to_string()),
        }
    }
    if segments.is_empty() {
        return Err(outside());
    }
    Ok(segments)
}

/// Like [`clean_components`] but for the configured prefix: an empty
/// result is allowed (root of the bucket) and leading/trailing
/// separators are tolerated since the prefix is operator config, not
/// an untrusted object name.
fn clean_prefix(prefix: &str) -> StorageResult<Vec<String>> {
    if prefix.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut segments = Vec::new();
    for part in prefix.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                return Err(StorageError::PathOutsideRoot {
                    path: PathBuf::from(prefix),
                });
            }
            p if p.contains('\0') => {
                return Err(StorageError::PathOutsideRoot {
                    path: PathBuf::from(prefix),
                });
            }
            p => segments.push(p.to_string()),
        }
    }
    Ok(segments)
}

/// Percent-encodes one path segment per RFC 3986 unreserved rules
/// (also the SigV4 canonical-URI encoding for S3).
fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Parses `scheme://authority[/base/path]` into transport pieces.
fn parse_endpoint(endpoint: &str) -> StorageResult<ParsedEndpoint> {
    let invalid = |msg: &str| config_error(format!("invalid S3 endpoint {endpoint:?}: {msg}"));

    let (scheme, rest) = endpoint
        .split_once("://")
        .ok_or_else(|| invalid("must include a scheme (http:// or https://)"))?;
    if scheme != "http" && scheme != "https" {
        return Err(invalid("scheme must be http or https"));
    }
    let (authority, path_part) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err(invalid("missing host"));
    }
    if path_part.contains('?') || path_part.contains('#') {
        return Err(invalid("query/fragment not allowed"));
    }

    // Optional base path (e.g. a path-prefix reverse proxy): normalize
    // and percent-encode each segment, rejecting traversal.
    let mut base_segments = Vec::new();
    for part in path_part.split('/').filter(|p| !p.is_empty()) {
        match part {
            "." => {}
            ".." => return Err(invalid("base path must not contain '..'")),
            p if p.contains('\0') => return Err(invalid("base path contains NUL")),
            p => base_segments.push(encode_segment(p)),
        }
    }
    let base_path = if base_segments.is_empty() {
        String::new()
    } else {
        format!("/{}", base_segments.join("/"))
    };

    Ok(ParsedEndpoint {
        base: format!("{scheme}://{authority}"),
        host: authority.to_string(),
        base_path,
    })
}

struct ParsedEndpoint {
    /// `scheme://authority` with no trailing slash or path.
    base: String,
    /// `authority` (host[:port]) — the value signed as `host`.
    host: String,
    /// "" or "/seg/seg" (percent-encoded).
    base_path: String,
}

fn config_error(msg: impl Into<String>) -> StorageError {
    StorageError::Io {
        path: PathBuf::new(),
        source: io::Error::other(msg.into()),
    }
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn hmac_sha256(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// Content-Length header value (0 when absent/unparseable).
fn content_length(headers: &[(String, String)]) -> u64 {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// `(amz_date, date_stamp)` for `now`: `YYYYMMDDTHHMMSSZ` and `YYYYMMDD`.
fn amz_timestamps(now: SystemTime) -> (String, String) {
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hh, mm, ss) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    (
        format!("{year:04}{month:02}{day:02}T{hh:02}{mm:02}{ss:02}Z"),
        format!("{year:04}{month:02}{day:02}"),
    )
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's
/// civil-from-days algorithm; avoids a datetime dependency.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// SplitMix64-style jitter in `[0, cap)`. Seeded from the clock and a
/// process-unique counter — jitter needs no cryptographic strength.
fn jitter(cap: Duration) -> Duration {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let cap_nanos = cap.as_nanos();
    if cap_nanos == 0 {
        return Duration::ZERO;
    }
    let nanos = cap_nanos.min(u64::MAX as u128) as u64;
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
        ^ COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    // splitmix64 finalizer
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // `% nanos` yields [0, cap); nanos > 0 is guaranteed above.
    Duration::from_nanos(z % nanos)
}

// ---------------------------------------------------------------------------
// StorageBackend impl
// ---------------------------------------------------------------------------

impl StorageBackend for S3Backend {
    fn write(&mut self, name: &str, data: &[u8]) -> StorageResult<()> {
        // Quota accounting needs the replaced object's size; HEAD it
        // first (only when a quota is configured).
        let old_len = if self.config.max_size_bytes.is_some() {
            self.object_len(name)?
        } else {
            0
        };
        self.check_quota(old_len, data.len() as u64, name)?;

        let segments = self.confined_segments(name)?;
        let request = self.build_request("PUT", &segments, data)?;
        let response = self.send(request)?;
        if !(200..300).contains(&response.status) {
            return Err(self.http_error("PUT", name, response.status, &response.body));
        }

        self.track_bytes(old_len, data.len() as u64);
        Ok(())
    }

    fn read(&mut self, name: &str) -> StorageResult<Vec<u8>> {
        let segments = self.confined_segments(name)?;
        let request = self.build_request("GET", &segments, &[])?;
        let response = self.send(request)?;
        match response.status {
            200..=299 => Ok(response.body),
            404 => Err(StorageError::NotFound {
                path: PathBuf::from(name),
            }),
            status => Err(self.http_error("GET", name, status, &response.body)),
        }
    }

    fn delete(&mut self, name: &str) -> StorageResult<()> {
        // Same HEAD-first pattern as write when tracking usage.
        let old_len = if self.config.max_size_bytes.is_some() {
            self.object_len(name)?
        } else {
            0
        };

        let segments = self.confined_segments(name)?;
        let request = self.build_request("DELETE", &segments, &[])?;
        let response = self.send(request)?;
        match response.status {
            // Deleting a missing object is a no-op per the trait.
            200..=299 | 404 => {
                self.track_bytes(old_len, 0);
                Ok(())
            }
            status => Err(self.http_error("DELETE", name, status, &response.body)),
        }
    }

    fn exists(&mut self, name: &str) -> StorageResult<bool> {
        match self.head_status(name)? {
            200..=299 => Ok(true),
            404 => Ok(false),
            status => {
                // Re-issue is wasteful; head_status already consumed the
                // response, so report the status directly.
                Err(StorageError::Io {
                    path: PathBuf::from(name),
                    source: io::Error::other(format!("s3 HEAD failed for {name}: HTTP {status}")),
                })
            }
        }
    }

    fn used_bytes(&self) -> Option<u64> {
        self.config.max_size_bytes.map(|_| self.used_bytes)
    }

    /// `HEAD` on the bucket: verifies credentials and connectivity.
    /// Idempotent — a successful check is not repeated.
    fn prepare(&mut self) -> StorageResult<()> {
        if self.prepared {
            return Ok(());
        }
        let request = self.build_bucket_request("HEAD")?;
        let response = self.send(request)?;
        match response.status {
            200..=299 => {
                self.prepared = true;
                Ok(())
            }
            404 => Err(StorageError::Io {
                path: PathBuf::from(&self.config.bucket),
                source: io::Error::other(format!(
                    "s3 bucket {:?} not found at {}",
                    self.config.bucket, self.endpoint_base
                )),
            }),
            403 => Err(StorageError::Io {
                path: PathBuf::from(&self.config.bucket),
                source: io::Error::other(
                    "s3 access denied: credentials lack HeadBucket permission \
                     or are invalid",
                ),
            }),
            status => {
                Err(self.http_error("HEAD bucket", &self.config.bucket, status, &response.body))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    // -- mocks ---------------------------------------------------------------

    /// Scripted transport: returns queued results, records requests.
    struct MockTransport {
        responses: Mutex<VecDeque<Result<HttpResponse, TransportError>>>,
        requests: Mutex<Vec<HttpRequest>>,
    }

    impl MockTransport {
        fn new(responses: Vec<Result<HttpResponse, TransportError>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn ok(status: u16) -> Result<HttpResponse, TransportError> {
            Ok(HttpResponse {
                status,
                headers: vec![("content-length".to_string(), "0".to_string())],
                body: Vec::new(),
            })
        }

        fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }

        fn last_request(&self) -> HttpRequest {
            self.requests.lock().unwrap().last().unwrap().clone()
        }
    }

    impl Transport for MockTransport {
        fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    Ok(HttpResponse {
                        status: 500,
                        headers: Vec::new(),
                        body: b"script exhausted".to_vec(),
                    })
                })
        }
    }

    fn test_config() -> RemoteStorageConfig {
        RemoteStorageConfig {
            endpoint: "http://s3.test:9000".to_string(),
            bucket: "docs".to_string(),
            region: "us-east-1".to_string(),
            prefix: "kaladoc".to_string(),
            access_key: "AKIDEXAMPLE".to_string(),
            secret_key: "secret".to_string(),
            timeout: Duration::from_secs(5),
            retry: RetryPolicy {
                max_attempts: 3,
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            },
            max_size_bytes: None,
        }
    }

    fn backend_with(
        responses: Vec<Result<HttpResponse, TransportError>>,
    ) -> (S3Backend, Arc<MockTransport>) {
        let transport = Arc::new(MockTransport::new(responses));
        let backend = S3Backend::new(test_config())
            .unwrap()
            .with_transport(transport.clone());
        (backend, transport)
    }

    // -- path confinement ------------------------------------------------------

    #[test]
    fn rejects_traversal_and_absolute_names() {
        let (mut backend, _) = backend_with(vec![]);
        for bad in [
            "../escape",
            "a/../../b",
            "..",
            "/absolute",
            "\\windows",
            "a\\..\\b",
            "",
            ".",
            "a/./..",
            "bad\0name",
        ] {
            assert!(
                matches!(
                    backend.write(bad, b"x"),
                    Err(StorageError::PathOutsideRoot { .. })
                ),
                "expected PathOutsideRoot for {bad:?}"
            );
        }
    }

    #[test]
    fn accepts_and_normalizes_relative_names() {
        let (mut backend, transport) = backend_with(vec![MockTransport::ok(200)]);
        backend.write("a//b/./c.txt", b"data").unwrap();
        let req = transport.last_request();
        // prefix "kaladoc" + normalized "a/b/c.txt"
        assert_eq!(req.url, "http://s3.test:9000/docs/kaladoc/a/b/c.txt");
    }

    #[test]
    fn special_characters_are_percent_encoded() {
        let (mut backend, transport) = backend_with(vec![MockTransport::ok(200)]);
        backend.write("dir/a b+c?.txt", b"x").unwrap();
        let req = transport.last_request();
        assert_eq!(
            req.url,
            "http://s3.test:9000/docs/kaladoc/dir/a%20b%2Bc%3F.txt"
        );
    }

    // -- request construction ---------------------------------------------------

    #[test]
    fn put_request_is_signed() {
        let (mut backend, transport) = backend_with(vec![MockTransport::ok(200)]);
        backend.write("obj.bin", b"hello").unwrap();
        let req = transport.last_request();

        assert_eq!(req.method, "PUT");
        assert_eq!(req.url, "http://s3.test:9000/docs/kaladoc/obj.bin");
        assert_eq!(req.body, b"hello");

        let headers = |name: &str| {
            req.headers
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        // Payload hash is the SHA-256 of the body.
        assert_eq!(headers("x-amz-content-sha256"), sha256_hex(b"hello"));
        // SigV4 authorization header structure.
        let auth = headers("Authorization");
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"));
        assert!(auth.contains("/us-east-1/s3/aws4_request"));
        assert!(auth.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date"));
        assert!(auth.contains("Signature="));
        // x-amz-date is present and well-formed.
        let date = headers("x-amz-date");
        assert_eq!(date.len(), 16);
        assert!(date.ends_with('Z'));
    }

    #[test]
    fn signing_is_deterministic_and_secret_dependent() {
        let backend = S3Backend::new(test_config()).unwrap();
        let empty_hash = sha256_hex(b"");
        fn params<'a>(payload_hash: &'a str) -> SignatureParams<'a> {
            SignatureParams {
                method: "GET",
                canonical_uri: "/docs/kaladoc/x",
                canonical_query: "",
                host: "s3.test:9000",
                payload_hash,
                amz_date: "20240102T030405Z",
                date_stamp: "20240102",
            }
        }
        let a = backend.sign_for_test(params(&empty_hash));
        let b = backend.sign_for_test(params(&empty_hash));
        assert_eq!(a, b);

        let mut other = test_config();
        other.secret_key = "different".to_string();
        let other_backend = S3Backend::new(other).unwrap();
        let c = other_backend.sign_for_test(params(&empty_hash));
        assert_ne!(a, c);
    }

    #[test]
    fn signature_matches_manual_hmac_chain() {
        // Cross-check authorization_header against an independently
        // computed SigV4 signature for fixed inputs.
        let backend = S3Backend::new(test_config()).unwrap();
        let method = "GET";
        let uri = "/docs/kaladoc/x";
        let host = "s3.test:9000";
        let payload_hash = sha256_hex(b"");
        let amz_date = "20240102T030405Z";
        let date_stamp = "20240102";

        let canonical_headers =
            format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n");
        let canonical_request = format!(
            "{method}\n{uri}\n\n{canonical_headers}\nhost;x-amz-content-sha256;x-amz-date\n{payload_hash}"
        );
        let scope = "20240102/us-east-1/s3/aws4_request";
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let k_date = hmac_sha256(b"AWS4secret", date_stamp);
        let k_region = hmac_sha256(&k_date, "us-east-1");
        let k_service = hmac_sha256(&k_region, "s3");
        let k_signing = hmac_sha256(&k_service, "aws4_request");
        let expected_sig = hex::encode(hmac_sha256(&k_signing, &string_to_sign));

        let auth = backend.sign_for_test(SignatureParams {
            method,
            canonical_uri: uri,
            canonical_query: "",
            host,
            payload_hash: &payload_hash,
            amz_date,
            date_stamp,
        });
        assert!(auth.ends_with(&format!("Signature={expected_sig}")));
    }

    #[test]
    fn amz_timestamps_and_civil_date() {
        // 2024-01-02T03:04:05Z
        let t = UNIX_EPOCH + Duration::from_secs(1_704_164_645);
        let (amz, date) = amz_timestamps(t);
        assert_eq!(amz, "20240102T030405Z");
        assert_eq!(date, "20240102");
        // Epoch sanity.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }

    // -- CRUD mapping ------------------------------------------------------------

    #[test]
    fn read_maps_404_to_not_found() {
        let (mut backend, _) = backend_with(vec![MockTransport::ok(404)]);
        assert!(matches!(
            backend.read("missing.txt"),
            Err(StorageError::NotFound { .. })
        ));
    }

    #[test]
    fn read_returns_body_on_200() {
        let (mut backend, _) = backend_with(vec![Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: b"payload".to_vec(),
        })]);
        assert_eq!(backend.read("f.bin").unwrap(), b"payload");
    }

    #[test]
    fn exists_maps_head_statuses() {
        let (mut backend, _) = backend_with(vec![MockTransport::ok(200)]);
        assert!(backend.exists("f").unwrap());

        let (mut backend, _) = backend_with(vec![MockTransport::ok(404)]);
        assert!(!backend.exists("f").unwrap());
    }

    #[test]
    fn delete_treats_404_as_noop() {
        let (mut backend, _) = backend_with(vec![MockTransport::ok(404)]);
        backend.delete("gone").unwrap();
    }

    #[test]
    fn http_4xx_surfaces_as_io_error() {
        let (mut backend, transport) = backend_with(vec![MockTransport::ok(403)]);
        let err = backend.read("f").unwrap_err();
        assert!(matches!(err, StorageError::Io { .. }));
        // 4xx is never retried.
        assert_eq!(transport.request_count(), 1);
    }

    // -- retry -------------------------------------------------------------------

    #[test]
    fn retries_5xx_then_succeeds() {
        let (mut backend, transport) = backend_with(vec![
            MockTransport::ok(503),
            MockTransport::ok(500),
            MockTransport::ok(200),
        ]);
        assert!(backend.exists("f").unwrap());
        assert_eq!(transport.request_count(), 3);
    }

    #[test]
    fn retries_transport_errors() {
        let (mut backend, transport) = backend_with(vec![
            Err(TransportError::Other("connection reset".to_string())),
            MockTransport::ok(200),
        ]);
        assert!(backend.exists("f").unwrap());
        assert_eq!(transport.request_count(), 2);
    }

    #[test]
    fn gives_up_after_max_attempts() {
        let (mut backend, transport) = backend_with(vec![
            MockTransport::ok(500),
            MockTransport::ok(500),
            MockTransport::ok(500),
            MockTransport::ok(200), // would succeed — never reached
        ]);
        // max_attempts = 3 → final 500 surfaces as Io error.
        let err = backend.exists("f").unwrap_err();
        assert!(matches!(err, StorageError::Io { .. }));
        assert_eq!(transport.request_count(), 3);
    }

    #[test]
    fn transport_error_exhausts_attempts() {
        let (mut backend, transport) = backend_with(vec![
            Err(TransportError::Other("dns".to_string())),
            Err(TransportError::Other("dns".to_string())),
            Err(TransportError::Other("dns".to_string())),
        ]);
        let err = backend.exists("f").unwrap_err();
        assert!(matches!(err, StorageError::Io { .. }));
        assert_eq!(transport.request_count(), 3);
    }

    // -- prepare / used_bytes -----------------------------------------------------

    #[test]
    fn prepare_checks_bucket_once() {
        let (mut backend, transport) = backend_with(vec![MockTransport::ok(200)]);
        backend.prepare().unwrap();
        backend.prepare().unwrap(); // idempotent: no second request
        assert_eq!(transport.request_count(), 1);

        let req = transport.last_request();
        assert_eq!(req.method, "HEAD");
        assert_eq!(req.url, "http://s3.test:9000/docs");
    }

    #[test]
    fn prepare_fails_on_missing_bucket() {
        let (mut backend, _) = backend_with(vec![MockTransport::ok(404)]);
        assert!(backend.prepare().is_err());
    }

    #[test]
    fn used_bytes_is_none_without_quota() {
        let (backend, _) = backend_with(vec![]);
        assert_eq!(backend.used_bytes(), None);
    }

    #[test]
    fn used_bytes_tracks_quota_writes() {
        // HEAD-before-PUT returns old_len; second HEAD reports 10 bytes.
        let transport = Arc::new(MockTransport::new(vec![
            MockTransport::ok(404),
            MockTransport::ok(200),
            Ok(HttpResponse {
                status: 200,
                headers: vec![("content-length".to_string(), "10".to_string())],
                body: Vec::new(),
            }),
            MockTransport::ok(200),
        ]));
        let mut cfg = test_config();
        cfg.max_size_bytes = Some(100);
        let mut backend = S3Backend::new(cfg).unwrap().with_transport(transport);

        backend.write("a", &[0u8; 10]).unwrap();
        assert_eq!(backend.used_bytes(), Some(10));

        // Overwrite with 50 bytes: HEAD says old=10 → 10-10+50.
        // The queued HEAD returns content-length 10 again.
        backend.write("a", &[0u8; 50]).unwrap();
        assert_eq!(backend.used_bytes(), Some(50));
    }

    #[test]
    fn quota_rejects_oversized_write() {
        let mut cfg = test_config();
        cfg.max_size_bytes = Some(10);
        let transport = Arc::new(MockTransport::new(vec![MockTransport::ok(404)]));
        let mut backend = S3Backend::new(cfg).unwrap().with_transport(transport);
        assert!(matches!(
            backend.write("big", &[0u8; 11]),
            Err(StorageError::QuotaExceeded { .. })
        ));
    }

    #[test]
    fn new_requires_complete_config() {
        let mut cfg = test_config();
        cfg.bucket.clear();
        assert!(S3Backend::new(cfg).is_err());

        let mut cfg = test_config();
        cfg.secret_key.clear();
        assert!(S3Backend::new(cfg).is_err());
    }

    #[test]
    fn new_rejects_bad_endpoint_and_prefix() {
        let mut cfg = test_config();
        cfg.endpoint = "ftp://nope".to_string();
        assert!(S3Backend::new(cfg).is_err());

        let mut cfg = test_config();
        cfg.prefix = "a/../b".to_string();
        assert!(S3Backend::new(cfg).is_err());
    }

    #[test]
    fn endpoint_with_base_path_is_supported() {
        let mut cfg = test_config();
        cfg.endpoint = "https://gw.example.com/storage/".to_string();
        let transport = Arc::new(MockTransport::new(vec![MockTransport::ok(200)]));
        let mut backend = S3Backend::new(cfg)
            .unwrap()
            .with_transport(transport.clone());
        backend.exists("f").unwrap();
        let req = transport.last_request();
        assert_eq!(req.url, "https://gw.example.com/storage/docs/kaladoc/f");
        // Signed host excludes the base path.
        let auth = req
            .headers
            .iter()
            .find(|(n, _)| n == "Authorization")
            .unwrap()
            .1
            .clone();
        assert!(auth.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date"));
    }
}
