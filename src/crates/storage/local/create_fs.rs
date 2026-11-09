use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::crates::shared_public::platform::{Platform, platform};
use crate::crates::storage::config::StorageConfig;

/// A probed description of a target storage device/partition.
///
/// Produced by [`create_fs_storage`] before formatting or mounting.
/// Fields that can't be determined on the current platform are `None`
/// rather than guessed.
#[derive(Debug, Clone)]
pub struct StorageTarget {
    /// The device or directory path that was probed.
    pub path: PathBuf,
    /// Filesystem to prepare it with (from config, or detected).
    pub filesystem: Option<String>,
    /// Mount point backing this path, if any.
    pub mount_point: Option<PathBuf>,
    /// Total size of the backing filesystem in bytes.
    pub total_bytes: Option<u64>,
    /// Available (free) bytes on the backing filesystem.
    pub free_bytes: Option<u64>,
    /// Whether the backing mount is read-only.
    pub read_only: bool,
    /// Platform this target was probed on.
    pub platform: Platform,
}

/// Error type for filesystem probing and formatting operations.
#[derive(Debug)]
pub enum FsError {
    /// The target path does not exist or is not accessible.
    TargetNotFound { path: PathBuf },
    /// Filesystem name failed validation.
    InvalidFilesystem { name: String },
    /// Filesystem isn't supported on this OS.
    UnsupportedFilesystem { os: &'static str, fs: String },
    /// An external tool (mkfs.*, diskutil, powershell) failed.
    CommandFailed {
        tool: String,
        status: Option<i32>,
        stderr: String,
    },
    /// Tool could not be executed at all.
    CommandSpawn { tool: String, source: io::Error },
    /// Underlying I/O failure.
    Io { path: PathBuf, source: io::Error },
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TargetNotFound { path } => {
                write!(f, "storage target not found: {}", path.display())
            }
            Self::InvalidFilesystem { name } => {
                write!(f, "invalid filesystem name: {name:?}")
            }
            Self::UnsupportedFilesystem { os, fs } => {
                write!(f, "filesystem {fs:?} is not supported on {os}")
            }
            Self::CommandFailed {
                tool,
                status,
                stderr,
            } => {
                let status = status.map(|s| s.to_string()).unwrap_or_else(|| "?".into());
                write!(f, "{tool} failed (exit {status}): {}", stderr.trim())
            }
            Self::CommandSpawn { tool, source } => {
                write!(f, "failed to run {tool}: {source}")
            }
            Self::Io { path, source } => {
                write!(f, "io error on {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for FsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::CommandSpawn { source, .. } | Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Probes `cdisk_path` and returns a [`StorageTarget`] describing it.
///
/// Uses `config.filesystem` as the intended filesystem when set;
/// otherwise reports the filesystem currently backing the path.
pub fn create_fs_storage(
    cdisk_path: PathBuf,
    config: &StorageConfig,
) -> Result<StorageTarget, FsError> {
    if !cdisk_path.exists() {
        return Err(FsError::TargetNotFound { path: cdisk_path });
    }

    let mount = probe_mount(&cdisk_path);
    let (total_bytes, free_bytes) = probe_size(&cdisk_path).map_or((None, None), |(t, f)| {
        (Some(t), Some(f))
    });

    Ok(StorageTarget {
        path: cdisk_path.clone(),
        filesystem: config
            .filesystem
            .clone()
            .or_else(|| mount.as_ref().map(|m| m.fstype.clone())),
        mount_point: mount.as_ref().map(|m| m.mount_point.clone()),
        total_bytes,
        free_bytes,
        read_only: mount.as_ref().is_some_and(|m| m.read_only),
        platform: platform(),
    })
}

/// Info about the mount backing a path.
#[derive(Debug, Clone)]
struct MountInfo {
    mount_point: PathBuf,
    fstype: String,
    read_only: bool,
}

/// Finds which mount backs `path` by parsing `/proc/self/mounts`.
/// Returns `None` on non-Linux or when no entry matches.
fn probe_mount(path: &Path) -> Option<MountInfo> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mounts = std::fs::read_to_string("/proc/self/mounts").ok()?;

        // Pick the mount point that is the longest prefix of the target.
        mounts
            .lines()
            .filter_map(|line| {
                let mut f = line.split_whitespace();
                let _device = f.next()?;
                let mount_point = f.next()?;
                let fstype = f.next()?;
                let opts = f.next()?;
                Some(MountInfo {
                    mount_point: PathBuf::from(mount_point),
                    fstype: fstype.to_string(),
                    read_only: opts.split(',').any(|o| o == "ro"),
                })
            })
            .filter(|m| target.starts_with(&m.mount_point))
            .max_by_key(|m| m.mount_point.components().count())
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = path;
        None
    }
}

/// Returns `(total_bytes, free_bytes)` for the filesystem backing `path`.
/// Uses `df -B1` on Unix; `None` elsewhere.
fn probe_size(path: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        let out = Command::new("df")
            .args(["-B1", "--output=size,avail", path.to_str()?])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().nth(1)?;
        let mut nums = line.split_whitespace().map(|n| n.parse::<u64>().ok());
        let total = nums.next()??;
        let free = nums.next()??;
        Some((total, free))
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Checks if a filesystem type is supported by the underlying OS.
///
/// On Linux this is a static list of common filesystems; ideally it
/// would read `/proc/filesystems` for the kernel's actual list.
/// On Windows/macOS it checks against known volume types.
pub fn fs_is_available(filesystem: &str) -> bool {
    let fs = filesystem.to_lowercase();
    let supported: &[&str] = if platform().is_linux() {
        &[
            "ext2", "ext3", "ext4", "xfs", "btrfs", "f2fs", "reiserfs", "jfs", "nilfs2", "vfat",
            "exfat", "ntfs", "ntfs3", "iso9660", "udf", "tmpfs", "ramfs", "overlay", "squashfs",
            "nfs", "nfs4", "cifs", "smb3", "fuse", "fuseblk", "zfs", "hfs", "hfsplus", "ufs", "9p",
        ]
    } else if platform().is_windows() {
        &["ntfs", "refs", "fat", "fat32", "exfat", "cdfs", "udf"]
    } else if platform().is_macos() {
        &[
            "apfs", "hfs", "hfsplus", "msdos", "exfat", "ntfs", "cd9660", "udf",
        ]
    } else {
        &["ufs", "zfs", "ext4", "xfs", "ntfs"]
    };
    supported.contains(&fs.as_str())
}

/// Checks whether the filesystem backing `path` is in the supported
/// set. More useful than [`fs_is_available`] because it probes the
/// actual volume rather than trusting a user-supplied name.
pub fn path_on_supported_fs(path: &Path) -> bool {
    match probe_mount(path) {
        Some(m) => fs_is_available(&m.fstype),
        // Can't determine the fs — don't block the caller.
        None => true,
    }
}

/// Formats `target_disk` with `filesystem`.
///
/// Destructive — erases all data on the target. Uses the platform's
/// native tool: `mkfs.*` on Linux, `Format-Volume` on Windows,
/// `diskutil eraseVolume` on macOS.
pub fn format_disk(target_disk: &str, filesystem: &str, mount_point: &str) -> Result<(), FsError> {
    let fs = filesystem.to_lowercase();

    if fs.is_empty() || fs.contains('/') || fs.contains('\\') {
        return Err(FsError::InvalidFilesystem {
            name: filesystem.to_string(),
        });
    }
    if !fs_is_available(&fs) {
        return Err(FsError::UnsupportedFilesystem {
            os: platform().os,
            fs: fs.clone(),
        });
    }

    let _ = mount_point; // reserved for logging / future mount step

    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mkfs_tool = match fs.as_str() {
            "fat" | "fat32" | "vfat" => "mkfs.vfat",
            "ext2" => "mkfs.ext2",
            "ext3" => "mkfs.ext3",
            "ext4" => "mkfs.ext4",
            "xfs" => "mkfs.xfs",
            "btrfs" => "mkfs.btrfs",
            "exfat" => "mkfs.exfat",
            "ntfs" => "mkfs.ntfs",
            other => {
                return Err(FsError::UnsupportedFilesystem {
                    os: platform().os,
                    fs: other.to_string(),
                });
            }
        };

        run_tool(mkfs_tool, &["-F", target_disk])
    }

    #[cfg(target_os = "windows")]
    {
        let drive = target_disk.trim_end_matches(':');
        if drive.len() != 1 || !drive.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(FsError::InvalidFilesystem {
                name: format!("Windows target must be a drive letter, got {target_disk:?}"),
            });
        }
        let ps_script = format!(
            "Format-Volume -DriveLetter {} -FileSystem {} -Force -Confirm:$false",
            drive.to_uppercase(),
            fs.to_uppercase()
        );
        run_tool("powershell", &["-NoProfile", "-Command", &ps_script])
    }

    #[cfg(target_os = "macos")]
    {
        let personality = match fs.as_str() {
            "apfs" => "APFS",
            "hfs" | "hfsplus" | "jhfs+" => "JHFS+",
            "exfat" => "ExFAT",
            "fat" | "fat32" | "msdos" => "MS-DOS FAT32",
            "ntfs" => "NTFS",
            other => {
                return Err(FsError::UnsupportedFilesystem {
                    os: platform().os,
                    fs: other.to_string(),
                });
            }
        };
        run_tool(
            "diskutil",
            &["eraseVolume", personality, "Formatted", target_disk],
        )
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "windows",
        target_os = "macos"
    )))]
    {
        Err(FsError::UnsupportedFilesystem {
            os: platform().os,
            fs,
        })
    }
}

/// Runs an external tool, mapping failures into [`FsError`].
fn run_tool(tool: &str, args: &[&str]) -> Result<(), FsError> {
    let output =
        Command::new(tool)
            .args(args)
            .output()
            .map_err(|source| FsError::CommandSpawn {
                tool: tool.to_string(),
                source,
            })?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = if output.stderr.is_empty() {
        String::from_utf8_lossy(&output.stdout).into_owned()
    } else {
        String::from_utf8_lossy(&output.stderr).into_owned()
    };

    Err(FsError::CommandFailed {
        tool: tool.to_string(),
        status: output.status.code(),
        stderr,
    })
}
