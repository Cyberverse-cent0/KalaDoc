//! HTTP transport abstraction for the remote backend.
//!
//! [`RemoteBackend`](super::backend::RemoteBackend) talks to a
//! [`HttpTransport`] trait object rather than a concrete HTTP client.
//! This keeps the backend fully testable without network access —
//! tests inject a mock — while the production transport
//! ([`UreqTransport`]) is a thin synchronous wrapper over `ureq`
//! (pure-Rust, rustls TLS), matching the synchronous
//! [`StorageBackend`](crate::crates::storage::backend::StorageBackend)
//! trait.

use std::time::Duration;

/// HTTP method for a transport request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Get,
    Put,
    Delete,
    Head,
}

impl Method {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
        }
    }
}

/// A single HTTP request. `url` is fully formed (scheme, authority,
/// percent-encoded path, optional query). `headers` are sent verbatim;
/// `body` is the raw payload (empty for GET/HEAD/DELETE-without-body).
#[derive(Debug, Clone)]
pub(crate) struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// A transport-level HTTP response. The backend maps `status` codes
/// onto `StorageError` variants; only the status and body are needed.
#[derive(Debug)]
pub(crate) struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Transport-level failure (connection refused, DNS, TLS, timeout,
/// malformed response). Always treated as *transient* by the retry
/// policy — permanent failures are expressed as HTTP status codes.
#[derive(Debug)]
pub(crate) struct TransportError(pub String);

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "remote transport error: {}", self.0)
    }
}

impl std::error::Error for TransportError {}

/// Synchronous HTTP transport used by [`RemoteBackend`].
///
/// Implementations must not follow redirects implicitly in a way that
/// breaks SigV4 signatures — `UreqTransport` disables redirect
/// following for exactly that reason (a redirected request would need
/// re-signing, which the backend does not do).
pub(crate) trait HttpTransport {
    fn request(&mut self, req: &HttpRequest) -> Result<HttpResponse, TransportError>;
}

/// Production transport over `ureq`.
///
/// Configured with `http_status_as_error(false)` so every HTTP status
/// reaches the backend as a response (the backend decides which
/// statuses are errors), a global timeout, and redirects disabled.
pub(crate) struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    /// Default global timeout applied to each request.
    pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

    pub(crate) fn new() -> Self {
        Self::with_timeout(Self::DEFAULT_TIMEOUT)
    }

    pub(crate) fn with_timeout(timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .max_redirects(0)
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

impl HttpTransport for UreqTransport {
    fn request(&mut self, req: &HttpRequest) -> Result<HttpResponse, TransportError> {
        // ureq types request builders by body capability; normalize
        // every method to `WithBody` so headers/body handling is shared.
        let mut builder = match req.method {
            Method::Put => self.agent.put(&req.url),
            Method::Get => self.agent.get(&req.url).force_send_body(),
            Method::Delete => self.agent.delete(&req.url).force_send_body(),
            Method::Head => self.agent.head(&req.url).force_send_body(),
        };
        for (k, v) in &req.headers {
            builder = builder.header(k.as_str(), v.as_str());
        }

        let result = builder.send(req.body.as_slice());

        let mut response = result.map_err(|e| TransportError(e.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_vec()
            .map_err(|e| TransportError(format!("reading response body: {e}")))?;
        Ok(HttpResponse { status, body })
    }
}
