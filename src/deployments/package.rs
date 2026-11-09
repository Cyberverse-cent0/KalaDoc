//! Release packaging: stage a binary plus per-OS assets into a
//! directory, then emit `.tar.gz` and `.zip` archives with checksums.
//!
//! The `.tar.gz` is produced by the system `tar` (checked via
//! [`require::is_installed`]); the `.zip` uses the dependency-free
//! writer in [`crate::deployments::archive`]. Both formats are produced
//! for every target — consumers pick whichever their platform prefers.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::deployments::arch;
use crate::deployments::archive::{self, ZipFile};
use crate::deployments::require;

// ============================================================
// Targets
// ============================================================

/// A release target: an OS/arch pair mapped to a Rust target triple.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReleaseTarget {
    LinuxX86_64,
    LinuxAarch64,
    MacosX86_64,
    MacosAarch64,
    WindowsX86_64,
}

impl ReleaseTarget {
    /// All supported targets, in a stable order.
    pub fn all() -> &'static [ReleaseTarget] {
        &[
            ReleaseTarget::LinuxX86_64,
            ReleaseTarget::LinuxAarch64,
            ReleaseTarget::MacosX86_64,
            ReleaseTarget::MacosAarch64,
            ReleaseTarget::WindowsX86_64,
        ]
    }

    /// The Rust `--target` triple used for `cargo build`.
    pub fn triple(self) -> &'static str {
        match self {
            ReleaseTarget::LinuxX86_64 => "x86_64-unknown-linux-gnu",
            ReleaseTarget::LinuxAarch64 => "aarch64-unknown-linux-gnu",
            ReleaseTarget::MacosX86_64 => "x86_64-apple-darwin",
            ReleaseTarget::MacosAarch64 => "aarch64-apple-darwin",
            ReleaseTarget::WindowsX86_64 => "x86_64-pc-windows-msvc",
        }
    }

    /// Short slug used in file names, e.g. `linux-x86_64`.
    pub fn slug(self) -> &'static str {
        match self {
            ReleaseTarget::LinuxX86_64 => "linux-x86_64",
            ReleaseTarget::LinuxAarch64 => "linux-aarch64",
            ReleaseTarget::MacosX86_64 => "macos-x86_64",
            ReleaseTarget::MacosAarch64 => "macos-aarch64",
            ReleaseTarget::WindowsX86_64 => "windows-x86_64",
        }
    }

    /// Binary name inside the package: `kaladoc` or `kaladoc.exe`.
    pub fn binary_name(self, package_name: &str) -> String {
        match self {
            ReleaseTarget::WindowsX86_64 => format!("{package_name}.exe"),
            _ => package_name.to_string(),
        }
    }

    /// Parses a slug produced by [`ReleaseTarget::slug`].
    pub fn from_slug(slug: &str) -> Option<Self> {
        Self::all().iter().copied().find(|t| t.slug() == slug)
    }
}

// ============================================================
// Spec
// ============================================================

/// Everything needed to produce one target's archives.
#[derive(Debug, Clone)]
pub struct PackageSpec {
    /// Path to the compiled binary on disk.
    pub binary: PathBuf,
    /// Release version, e.g. `0.2.0` (no leading `v`).
    pub version: String,
    pub target: ReleaseTarget,
    /// Directory the archives are written to.
    pub out_dir: PathBuf,
    /// Distribution name used for the package dir and binary.
    pub package_name: String,
    /// Extra files to include: `(source path, archive-relative name)`.
    pub assets: Vec<(PathBuf, String)>,
}

impl PackageSpec {
    pub fn new(binary: impl Into<PathBuf>, version: &str, target: ReleaseTarget) -> Self {
        Self {
            binary: binary.into(),
            version: version.to_string(),
            target,
            out_dir: PathBuf::from("dist"),
            package_name: "kaladoc".to_string(),
            assets: Vec::new(),
        }
    }

