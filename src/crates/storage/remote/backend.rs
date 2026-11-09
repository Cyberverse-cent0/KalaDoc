//! [`RemoteBackend`]: an S3-compatible object-store implementation of
//! [`StorageBackend`].
//!
//! ## Design choice
//!
//! v1 targets **S3 with AWS Signature Version 4** over plain HTTP
//! (path-style URLs: `{endpoint}/{bucket}/{key}`). This covers AWS S3
//! and the common S3-compatible stores (MinIO, Ceph RGW, Garage,
//! LocalStack, ...) with one code path. Signing is implemented
//! in-crate ([`super::sign`]) on top of `sha2`; transport is a
//! synchronous `ureq` client behind a [`HttpTransport`] trait object so
//! tests inject a mock and never touch the network.
//!
//! ## Confinement
//!
//! Object names are confined exactly like the local backend: the name
//! is normalized component-wise (`.` dropped, `..` pops a component),
//! absolute paths and escapes above the root are rejected with
//! [`StorageError::PathOutsideRoot`], and the surviving segments are
//! appended under `bucket/prefix/` — so a name can never address an
//! object outside the configured bucket+prefix.
//!
//! ## Errors and retries
//!
//! Transport failures (connect/DNS/TLS/timeout) and transient HTTP
//! statuses (408, 429, 5xx) are retried with exponential backoff up to
//! `config.max_retries` times; exhaustion maps to
//! [`StorageError::Io`]. 404 maps to [`StorageError::NotFound`] for
//! reads and to success-no-op for deletes/`exists`. Other permanent
//! statuses map to `Io` with the status and a body snippet. There is
//! no client-side quota: `used_bytes` returns `None`.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use super::config::RemoteStorageConfig;
use super::sign::{SigV4Signer, encode_segment};
use super::transport::{HttpRequest, HttpResponse, HttpTransport, Method, UreqTransport};
use crate::crates::storage::backend::StorageBackend;
use crate::crates::storage::local::debug_when_full::{StorageError, StorageResult};

/// S3-compatible remote object-store backend.
///
/// Construct with [`RemoteBackend::new`] for the production `ureq`
/// transport, or [`RemoteBackend::with_transport`] to inject a mock in
/// tests.
pub struct RemoteBackend {
    config: RemoteStorageConfig,
    /// Scheme + authority, normalized (no trailing slash, no path).
    endpoint: String,
    signer: SigV4Signer,
    transport: Box<dyn HttpTransport>,
}

impl RemoteBackend {
    /// Builds a backend over the production `ureq` transport.
    ///
    /// # Errors
    /// Returns `io::Error` (InvalidInput) when `config.endpoint` is not
    /// a bare `http(s)://authority` URL, `config.bucket` is empty, or
    /// credentials are missing.
    pub fn new(config: RemoteStorageConfig) -> io::Result<Self> {
        Self::with_transport(config, Box::new(UreqTransport::new()))
    }

    /// Builds a backend over an injected transport — used by tests and
    /// by embedders that want a custom HTTP stack.
    ///
    /// # Errors
    /// Same validation as [`RemoteBackend::new`].
    pub(crate) fn with_transport(
        config: RemoteStorageConfig,
        transport: Box<dyn HttpTransport>,
    ) -> io::Result<Self> {
        let endpoint = normalize_endpoint(&config.endpoint)?;
        if config.bucket.trim().is_empty() {
            return Err(invalid_input("remote bucket must not be empty"));
        }
        if config.access_key.is_empty() || config.secret_key.is_empty() {
            return Err(invalid_input(
                "remote credentials (access key / secret key) must not be empty",
            ));
        }
        let signer = SigV4Signer::new(&config.access_key, &config.secret_key, &config.region);
        Ok(Self {
            config,
            endpoint,
            signer,
            transport,
        })
    }

    /// The configuration this backend was built with.
    pub fn config(&self) -> &RemoteStorageConfig {
        &self.config
    }

