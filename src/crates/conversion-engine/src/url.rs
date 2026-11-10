//! URL fetching for `url` → `pdf` conversions.
//!
//! [`Format::Url`](crate::crates::conversion_engine::Format::Url)
//! requests carry the target URL as UTF-8 bytes in
//! [`ConvertRequest::input`](crate::crates::conversion_engine::ConvertRequest).
//! [`fetch_url`] retrieves the document over HTTP(S) with ureq (already
//! a workspace dependency, rustls TLS) under strict guards:
//!
//! - only `http://` and `https://` schemes are accepted — `file:`,
//!   `ftp:`, `javascript:`, etc. are rejected before any network I/O
//! - redirects are followed manually (the agent is configured with
//!   `max_redirects(0)`) so every hop's `Location` is re-validated:
//!   at most [`MAX_REDIRECTS`] hops, each target must again be an
//!   http(s) URL — a redirect to `file:` or another scheme fails
//! - the whole exchange is bounded by [`FETCH_TIMEOUT`]
//! - the response body is capped at [`MAX_FETCH_BYTES`] (the same
//!   256 MiB the engine advertises via `EngineLimits::max_input_bytes`)
//!
//! The fetched bytes are treated as HTML and handed to the normal
//! backend dispatch (`html` → `pdf`), so URL conversion inherits
//! whatever HTML→PDF capability the environment provides (pandoc +
//! PDF engine today; the pure-Rust backend may grow it later).

use std::io::Read;
use std::time::Duration;

use super::engine::MAX_INPUT_BYTES;
use super::error::{ConvertError, ConvertResult};

/// Maximum accepted URL length in bytes. URLs longer than this are
/// rejected before any network I/O.
pub(crate) const MAX_URL_BYTES: usize = 2048;

/// Maximum redirects followed per fetch. Each hop is re-validated, so
/// a redirect loop cannot spin forever.
pub(crate) const MAX_REDIRECTS: u32 = 5;

/// Overall timeout for a single HTTP request (connect + response).
/// The redirect chain as a whole is bounded by `MAX_REDIRECTS`
/// sequential requests, each with this budget.
pub(crate) const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Response body cap. Matches the engine-level input limit
/// ([`MAX_INPUT_BYTES`], 256 MiB) so a fetched page can never exceed
/// what `html` → `pdf` would accept anyway.
const MAX_FETCH_BYTES: u64 = MAX_INPUT_BYTES;

/// Human-readable form of [`MAX_FETCH_BYTES`] for error messages.
const MAX_FETCH_BYTES_LABEL: &str = "256 MiB";

/// Identify ourselves politely; some hosts reject empty UAs.
const USER_AGENT: &str = "KalaDoc conversion-engine (+https://kaladoc)";

/// Validate `raw` as a fetchable URL and return it normalized (trimmed).
///
/// Rules: non-empty, ≤ [`MAX_URL_BYTES`], plain ASCII with no
/// whitespace or control characters, and an `http://` or `https://`
/// scheme with a non-empty authority. Anything else is
/// [`ConvertError::InvalidInput`].
pub(crate) fn validate_url(raw: &str) -> ConvertResult<String> {
    let url = raw.trim();
    if url.is_empty() {
        return Err(ConvertError::InvalidInput {
            reason: "url is empty".to_string(),
        });
    }
    if url.len() > MAX_URL_BYTES {
        return Err(ConvertError::InvalidInput {
            reason: format!("url exceeds {MAX_URL_BYTES} bytes"),
        });
    }
    if !url.is_ascii() || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(ConvertError::InvalidInput {
            reason: "url must be plain ASCII http(s) without whitespace".to_string(),
        });
    }
    let scheme_end = url.find("://").ok_or_else(|| ConvertError::InvalidInput {
        reason: "url must include a scheme (http:// or https://)".to_string(),
    })?;
    let scheme = &url[..scheme_end];
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err(ConvertError::InvalidInput {
            reason: format!("unsupported url scheme `{scheme}` (only http/https allowed)"),
        });
    }
    let authority = &url[scheme_end + 3..];
    let host = authority.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() {
        return Err(ConvertError::InvalidInput {
            reason: "url has no host".to_string(),
        });
    }
    Ok(url.to_string())
}

/// Build the shared ureq agent: no automatic redirects (we follow
/// them manually to re-validate each target), no error-on-status
/// (we classify statuses ourselves), one global timeout per request.
fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .http_status_as_error(false)
        .max_redirects(0)
        .build();
    config.into()
}

