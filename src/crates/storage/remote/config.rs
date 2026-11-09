//! Configuration for the remote storage backend.
//!
//! All values come from the environment (or explicit builders in
//! tests/tools) — credentials are never hardcoded. See
//! [`RemoteStorageConfig::from_env`] for the variable names, which
//! follow the `KALADOC_REMOTE_*` convention mirroring the local
//! backend's `KALADOC_STORAGE_*` variables.

/// Configuration for an S3-compatible object-store backend.
///
/// `endpoint` is the scheme + authority of the object store, e.g.
/// `https://s3.us-east-1.amazonaws.com` or `http://minio.local:9000`.
/// Requests use path-style addressing (`{endpoint}/{bucket}/{key}`),
/// which works with AWS S3 and S3-compatible stores (MinIO, Ceph,
/// Garage, ...). Do not include a path component in `endpoint`;
/// object keys are placed under `prefix` inside `bucket`.
#[derive(Debug, Clone)]
pub struct RemoteStorageConfig {
    /// Base URL of the object store: `scheme://authority` only.
    pub endpoint: String,
    /// Bucket all object names are confined to.
    pub bucket: String,
    /// Optional key prefix prepended to every object name
    /// (e.g. `"kaladoc/production"`). Stored without leading or
    /// trailing slashes.
    pub prefix: String,
    /// S3 access key id used for request signing.
    pub access_key: String,
    /// S3 secret access key used for request signing.
    pub secret_key: String,
    /// AWS region used in the SigV4 credential scope. For S3-compatible
    /// stores that ignore region (MinIO, etc.) any non-empty value works.
    pub region: String,
    /// How many times a transiently-failed request is retried before
    /// giving up. `0` disables retries.
    pub max_retries: u32,
    /// Base delay in milliseconds for exponential backoff between
    /// retries (doubled per attempt, capped at 30s).
    pub initial_backoff_ms: u64,
}

impl Default for RemoteStorageConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            bucket: String::new(),
            prefix: String::new(),
            access_key: String::new(),
            secret_key: String::new(),
            region: "us-east-1".to_string(),
            max_retries: 3,
            initial_backoff_ms: 100,
        }
    }
}

impl RemoteStorageConfig {
    /// Minimal constructor; credentials and prefix are set via the
    /// builder methods or [`RemoteStorageConfig::from_env`].
    pub fn new(endpoint: impl Into<String>, bucket: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            bucket: bucket.into(),
            ..Default::default()
        }
    }

    /// Builder: attach credentials for SigV4 request signing.
    pub fn with_credentials(
        mut self,
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
    ) -> Self {
        self.access_key = access_key.into();
        self.secret_key = secret_key.into();
        self
    }

    /// Builder: set the key prefix objects are stored under.
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Builder: set the AWS region for the SigV4 scope.
    pub fn with_region(mut self, region: impl Into<String>) -> Self {
        self.region = region.into();
        self
    }

    /// Builder: set the retry policy (attempts after the first try,
    /// base backoff in milliseconds).
    pub fn with_retry(mut self, max_retries: u32, initial_backoff_ms: u64) -> Self {
        self.max_retries = max_retries;
        self.initial_backoff_ms = initial_backoff_ms;
        self
    }

    /// Normalizes `prefix` to have no leading or trailing slashes.
    pub(crate) fn normalized_prefix(&self) -> &str {
        self.prefix.trim_matches('/')
    }

    /// Builds a config from environment variables:
    /// `KALADOC_REMOTE_ENDPOINT`, `KALADOC_REMOTE_BUCKET`,
    /// `KALADOC_REMOTE_PREFIX`, `KALADOC_REMOTE_ACCESS_KEY`,
    /// `KALADOC_REMOTE_SECRET_KEY`, `KALADOC_REMOTE_REGION`,
    /// `KALADOC_REMOTE_MAX_RETRIES`, `KALADOC_REMOTE_RETRY_BACKOFF_MS`.
    ///
    /// Follows the same "defaults + optional overrides" pattern as
    /// [`crate::crates::storage::local::config::StorageConfig::from_env`]:
    /// unset variables keep their defaults, so callers must still
    /// validate that `endpoint`/`bucket`/credentials are populated
    /// before constructing a backend.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Ok(v) = std::env::var("KALADOC_REMOTE_ENDPOINT") {
            cfg.endpoint = v.trim().trim_end_matches('/').to_string();
        }
        if let Ok(v) = std::env::var("KALADOC_REMOTE_BUCKET") {
            cfg.bucket = v.trim().to_string();
        }
        if let Ok(v) = std::env::var("KALADOC_REMOTE_PREFIX") {
            cfg.prefix = v.trim().to_string();
        }
        if let Ok(v) = std::env::var("KALADOC_REMOTE_ACCESS_KEY") {
            cfg.access_key = v;
        }
        if let Ok(v) = std::env::var("KALADOC_REMOTE_SECRET_KEY") {
            cfg.secret_key = v;
        }
        if let Ok(v) = std::env::var("KALADOC_REMOTE_REGION")
            && !v.trim().is_empty()
        {
            cfg.region = v.trim().to_string();
        }
        if let Ok(v) = std::env::var("KALADOC_REMOTE_MAX_RETRIES") {
            cfg.max_retries = v.parse().unwrap_or(cfg.max_retries);
        }
        if let Ok(v) = std::env::var("KALADOC_REMOTE_RETRY_BACKOFF_MS") {
            cfg.initial_backoff_ms = v.parse().unwrap_or(cfg.initial_backoff_ms);
        }

        cfg
    }
}
