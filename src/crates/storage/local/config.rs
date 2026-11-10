use std::path::PathBuf;

/// Configuration for the storage engine.
#[derive(Debug, Clone)]
pub struct StorageConfig {
    /// Root directory all storage operations are confined to.
    pub root_dir: PathBuf,
    /// Optional cap on total bytes stored under `root_dir`.
    pub max_size_bytes: Option<u64>,
    /// Filesystem to use when preparing/formatting the target storage
    /// (e.g. "ext4", "xfs", "ntfs", "apfs"). `None` = don't format.
    pub filesystem: Option<String>,
    /// Master switch for fault injection (debug builds / tests only).
    pub fault_injection: FaultInjectionConfig,
    /// Content-addressed storage / dedup (see
    /// [`crate::crates::storage::dedup`]). When `true`, committed
    /// objects can be registered under `cas/<sha256>` so duplicate
    /// content is stored once. Default `false` — opt-in.
    pub dedup: bool,
}

/// Controls deterministic fault injection so disk-full, permission,
/// and IO failures can be exercised without real hardware faults.
#[derive(Debug, Clone, Default)]
pub struct FaultInjectionConfig {
    pub enabled: bool,
    /// Inject a fault on every Nth storage operation (0 = never).
    pub every_n_ops: u32,
    /// Which fault kinds are eligible for injection.
    pub allowed_kinds: Vec<FaultKind>,
}

/// A simulated failure mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    /// Simulates ENOSPC — the disk is full.
    DiskFull,
    /// Simulates EACCES — permission denied.
    PermissionDenied,
    /// Simulates a generic EIO.
    IoError,
    /// Simulates a stalled/slow device (sleep before failing).
    Timeout,
    /// Write appears to succeed but data is corrupted on read-back.
    CorruptedWrite,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("./storage"),
            max_size_bytes: None,
            filesystem: None,
            fault_injection: FaultInjectionConfig::default(),
            dedup: false,
        }
    }
}

impl FaultInjectionConfig {
    /// Sensible test preset: fail every 3rd op with DiskFull or IoError.
    pub fn disk_full_every_3() -> Self {
        Self {
            enabled: true,
            every_n_ops: 3,
            allowed_kinds: vec![FaultKind::DiskFull, FaultKind::IoError],
        }
    }
}

impl StorageConfig {
    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        Self {
            root_dir: root_dir.into(),
            ..Default::default()
        }
    }

    /// Builder: set the storage quota in bytes.
    pub fn with_quota(mut self, bytes: u64) -> Self {
        self.max_size_bytes = Some(bytes);
        self
    }

    /// Builder: attach a fault-injection preset.
    pub fn with_faults(mut self, faults: FaultInjectionConfig) -> Self {
        self.fault_injection = faults;
        self
    }

    /// Builder: set the target filesystem for disk preparation.
    pub fn with_filesystem(mut self, fs: impl Into<String>) -> Self {
        self.filesystem = Some(fs.into());
        self
    }

    /// Builder: enable/disable content-addressed storage dedup.
    /// See [`crate::crates::storage::dedup`] for the on-disk layout.
    pub fn with_dedup(mut self, enabled: bool) -> Self {
        self.dedup = enabled;
        self
    }

    /// Builds a config from environment variables:
    /// `KALADOC_STORAGE_DIR`, `KALADOC_STORAGE_MAX_BYTES`,
    /// `KALADOC_STORAGE_DEDUP` ("1"/"0"),
    /// `KALADOC_FAULT_INJECTION` ("1"/"0"), `KALADOC_FAULT_EVERY_N`.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Ok(dir) = std::env::var("KALADOC_STORAGE_DIR") {
            cfg.root_dir = PathBuf::from(dir);
        }
        if let Ok(bytes) = std::env::var("KALADOC_STORAGE_MAX_BYTES") {
            cfg.max_size_bytes = bytes.parse().ok();
        }
        if let Ok(fs) = std::env::var("KALADOC_FS_TYPE")
            && !fs.trim().is_empty()
        {
            cfg.filesystem = Some(fs.trim().to_lowercase());
        }
        if let Ok(v) = std::env::var("KALADOC_STORAGE_DEDUP") {
            cfg.dedup = matches!(v.as_str(), "1" | "true" | "yes");
        }
        if let Ok(v) = std::env::var("KALADOC_FAULT_INJECTION") {
            cfg.fault_injection.enabled = matches!(v.as_str(), "1" | "true" | "yes");
        }
        if let Ok(n) = std::env::var("KALADOC_FAULT_EVERY_N") {
            cfg.fault_injection.every_n_ops = n.parse().unwrap_or(0);
            if cfg.fault_injection.every_n_ops > 0 && cfg.fault_injection.allowed_kinds.is_empty() {
                cfg.fault_injection.allowed_kinds = vec![FaultKind::DiskFull, FaultKind::IoError];
            }
        }

        cfg
    }
}