    /// Validates `name` and returns its normalized form (components
    /// joined by `/`), or `PathOutsideRoot` when it escapes the root.
    /// Mirrors [`FaultInjectingFs::resolve`]'s normalization semantics.
    fn normalize_name(&self, name: &str) -> StorageResult<String> {
        let path = Path::new(name);
        let mut normalized: Vec<String> = Vec::new();
        for comp in path.components() {
            match comp {
                Component::CurDir => {}
                Component::ParentDir => {
                    if normalized.pop().is_none() {
                        return Err(StorageError::PathOutsideRoot {
                            path: PathBuf::from(name),
                        });
                    }
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(StorageError::PathOutsideRoot {
                        path: PathBuf::from(name),
                    });
                }
                Component::Normal(c) => normalized.push(c.to_string_lossy().into_owned()),
            }
        }
        if normalized.is_empty() {
            // "." or "" address the root itself — not an object.
            return Err(StorageError::PathOutsideRoot {
                path: PathBuf::from(name),
            });
        }
        Ok(normalized.join("/"))
    }

    /// Builds the percent-encoded canonical path for `name`,
    /// e.g. `/bucket/prefix/dir/file.txt`.
    fn object_path(&self, name: &str) -> StorageResult<String> {
        let normalized = self.normalize_name(name)?;
        let mut segments: Vec<&str> = Vec::new();
        let prefix = self.config.normalized_prefix();
        if !prefix.is_empty() {
            segments.extend(prefix.split('/'));
        }
        segments.extend(normalized.split('/'));

        let mut path = String::with_capacity(64);
        path.push('/');
        path.push_str(&encode_segment(&self.config.bucket));
        for seg in segments {
            path.push('/');
            path.push_str(&encode_segment(seg));
        }
        Ok(path)
    }

    /// Sends one signed request with bounded retry, returning the final
    /// HTTP response (whatever its status) or `StorageError::Io` when
    /// transport-level retries are exhausted.
    fn send(&mut self, method: Method, path: &str, body: &[u8]) -> StorageResult<HttpResponse> {
        let url = format!("{}{}", self.endpoint, path);

        let mut headers: Vec<(String, String)> = Vec::new();
        let authorization = self.signer.sign(method.as_str(), &url, &mut headers, body);
        headers.push(("authorization".to_string(), authorization));

        let request = HttpRequest {
            method,
            url,
            headers,
            body: body.to_vec(),
        };

        let mut attempt = 0u32;
        loop {
            match self.transport.request(&request) {
                Ok(resp) => {
                    if is_transient_status(resp.status) && attempt < self.config.max_retries {
                        sleep_backoff(&self.config, attempt);
                        attempt += 1;
                        continue;
                    }
                    return Ok(resp);
                }
                Err(e) => {
                    if attempt < self.config.max_retries {
                        sleep_backoff(&self.config, attempt);
                        attempt += 1;
                        continue;
                    }
                    return Err(StorageError::Io {
                        path: PathBuf::from(path),
                        source: io::Error::new(io::ErrorKind::Other, e.to_string()),
                    });
                }
            }
        }
    }

    /// Maps a non-success HTTP status to `StorageError::Io`, including
    /// a short body snippet for diagnosability.
    fn status_error(path: &Path, op: &str, resp: &HttpResponse) -> StorageError {
        let snippet: String = String::from_utf8_lossy(&resp.body[..resp.body.len().min(200)])
            .trim()
            .to_string();
        StorageError::Io {
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::Other,
                format!("remote {op} failed: HTTP {} {snippet}", resp.status),
            ),
        }
    }

    fn bucket_path(&self) -> String {
        format!("/{}", encode_segment(&self.config.bucket))
    }
}

