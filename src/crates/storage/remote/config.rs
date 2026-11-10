//! Remote storage configuration.
//!
//! [`RemoteStorageConfig`] describes an S3-compatible object store
//! (AWS S3, MinIO, R2, GCS S3-interop, ...). It is populated **only**
//! from environment variables — credentials must never come from a
//! config file or be hardcoded, mirroring the secrets policy in
//! [`crate::config`].
//!
//! | Variable                    | Field          | Default                    |
//! |-----------------------------|----------------|----------------------------|
//! | `KALADOC_S3_ENDPOINT`       | `endpoint`     | `https://s3.{region}.amazonaws.com` |
//! | `KALADOC_S3_BUCKET`         | `bucket`       | *(required)*               |
//! | `KALADOC_S3_REGION`         | `region`       | `us-east-1`                |
//! | `KALADOC_S3_PREFIX`         | `prefix`       | `""`                       |
//! | `KALADOC_S3_ACCESS_KEY`     | `access_key`   | *(required)*               |
//! | `KALADOC_S3_SECRET_KEY`     | `secret_key`   | *(required)*               |
//! | `KALADOC_S3_TIMEOUT_SECS`   | `timeout`      | `30`                       |
//! | `KALADOC_S3_MAX_ATTEMPTS`   | `retry.max_attempts` | `4`                  |

use std::fmt;
use std::time::Duration;

/// Bounded retry policy for transient remote failures.
///
/// Retries apply only to transport-level errors (connect failures,
/// timeouts, connection resets) and HTTP `5xx` responses. `4xx`
/// responses are never retried — they indicate caller error
/// (bad credentials, missing bucket, malformed request).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts including the first try (>= 1).
    pub max_attempts: u32,
    /// First backoff delay; doubles each attempt.
    pub base_delay: Duration,
    /// Upper bound for the computed backoff delay.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(8),
        }
    }
}

/// Configuration for an S3-compatible remote storage backend.
///
/// `endpoint` may be left empty to derive
/// `https://s3.{region}.amazonaws.com`. `bucket`, `access_key`, and
/// `secret_key` must be non-empty for the backend to function —
/// [`RemoteStorageConfig::is_complete`] reports whether they are set.
///
/// `max_size_bytes`, when set, enables client-side quota accounting:
/// the backend tracks bytes written through it and reports
/// [`used_bytes`](crate::crates::storage::backend::StorageBackend::used_bytes)
/// as `Some`. Object stores cannot cheaply report total usage, so the
/// counter is in-memory only and may drift if objects are modified
/// out-of-band.
#[derive(Clone)]
pub struct RemoteStorageConfig {
    /// Base URL of the S3 endpoint, e.g. `https://s3.us-east-1.amazonaws.com`
    /// or `http://localhost:9000` for MinIO. Empty = derive from `region`.
    pub endpoint: String,
    /// Target bucket. All object names are confined under it.
    pub bucket: String,
    /// AWS region used for SigV4 request signing.
    pub region: String,
    /// Optional key prefix inside the bucket (e.g. `"kaladoc"`).
    /// `""` stores objects at the bucket root.
    pub prefix: String,
    /// Access key id. Environment-sourced only.
    pub access_key: String,
    /// Secret access key. Environment-sourced only; never logged.
    pub secret_key: String,
    /// Per-request timeout for the HTTP transport.
    pub timeout: Duration,
    /// Retry policy for transient failures.
    pub retry: RetryPolicy,
    /// Optional client-side quota in bytes (see struct docs).
    pub max_size_bytes: Option<u64>,
}

/// Debug output that never reveals credentials.
impl fmt::Debug for RemoteStorageConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteStorageConfig")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("prefix", &self.prefix)
            .field("access_key", &self.access_key)
            .field("secret_key", &"<redacted>")
            .field("timeout", &self.timeout)
            .field("retry", &self.retry)
            .field("max_size_bytes", &self.max_size_bytes)
            .finish()
    }
}

impl Default for RemoteStorageConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            bucket: String::new(),
            region: "us-east-1".to_string(),
            prefix: String::new(),
            access_key: String::new(),
            secret_key: String::new(),
            timeout: Duration::from_secs(30),
            retry: RetryPolicy::default(),
            max_size_bytes: None,
        }
    }
}

impl RemoteStorageConfig {
    /// Builder: set credentials (normally populated from env vars).
    pub fn with_credentials(
        mut self,
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
    ) -> Self {
        self.access_key = access_key.into();
        self.secret_key = secret_key.into();
        self
    }

    /// Builder: set the key prefix inside the bucket.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Builder: enable client-side quota accounting.
    pub fn with_quota(mut self, bytes: u64) -> Self {
        self.max_size_bytes = Some(bytes);
        self
    }

    /// `true` when every field the backend needs is populated.
    pub fn is_complete(&self) -> bool {
        !self.bucket.is_empty() && !self.access_key.is_empty() && !self.secret_key.is_empty()
    }