/// Extract the `Location` header of a redirect response.
fn redirect_location(response: &ureq::http::Response<ureq::Body>) -> Option<String> {
    response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Whether `location` carries an explicit non-http(s) scheme
/// (`file:`, `javascript:`, `data:`, ...). RFC 3986 scheme syntax is
/// `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ) ":"`; anything matching
/// that before the first `:` is a scheme, never a relative path.
fn has_foreign_scheme(location: &str) -> bool {
    let lower = location.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return false;
    }
    match location.find(':') {
        Some(colon) => {
            let scheme = &location[..colon];
            !scheme.is_empty()
                && scheme
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        }
        None => false,
    }
}

/// Resolve a `Location` header value against `base`.
///
/// Handles absolute http(s) URLs, scheme-relative `//host/path`,
/// absolute-path `/path`, and simple relative paths. Anything that
/// does not resolve to a valid http(s) URL is rejected — redirect
/// targets are never trusted.
fn resolve_location(base: &str, location: &str) -> ConvertResult<String> {
    if has_foreign_scheme(location) {
        return Err(ConvertError::InvalidInput {
            reason: "redirect target uses a non-http(s) scheme".to_string(),
        });
    }
    let lower = location.to_ascii_lowercase();
    let resolved = if lower.starts_with("http://") || lower.starts_with("https://") {
        location.to_string()
    } else if let Some(rest) = location.strip_prefix("//") {
        // Scheme-relative: inherit the base scheme.
        let base_scheme = base
            .split("://")
            .next()
            .unwrap_or("https")
            .to_ascii_lowercase();
        format!("{base_scheme}://{rest}")
    } else if location.starts_with('/') {
        // Absolute path: keep scheme + authority of the base URL.
        let after_scheme = &base[base.find("://").map(|i| i + 3).unwrap_or(0)..];
        let authority_end = after_scheme
            .find(['/', '?', '#'])
            .unwrap_or(after_scheme.len());
        let scheme = &base[..base.find("://").unwrap_or(0)];
        format!("{scheme}://{}{}", &after_scheme[..authority_end], location)
    } else {
        // Bare relative path: resolve against the base's directory.
        // Query/fragment of the base are dropped; `..` segments are
        // left for the server to interpret (ureq rejects malformed URIs).
        let no_frag = base.split('#').next().unwrap_or(base);
        let no_query = no_frag.split('?').next().unwrap_or(no_frag);
        let dir_end = no_query.rfind('/').map(|i| i + 1).unwrap_or(no_query.len());
        format!("{}{}", &no_query[..dir_end], location)
    };
    validate_url(&resolved)
}

/// Send one GET request for an already-validated URL.
fn send(agent: &ureq::Agent, url: &str) -> ConvertResult<ureq::http::Response<ureq::Body>> {
    let request = ureq::http::Request::builder()
        .method("GET")
        .uri(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "text/html,application/xhtml+xml,*/*;q=0.8")
        .body(Vec::new())
        .map_err(|e| ConvertError::InvalidInput {
            reason: format!("could not build request for url: {e}"),
        })?;
    agent.run(request).map_err(|e| match e {
        ureq::Error::Io(io) => ConvertError::Io(io),
        other => ConvertError::BackendFailed {
            backend: "url".to_string(),
            message: format!("request failed: {other}"),
        },
    })
}