/// Statuses worth retrying: request timeout, rate limiting, and
/// server-side transients. Everything else is permanent.
fn is_transient_status(status: u16) -> bool {
    matches!(status, 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

fn sleep_backoff(config: &RemoteStorageConfig, attempt: u32) {
    // Exponential backoff, capped at 30s. `attempt` is 0-based.
    let shift = attempt.min(16);
    let ms = config
        .initial_backoff_ms
        .saturating_mul(1u64 << shift)
        .min(30_000);
    if ms > 0 {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

/// Validates `endpoint` is `http(s)://authority` with no path, and
/// returns the normalized `scheme://authority` (no trailing slash).
fn normalize_endpoint(endpoint: &str) -> io::Result<String> {
    let trimmed = endpoint.trim().trim_end_matches('/');
    let rest = trimmed
        .strip_prefix("https://")
        .map(|r| ("https", r))
        .or_else(|| trimmed.strip_prefix("http://").map(|r| ("http", r)))
        .ok_or_else(|| invalid_input("remote endpoint must start with http:// or https://"))?;
    let (scheme, rest) = rest;
    // Reject a path component — object keys live under the bucket.
    let authority = match rest.find('/') {
        Some(i) => {
            if rest[i + 1..].is_empty() {
                &rest[..i]
            } else {
                return Err(invalid_input(
                    "remote endpoint must not include a path component",
                ));
            }
        }
        None => rest,
    };
    if authority.is_empty() {
        return Err(invalid_input("remote endpoint has no host"));
    }
    Ok(format!("{scheme}://{authority}"))
}

fn invalid_input(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.to_string())
}

impl StorageBackend for RemoteBackend {
    fn write(&mut self, name: &str, data: &[u8]) -> StorageResult<()> {
        let path = self.object_path(name)?;
        let resp = self.send(Method::Put, &path, data)?;
        if (200..300).contains(&resp.status) {
            Ok(())
        } else if resp.status == 404 {
            Err(StorageError::Io {
                path: PathBuf::from(name),
                source: io::Error::new(
                    io::ErrorKind::NotFound,
                    "remote bucket not found (HTTP 404)",
                ),
            })
        } else {
            Err(Self::status_error(Path::new(name), "write", &resp))
        }
    }

    fn read(&mut self, name: &str) -> StorageResult<Vec<u8>> {
        let path = self.object_path(name)?;
        let resp = self.send(Method::Get, &path, &[])?;
        match resp.status {
            200..=299 => Ok(resp.body),
            404 => Err(StorageError::NotFound {
                path: PathBuf::from(name),
            }),
            _ => Err(Self::status_error(Path::new(name), "read", &resp)),
        }
    }

    fn delete(&mut self, name: &str) -> StorageResult<()> {
        let path = self.object_path(name)?;
        let resp = self.send(Method::Delete, &path, &[])?;
        // S3 DELETE is idempotent: 204/200 on success, 404 when the
        // object (or bucket) is already gone — both are no-ops.
        if (200..300).contains(&resp.status) || resp.status == 404 {
            Ok(())
        } else {
            Err(Self::status_error(Path::new(name), "delete", &resp))
        }
    }

    fn exists(&mut self, name: &str) -> StorageResult<bool> {
        let path = self.object_path(name)?;
        let resp = self.send(Method::Head, &path, &[])?;
        match resp.status {
            200..=299 => Ok(true),
            404 => Ok(false),
            _ => Err(Self::status_error(Path::new(name), "exists", &resp)),
        }
    }

    /// Object stores can't cheaply report total usage — the trait
    /// explicitly allows `None` when accounting isn't maintained.
    fn used_bytes(&self) -> Option<u64> {
        None
    }

    /// Idempotent connectivity/credential check: `HEAD` on the bucket.
    /// 200 means reachable with working credentials; 401/403 means the
    /// credentials were rejected; 404 means the bucket doesn't exist.
    fn prepare(&mut self) -> StorageResult<()> {
        let path = self.bucket_path();
        let resp = self.send(Method::Head, &path, &[])?;
        match resp.status {
            200..=299 => Ok(()),
            401 | 403 => Err(StorageError::Io {
                path: PathBuf::from(&self.config.bucket),
                source: io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "remote credentials rejected by {} (HTTP {})",
                        self.config.endpoint, resp.status
                    ),
                ),
            }),
            404 => Err(StorageError::Io {
                path: PathBuf::from(&self.config.bucket),
                source: io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("remote bucket not found: {}", self.config.bucket),
                ),
            }),
            _ => Err(Self::status_error(
                Path::new(&self.config.bucket),
                "prepare",
                &resp,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::storage::remote::transport::{HttpRequest, HttpResponse, TransportError};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    /// In-memory object store behind the transport trait. `script`
    /// queues per-request outcomes to simulate transient failures;
    /// `objects` is the backing map keyed by request path.
    #[derive(Default)]
    struct MockState {
        objects: HashMap<String, Vec<u8>>,
        /// Queued statuses to return before falling back to normal
        /// handling, consumed one per request.
        scripted: std::collections::VecDeque<u16>,
        /// Queued transport errors.
        transport_errors: std::collections::VecDeque<String>,
        requests: Vec<(Method, String)>,
    }

    struct MockTransport {
        state: Rc<RefCell<MockState>>,
    }

    impl HttpTransport for MockTransport {
        fn request(&mut self, req: &HttpRequest) -> Result<HttpResponse, TransportError> {
            let mut st = self.state.borrow_mut();
            st.requests.push((req.method, req.url.clone()));

            if let Some(msg) = st.transport_errors.pop_front() {
                return Err(TransportError(msg));
            }
            if let Some(status) = st.scripted.pop_front() {
                return Ok(HttpResponse {
                    status,
                    body: Vec::new(),
                });
            }

            // Extract the object path from the URL: everything after
            // the authority.
            let path = req
                .url
                .split_once("://")
                .map(|(_, rest)| match rest.find('/') {
                    Some(i) => rest[i..].to_string(),
                    None => String::new(),
                })
                .unwrap_or_default();

            match req.method {
                Method::Put => {
                    st.objects.insert(path, req.body.clone());
                    Ok(HttpResponse {
                        status: 200,
                        body: Vec::new(),
                    })
                }
                Method::Get => match st.objects.get(&path) {
                    Some(data) => Ok(HttpResponse {
                        status: 200,
                        body: data.clone(),
                    }),
                    None => Ok(HttpResponse {
                        status: 404,
                        body: Vec::new(),
                    }),
                },
                Method::Delete => {
                    st.objects.remove(&path);
                    Ok(HttpResponse {
                        status: 204,
                        body: Vec::new(),
                    })
                }
                Method::Head => {
                    // HEAD bucket path is "/bucket"; object HEADs are
                    // "/bucket/...".
                    let exists = if path == format!("/{}", TEST_BUCKET) {
                        true // bucket always exists in the mock
                    } else {
                        st.objects.contains_key(&path)
                    };
                    Ok(HttpResponse {
                        status: if exists { 200 } else { 404 },
                        body: Vec::new(),
                    })
                }
            }
        }
    }

    const TEST_BUCKET: &str = "test-bucket";

    fn test_config() -> RemoteStorageConfig {
        RemoteStorageConfig::new("http://objects.test:9000", TEST_BUCKET)
            .with_credentials("AKID", "secret")
            .with_prefix("kaladoc")
            .with_retry(3, 0)
    }

    fn backend() -> (RemoteBackend, Rc<RefCell<MockState>>) {
        let state = Rc::new(RefCell::new(MockState::default()));
        let transport = MockTransport {
            state: Rc::clone(&state),
        };
        let b = RemoteBackend::with_transport(test_config(), Box::new(transport)).unwrap();
        (b, state)
    }

    #[test]
    fn write_read_delete_round_trip() {
        let (mut b, state) = backend();

        b.write("docs/report.pdf", b"pdf-bytes").unwrap();
        assert_eq!(b.read("docs/report.pdf").unwrap(), b"pdf-bytes");
        assert!(b.exists("docs/report.pdf").unwrap());
        assert!(!b.exists("docs/missing.pdf").unwrap());

        b.delete("docs/report.pdf").unwrap();
        assert!(!b.exists("docs/report.pdf").unwrap());
        // Delete is idempotent.
        b.delete("docs/report.pdf").unwrap();

        // Requests went under /test-bucket/kaladoc/ (prefix confinement).
        let st = state.borrow();
        assert!(!st.requests.is_empty());
        assert!(
            st.requests
                .iter()
                .all(|(_, u)| u.contains("/test-bucket/kaladoc/"))
        );
        // PUT/GET/HEAD/DELETE all targeted the object path.
        for m in [Method::Put, Method::Get, Method::Delete] {
            assert!(
                st.requests
                    .iter()
                    .any(|(rm, u)| *rm == m && u.ends_with("/docs/report.pdf"))
            );
        }
        assert!(
            st.requests
                .iter()
                .any(|(m, u)| *m == Method::Head && u.ends_with("/docs/missing.pdf"))
        );
    }

    #[test]
    fn read_missing_maps_to_not_found() {
        let (mut b, _) = backend();
        assert!(matches!(
            b.read("nope.bin"),
            Err(StorageError::NotFound { .. })
        ));
    }

    #[test]
    fn confinement_rejects_traversal_and_absolute() {
        let (mut b, state) = backend();
        for bad in ["../escape", "a/../../escape", "/abs/path", "..", ".", ""] {
            assert!(
                matches!(
                    b.write(bad, b"x"),
                    Err(StorageError::PathOutsideRoot { .. })
                ),
                "expected PathOutsideRoot for {bad:?}"
            );
        }
        // "a/../b" normalizes to "b" and stays inside the prefix.
        b.write("a/../b", b"ok").unwrap();
        assert_eq!(b.read("b").unwrap(), b"ok");
        assert!(
            state
                .borrow()
                .requests
                .iter()
                .all(|(_, u)| { u.contains("/test-bucket/kaladoc/") && !u.contains("..") })
        );
    }

    #[test]
    fn retries_transient_status_then_succeeds() {
        let (mut b, state) = backend();
        {
            let mut st = state.borrow_mut();
            st.scripted.push_back(503);
            st.scripted.push_back(429);
        }
        b.write("retry.txt", b"data").unwrap();
        // 1 real PUT + 2 scripted retries = 3 requests.
        assert_eq!(state.borrow().requests.len(), 3);
    }

    #[test]
    fn retries_exhausted_maps_to_io() {
        let (mut b, state) = backend();
        {
            let mut st = state.borrow_mut();
            for _ in 0..10 {
                st.scripted.push_back(500);
            }
        }
        assert!(matches!(b.read("x"), Err(StorageError::Io { .. })));
        // 1 initial + 3 retries.
        assert_eq!(state.borrow().requests.len(), 4);
    }

    #[test]
    fn retries_transport_errors() {
        let (mut b, state) = backend();
        {
            let mut st = state.borrow_mut();
            st.transport_errors.push_back("connection refused".into());
        }
        b.write("ok.txt", b"v").unwrap();
        assert_eq!(state.borrow().requests.len(), 2);
    }

    #[test]
    fn permanent_status_maps_to_io() {
        let (mut b, state) = backend();
        {
            let mut st = state.borrow_mut();
            st.scripted.push_back(403);
        }
        assert!(matches!(b.write("k", b"v"), Err(StorageError::Io { .. })));
        // No retry on permanent statuses.
        assert_eq!(state.borrow().requests.len(), 1);
    }

    #[test]
    fn prepare_checks_bucket_head() {
        let (mut b, state) = backend();
        b.prepare().unwrap();
        let st = state.borrow();
        assert_eq!(st.requests.len(), 1);
        assert_eq!(st.requests[0].0, Method::Head);
        assert!(st.requests[0].1.ends_with("/test-bucket"));
        // prepare is idempotent.
        drop(st);
        b.prepare().unwrap();
        assert_eq!(state.borrow().requests.len(), 2);
    }

    #[test]
    fn used_bytes_is_none() {
        let (b, _) = backend();
        assert_eq!(b.used_bytes(), None);
    }

    #[test]
    fn signed_requests_carry_auth_headers() {
        let (b, _) = backend();
        // Capture headers by wrapping transport? The mock discards
        // them — instead verify via a second mock that records.
        struct Recording(Rc<RefCell<Vec<Vec<(String, String)>>>>);
        impl HttpTransport for Recording {
            fn request(&mut self, req: &HttpRequest) -> Result<HttpResponse, TransportError> {
                self.0.borrow_mut().push(req.headers.clone());
                Ok(HttpResponse {
                    status: 200,
                    body: Vec::new(),
                })
            }
        }
        let seen = Rc::new(RefCell::new(Vec::new()));
        let mut b2 =
            RemoteBackend::with_transport(test_config(), Box::new(Recording(Rc::clone(&seen))))
                .unwrap();
        let _ = b2.write("k", b"v");
        let headers = seen.borrow();
        let flat: Vec<&str> = headers[0].iter().map(|(k, _)| k.as_str()).collect();
        assert!(flat.contains(&"authorization"));
        assert!(flat.contains(&"x-amz-date"));
        assert!(flat.contains(&"x-amz-content-sha256"));
        let auth = headers[0]
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKID/"));
        let _ = b;
    }

    #[test]
    fn invalid_config_rejected() {
        assert!(
            RemoteBackend::new(
                RemoteStorageConfig::new("ftp://bad", "b").with_credentials("a", "s")
            )
            .is_err()
        );
        assert!(
            RemoteBackend::new(
                RemoteStorageConfig::new("http://h/base/path", "b").with_credentials("a", "s")
            )
            .is_err()
        );
        assert!(
            RemoteBackend::new(RemoteStorageConfig::new("http://h", "").with_credentials("a", "s"))
                .is_err()
        );
    }
}