    pub fn with_out_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.out_dir = dir.into();
        self
    }

    pub fn with_package_name(mut self, name: &str) -> Self {
        self.package_name = name.to_string();
        self
    }

    /// Adds an extra file to the package root, e.g.
    /// `.with_asset("LICENSE", "LICENSE")`.
    pub fn with_asset(mut self, source: impl Into<PathBuf>, name: &str) -> Self {
        self.assets.push((source.into(), name.to_string()));
        self
    }

    /// `kaladoc-0.2.0-linux-x86_64`.
    pub fn package_dir_name(&self) -> String {
        format!(
            "{}-{}-{}",
            self.package_name,
            self.version,
            self.target.slug()
        )
    }
}

// ============================================================
// Results / errors
// ============================================================

/// A produced archive plus its digest.
#[derive(Debug, Clone, Serialize)]
pub struct ArtifactFile {
    pub path: PathBuf,
    pub sha256: String,
    pub size_bytes: u64,
}

/// Both archives for one target.
#[derive(Debug)]
pub struct PackageArtifacts {
    pub package_dir_name: String,
    pub target: ReleaseTarget,
    pub tar_gz: ArtifactFile,
    pub zip: ArtifactFile,
}

#[derive(Debug)]
pub enum PackageError {
    Io(io::Error),
    MissingTool(String),
    Command {
        program: String,
        args: Vec<String>,
        status: Option<i32>,
        stderr: String,
    },
    InvalidInput(String),
}

impl std::fmt::Display for PackageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::MissingTool(t) => write!(f, "required tool not found on PATH: {t}"),
            Self::Command {
                program,
                args,
                status,
                stderr,
            } => write!(
                f,
                "`{program} {}` failed (status {status:?}): {stderr}",
                args.join(" ")
            ),
            Self::InvalidInput(m) => write!(f, "invalid input: {m}"),
        }
    }
}

impl std::error::Error for PackageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for PackageError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub type PackageResult<T> = Result<T, PackageError>;

// ============================================================
// Packaging
// ============================================================

/// Stages the binary + assets and writes `.tar.gz` and `.zip`
/// archives into `spec.out_dir`.
pub fn package(spec: &PackageSpec) -> PackageResult<PackageArtifacts> {
    if !spec.binary.is_file() {
        return Err(PackageError::InvalidInput(format!(
            "binary not found: {}",
            spec.binary.display()
        )));
    }
    if !require::is_installed("tar") {
        return Err(PackageError::MissingTool("tar".to_string()));
    }

    let pkg_name = spec.package_dir_name();
    let staging = tempfile::tempdir()?;
    let pkg_root = staging.path().join(&pkg_name);
    std::fs::create_dir_all(&pkg_root)?;

    // Binary at the package root, executable on unix.
    let bin_name = spec.target.binary_name(&spec.package_name);
    let bin_dest = pkg_root.join(&bin_name);
    std::fs::copy(&spec.binary, &bin_dest)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin_dest, std::fs::Permissions::from_mode(0o755))?;
    }

    // Caller-supplied assets.
    for (src, arcname) in &spec.assets {
        validate_arcname(arcname)?;
        let dest = pkg_root.join(arcname);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, &dest)?;
    }

    // Per-OS generated assets (install scripts, ...).
    for asset in arch::init::assets(spec.target) {
        validate_arcname(&asset.name)?;
        std::fs::write(pkg_root.join(&asset.name), asset.contents)?;
    }

    std::fs::create_dir_all(&spec.out_dir)?;
    let tar_gz_path = spec.out_dir.join(format!("{pkg_name}.tar.gz"));
    let zip_path = spec.out_dir.join(format!("{pkg_name}.zip"));

    // tar.gz via the system tar — preserves the exec bit and handles
    // both GNU tar (Linux CI) and bsdtar (macOS/Windows runners).
    let output = Command::new("tar")
        .arg("-czf")
        .arg(&tar_gz_path)
        .arg("-C")
        .arg(staging.path())
        .arg(&pkg_name)
        .output()?;
    if !output.status.success() {
        return Err(PackageError::Command {
            program: "tar".to_string(),
            args: vec![
                "-czf".into(),
                tar_gz_path.display().to_string(),
                pkg_name.clone(),
            ],
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    // zip via the in-tree writer (stored entries; deterministic).
    let mut entries = Vec::new();
    for file in walk_files(&pkg_root)? {
        let rel = file
            .strip_prefix(&pkg_root)
            .map_err(|e| PackageError::InvalidInput(e.to_string()))?;
        let arcname = format!("{pkg_name}/{}", rel.to_string_lossy().replace('\\', "/"));
        let data = std::fs::read(&file)?;
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            (std::fs::metadata(&file)?.permissions().mode() & 0o7777) | 0o100000
        };
        #[cfg(not(unix))]
        let mode = 0o100644;
        entries.push(ZipFile {
            name: arcname,
            mode,
            data,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    archive::write_zip(&zip_path, &entries).map_err(PackageError::Io)?;

    Ok(PackageArtifacts {
        package_dir_name: pkg_name,
        target: spec.target,
        tar_gz: describe(&tar_gz_path)?,
        zip: describe(&zip_path)?,
    })
}

fn validate_arcname(name: &str) -> PackageResult<()> {
    let p = Path::new(name);
    if name.is_empty()
        || p.is_absolute()
        || name.contains('\\')
        || p.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        })
    {
        return Err(PackageError::InvalidInput(format!(
            "unsafe archive path: {name:?}"
        )));
    }
    Ok(())
}

/// All regular files under `root`, recursively.
fn walk_files(root: &Path) -> PackageResult<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }
    Ok(out)
}