/// Fetch `raw_url` and return the response body, following up to
/// [`MAX_REDIRECTS`] validated http(s) redirects.
///
/// The URL is validated before the first request and every redirect
/// target is re-validated, so non-http(s) schemes can never be
/// reached — including via `Location` headers.
pub(crate) fn fetch_url(raw_url: &str) -> ConvertResult<Vec<u8>> {
    let mut url = validate_url(raw_url)?;
    let agent = agent();

    for _ in 0..=MAX_REDIRECTS {
        let mut response = send(&agent, &url)?;
        let status = response.status().as_u16();

        if (300..400).contains(&status) {
            let location =
                redirect_location(&response).ok_or_else(|| ConvertError::BackendFailed {
                    backend: "url".to_string(),
                    message: format!("server returned HTTP {status} without a Location header"),
                })?;
            url = resolve_location(&url, &location)?;
            continue;
        }

        if !(200..300).contains(&status) {
            return Err(ConvertError::BackendFailed {
                backend: "url".to_string(),
                message: format!("server returned HTTP status {status}"),
            });
        }

        let mut body = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(MAX_FETCH_BYTES + 1)
            .read_to_end(&mut body)
            .map_err(ConvertError::Io)?;
        if body.len() as u64 > MAX_FETCH_BYTES {
            return Err(ConvertError::TooLarge {
                limit: MAX_FETCH_BYTES_LABEL.to_string(),
                actual: body.len() as u64,
            });
        }
        if body.is_empty() {
            return Err(ConvertError::BackendFailed {
                backend: "url".to_string(),
                message: "server returned an empty body".to_string(),
            });
        }
        return Ok(body);
    }

    Err(ConvertError::BackendFailed {
        backend: "url".to_string(),
        message: format!("too many redirects (limit {MAX_REDIRECTS})"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_http_and_https() {
        assert_eq!(
            validate_url("https://example.com/page?x=1#frag").unwrap(),
            "https://example.com/page?x=1#frag"
        );
        assert_eq!(
            validate_url("  http://example.com  ").unwrap(),
            "http://example.com"
        );
        assert_eq!(
            validate_url("HTTP://EXAMPLE.COM/A").unwrap(),
            "HTTP://EXAMPLE.COM/A"
        );
    }

    #[test]
    fn validate_rejects_non_http_schemes() {
        for bad in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "javascript:alert(1)",
            "data:text/html,<h1>x</h1>",
            "gopher://example.com",
        ] {
            assert!(
                matches!(validate_url(bad), Err(ConvertError::InvalidInput { .. })),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn validate_rejects_malformed_urls() {
        for bad in [
            "",
            "   ",
            "example.com/no-scheme",
            "https://", // no host
            "https:///path-only",
            "https://example.com/has space",
            "https://example.com/line\nbreak",
            "https://exämple.com/üñí",
        ] {
            assert!(
                matches!(validate_url(bad), Err(ConvertError::InvalidInput { .. })),
                "{bad:?} should be rejected"
            );
        }
        // Over-length URL.
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES));
        assert!(matches!(
            validate_url(&long),
            Err(ConvertError::InvalidInput { .. })
        ));
    }

    #[test]
    fn resolve_absolute_and_relative_locations() {
        let base = "https://example.com/dir/page.html?x=1";
        assert_eq!(
            resolve_location(base, "https://other.com/y").unwrap(),
            "https://other.com/y"
        );
        assert_eq!(
            resolve_location(base, "HTTP://other.com/y").unwrap(),
            "HTTP://other.com/y"
        );
        assert_eq!(
            resolve_location(base, "//cdn.example.com/a.js").unwrap(),
            "https://cdn.example.com/a.js"
        );
        assert_eq!(
            resolve_location(base, "/root.html").unwrap(),
            "https://example.com/root.html"
        );
        assert_eq!(
            resolve_location(base, "next.html").unwrap(),
            "https://example.com/dir/next.html"
        );
        // Redirect to a non-http(s) scheme is refused outright.
        for evil in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ftp://example.com/x",
            "data:text/html,x",
        ] {
            assert!(
                matches!(
                    resolve_location(base, evil),
                    Err(ConvertError::InvalidInput { .. })
                ),
                "{evil} redirect should be rejected"
            );
        }
    }

    /// Serve exactly one HTTP response on an ephemeral localhost port.
    /// Returns the port; the server thread exits after one request.
    fn serve_once(response: &str) -> u16 {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let response = response.to_string();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            // Drain request headers (and ignore any body — GET has none).
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
            }
            let mut stream = stream;
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
        });
        port
    }

    #[test]
    fn fetch_url_retrieves_local_http_body() {
        let body = "<html><body><h1>hello</h1></body></html>";
        let port = serve_once(&format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        ));
        let fetched = fetch_url(&format!("http://127.0.0.1:{port}/page")).unwrap();
        assert_eq!(fetched, body.as_bytes());
    }

    #[test]
    fn fetch_url_rejects_redirect_to_non_http_scheme() {
        // Server answers with a redirect to file:// — the fetch must
        // fail without ever issuing a second request.
        let port = serve_once(
            "HTTP/1.1 302 Found\r\nLocation: file:///etc/passwd\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let err = fetch_url(&format!("http://127.0.0.1:{port}/")).unwrap_err();
        assert!(matches!(
            err,
            ConvertError::InvalidInput { .. } | ConvertError::BackendFailed { .. }
        ));
    }

    #[test]
    fn fetch_url_rejects_connection_refused() {
        // Bind-then-drop to get a port that is (almost surely) closed.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        assert!(fetch_url(&format!("http://127.0.0.1:{port}/")).is_err());
    }
}