    /// Effective endpoint URL: the configured one, or the regional AWS
    /// S3 endpoint derived from `region`.
    pub fn resolved_endpoint(&self) -> String {
        if !self.endpoint.trim().is_empty() {
            self.endpoint.trim_end_matches('/').to_string()
        } else {
            format!("https://s3.{}.amazonaws.com", self.region)
        }
    }

    /// Builds a config from environment variables. See the module docs
    /// for the variable names.
    pub fn from_env() -> Self {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// Builds a config by looking up each variable through `lookup`.
    ///
    /// Split out from [`RemoteStorageConfig::from_env`] so tests can
    /// exercise parsing without mutating process environment (which is
    /// racy under parallel tests). `lookup` should only ever be backed
    /// by environment variables — credentials must not come from files.
    pub fn from_vars<F>(lookup: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        let get = |name: &str| {
            lookup(name)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        let mut cfg = Self::default();
        if let Some(v) = get("KALADOC_S3_ENDPOINT") {
            cfg.endpoint = v;
        }
        if let Some(v) = get("KALADOC_S3_BUCKET") {
            cfg.bucket = v;
        }
        if let Some(v) = get("KALADOC_S3_REGION") {
            cfg.region = v;
        }
        if let Some(v) = get("KALADOC_S3_PREFIX") {
            cfg.prefix = v;
        }
        if let Some(v) = get("KALADOC_S3_ACCESS_KEY") {
            cfg.access_key = v;
        }
        if let Some(v) = get("KALADOC_S3_SECRET_KEY") {
            cfg.secret_key = v;
        }
        if let Some(v) = get("KALADOC_S3_TIMEOUT_SECS")
            && let Ok(secs) = v.parse::<u64>()
            && secs > 0
        {
            cfg.timeout = Duration::from_secs(secs);
        }
        if let Some(v) = get("KALADOC_S3_MAX_ATTEMPTS")
            && let Ok(n) = v.parse::<u32>()
            && n > 0
        {
            cfg.retry.max_attempts = n;
        }
        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(map: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = map
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn from_vars_reads_all_fields() {
        let cfg = RemoteStorageConfig::from_vars(lookup(&[
            ("KALADOC_S3_ENDPOINT", "http://minio.local:9000/"),
            ("KALADOC_S3_BUCKET", "docs"),
            ("KALADOC_S3_REGION", "eu-west-1"),
            ("KALADOC_S3_PREFIX", "kaladoc/"),
            ("KALADOC_S3_ACCESS_KEY", "AKID"),
            ("KALADOC_S3_SECRET_KEY", "s3cr3t"),
            ("KALADOC_S3_TIMEOUT_SECS", "10"),
            ("KALADOC_S3_MAX_ATTEMPTS", "2"),
        ]));

        assert_eq!(cfg.endpoint, "http://minio.local:9000/");
        assert_eq!(cfg.bucket, "docs");
        assert_eq!(cfg.region, "eu-west-1");
        assert_eq!(cfg.prefix, "kaladoc/");
        assert_eq!(cfg.access_key, "AKID");
        assert_eq!(cfg.secret_key, "s3cr3t");
        assert_eq!(cfg.timeout, Duration::from_secs(10));
        assert_eq!(cfg.retry.max_attempts, 2);
        assert!(cfg.is_complete());
    }

    #[test]
    fn from_vars_defaults_and_empty_values() {
        let cfg = RemoteStorageConfig::from_vars(lookup(&[
            ("KALADOC_S3_BUCKET", "docs"),
            ("KALADOC_S3_ACCESS_KEY", "AKID"),
            ("KALADOC_S3_SECRET_KEY", "s3cr3t"),
            // Empty strings are treated as unset.
            ("KALADOC_S3_PREFIX", "   "),
            ("KALADOC_S3_TIMEOUT_SECS", "not-a-number"),
            ("KALADOC_S3_MAX_ATTEMPTS", "0"),
        ]));

        assert_eq!(cfg.region, "us-east-1");
        assert_eq!(cfg.prefix, "");
        assert_eq!(cfg.timeout, Duration::from_secs(30));
        assert_eq!(cfg.retry.max_attempts, RetryPolicy::default().max_attempts);
        // Endpoint empty → derived from region.
        assert_eq!(
            cfg.resolved_endpoint(),
            "https://s3.us-east-1.amazonaws.com"
        );
    }

    #[test]
    fn is_complete_requires_bucket_and_credentials() {
        assert!(!RemoteStorageConfig::default().is_complete());
        let cfg = RemoteStorageConfig::default()
            .with_credentials("AK", "SK")
            .with_prefix("p");
        // Still missing bucket.
        assert!(!cfg.is_complete());
    }

    #[test]
    fn debug_redacts_secret_key() {
        let cfg = RemoteStorageConfig::default()
            .with_credentials("AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY");
        let dbg = format!("{cfg:?}");
        assert!(dbg.contains("<redacted>"));
        assert!(!dbg.contains("wJalrXUtnFEMI"));
    }
}