/// Lowercase hex SHA-256 of `data`.
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xF) as usize] as char);
    }
    out
}

fn describe(path: &Path) -> PackageResult<ArtifactFile> {
    let data = std::fs::read(path)?;
    Ok(ArtifactFile {
        sha256: sha256_hex(&data),
        size_bytes: data.len() as u64,
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_slugs_roundtrip() {
        for t in ReleaseTarget::all() {
            assert_eq!(ReleaseTarget::from_slug(t.slug()), Some(*t));
        }
        assert_eq!(ReleaseTarget::from_slug("beos-x86"), None);
    }

    #[test]
    fn package_dir_name_format() {
        let spec = PackageSpec::new("/bin/kaladoc", "0.2.0", ReleaseTarget::LinuxX86_64);
        assert_eq!(spec.package_dir_name(), "kaladoc-0.2.0-linux-x86_64");
    }

    #[test]
    fn package_produces_both_archives() {
        if !require::is_installed("tar") {
            eprintln!("skipping: tar not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("kaladoc");
        std::fs::write(&bin, b"fake binary").unwrap();

        let spec = PackageSpec::new(&bin, "9.9.9", ReleaseTarget::LinuxX86_64)
            .with_out_dir(dir.path().join("dist"))
            .with_asset(dir.path().join("LICENSE"), "LICENSE");
        std::fs::write(dir.path().join("LICENSE"), "MIT").unwrap();

        let arts = package(&spec).expect("package");
        assert!(arts.tar_gz.path.is_file());
        assert!(arts.zip.path.is_file());
        assert_eq!(arts.tar_gz.sha256.len(), 64);
        assert_eq!(arts.zip.sha256.len(), 64);
        assert!(arts.tar_gz.size_bytes > 0 && arts.zip.size_bytes > 0);

        // The tarball must list the expected members.
        let out = Command::new("tar")
            .arg("-tzf")
            .arg(&arts.tar_gz.path)
            .output()
            .unwrap();
        assert!(out.status.success());
        let listing = String::from_utf8_lossy(&out.stdout);
        let pkg = arts.package_dir_name;
        assert!(listing.contains(&format!("{pkg}/kaladoc")), "{listing}");
        assert!(listing.contains(&format!("{pkg}/LICENSE")), "{listing}");
        assert!(listing.contains("install.sh"), "{listing}");
    }
}
