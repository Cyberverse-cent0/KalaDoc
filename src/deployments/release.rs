//! Release orchestration: build per-target binaries, package them,
//! aggregate checksums/manifests, and publish to GitHub Releases.
//!
//! Configuration comes from [`ReleaseConfig::from_env`] (`KALADOC_RELEASE_*`
//! variables) or is built directly with the `with_*` methods.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::deployments::package::{
    self, PackageArtifacts, PackageError, PackageSpec, ReleaseTarget,
};
use crate::deployments::require;

/// Top-level release configuration.
#[derive(Debug, Clone)]
pub struct ReleaseConfig {
    /// Version without a leading `v`, e.g. `0.2.0`.
    pub version: String,
    /// Targets to build/package.
    pub targets: Vec<ReleaseTarget>,
    /// Output directory for archives.
    pub out_dir: PathBuf,
    /// Distribution name (`kaladoc`).
    pub package_name: String,
    /// Git tag used when publishing, e.g. `v0.2.0`.
    pub tag: Option<String>,
}

impl ReleaseConfig {
    /// Config for `version` targeting only the host platform — the safe
    /// default for local runs (cross-compiling macOS/Windows from Linux
    /// is not possible).
    pub fn new(version: &str) -> Self {
        Self {
            version: version.to_string(),
            targets: vec![host_target()],
            out_dir: PathBuf::from("dist"),
            package_name: "kaladoc".to_string(),
            tag: None,
        }
    }

    pub fn with_targets(mut self, targets: Vec<ReleaseTarget>) -> Self {
        self.targets = targets;
        self
    }

    pub fn with_out_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.out_dir = dir.into();
        self
    }

    pub fn with_package_name(mut self, name: &str) -> Self {
        self.package_name = name.to_string();
        self
    }

    pub fn with_tag(mut self, tag: &str) -> Self {
        self.tag = Some(tag.to_string());
        self
    }

    /// Reads `KALADOC_RELEASE_*` variables; unset values fall back to
    /// the defaults from [`ReleaseConfig::new`].
    ///
    /// | Variable                  | Effect                                  |
    /// |---------------------------|-----------------------------------------|
    /// | `KALADOC_RELEASE_VERSION` | version (default: `CARGO_PKG_VERSION`)  |
    /// | `KALADOC_RELEASE_TARGETS` | comma-separated slugs, e.g. `linux-x86_64,windows-x86_64` |
    /// | `KALADOC_RELEASE_OUT_DIR` | output dir (default: `dist`)            |
    /// | `KALADOC_RELEASE_TAG`     | git tag for publish                     |
    pub fn from_env() -> Self {
        let version = std::env::var("KALADOC_RELEASE_VERSION")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
        let mut cfg = Self::new(&version);

        if let Ok(targets) = std::env::var("KALADOC_RELEASE_TARGETS") {
            let parsed: Vec<ReleaseTarget> = targets
                .split(',')
                .filter_map(|s| ReleaseTarget::from_slug(s.trim()))
                .collect();
            if !parsed.is_empty() {
                cfg.targets = parsed;
            }
        }
        if let Ok(dir) = std::env::var("KALADOC_RELEASE_OUT_DIR")
            && !dir.trim().is_empty()
        {
            cfg.out_dir = PathBuf::from(dir);
        }
        if let Ok(tag) = std::env::var("KALADOC_RELEASE_TAG")
            && !tag.trim().is_empty()
        {
            cfg.tag = Some(tag);
        }
        if let Ok(name) = std::env::var("KALADOC_RELEASE_PACKAGE_NAME")
            && !name.trim().is_empty()
        {
            cfg.package_name = name;
        }
        cfg
    }
}

/// The [`ReleaseTarget`] matching the machine this code runs on.
pub fn host_target() -> ReleaseTarget {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => ReleaseTarget::LinuxX86_64,
        ("linux", "aarch64") => ReleaseTarget::LinuxAarch64,
        ("macos", "x86_64") => ReleaseTarget::MacosX86_64,
        ("macos", "aarch64") => ReleaseTarget::MacosAarch64,
        ("windows", "x86_64") => ReleaseTarget::WindowsX86_64,
        _ => ReleaseTarget::LinuxX86_64, // best-effort fallback
    }
}

#[derive(Debug)]
pub enum ReleaseError {
    Package(PackageError),
    Io(io::Error),
    Command {
        program: String,
        args: Vec<String>,
        status: Option<i32>,
        stderr: String,
    },
    InvalidInput(String),
}

impl std::fmt::Display for ReleaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Package(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "io error: {e}"),
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

impl std::error::Error for ReleaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Package(e) => Some(e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<PackageError> for ReleaseError {
    fn from(e: PackageError) -> Self {
        Self::Package(e)
    }
}

impl From<io::Error> for ReleaseError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub type ReleaseResult<T> = Result<T, ReleaseError>;

// ============================================================
// Build + package
// ============================================================

/// `cargo build --release --target <triple>` for one target, then
/// packages the resulting binary. Fails fast on the first target that
/// cannot build (e.g. a toolchain target that isn't installed).
pub fn build_release(cfg: &ReleaseConfig) -> ReleaseResult<Vec<PackageArtifacts>> {
    let mut out = Vec::with_capacity(cfg.targets.len());
    for &target in &cfg.targets {
        out.push(build_target(cfg, target)?);
    }
    Ok(out)
}

/// Builds and packages a single target.
pub fn build_target(cfg: &ReleaseConfig, target: ReleaseTarget) -> ReleaseResult<PackageArtifacts> {
    let triple = target.triple();
    let output = Command::new("cargo")
        .args(["build", "--release", "--target", triple])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()?;
    if !output.status.success() {
        return Err(ReleaseError::Command {
            program: "cargo".to_string(),
            args: vec![
                "build".into(),
                "--release".into(),
                "--target".into(),
                triple.into(),
            ],
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    let bin_path = find_binary(cfg.package_name.as_str(), target);
    if !bin_path.is_file() {
        return Err(ReleaseError::InvalidInput(format!(
            "expected binary not found after build: {}",
            bin_path.display()
        )));
    }

    let spec = PackageSpec::new(&bin_path, &cfg.version, target)
        .with_out_dir(&cfg.out_dir)
        .with_package_name(&cfg.package_name);
    Ok(package::package(&spec)?)
}

/// Locates the built binary under `target/<triple>/release/`.
fn find_binary(package_name: &str, target: ReleaseTarget) -> PathBuf {
    let exe = target.binary_name(package_name);
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(target.triple())
        .join("release");
    let with_exe = base.join(&exe);
    if with_exe.is_file() {
        return with_exe;
    }
    base.join(package_name)
}

// ============================================================
// Aggregate checksums + manifest
// ============================================================

/// A row in `SHA256SUMS` / the release manifest.
#[derive(Debug, Clone, Serialize)]
pub struct ArtifactEntry {
    pub file: String,
    pub sha256: String,
    pub size_bytes: u64,
}

/// Scans `dir` for `*.tar.gz` and `*.zip` archives and writes a
/// combined `SHA256SUMS` and `manifest.json` next to them. Used by the
/// release job after all per-target artifacts are downloaded into one
/// directory.
pub fn aggregate(
    dir: &Path,
    package_name: &str,
    version: Option<&str>,
) -> ReleaseResult<Vec<ArtifactEntry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !(name.ends_with(".tar.gz") || name.ends_with(".zip")) {
            continue;
        }
        let data = std::fs::read(&path)?;
        entries.push(ArtifactEntry {
            file: name,
            sha256: package::sha256_hex(&data),
            size_bytes: data.len() as u64,
        });
    }
    entries.sort_by(|a, b| a.file.cmp(&b.file));
    if entries.is_empty() {
        return Err(ReleaseError::InvalidInput(format!(
            "no .tar.gz/.zip archives found in {}",
            dir.display()
        )));
    }

    let sums: String = entries
        .iter()
        .map(|e| format!("{}  {}\n", e.sha256, e.file))
        .collect();
    std::fs::write(dir.join("SHA256SUMS"), sums)?;

    let manifest = serde_json::json!({
        "name": package_name,
        "version": version,
        "artifacts": entries,
    });
    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)
            .map_err(|e| ReleaseError::InvalidInput(e.to_string()))?,
    )?;

    Ok(entries)
}

// ============================================================
// Publish to GitHub Releases
// ============================================================

/// Uploads every archive plus `SHA256SUMS`/`manifest.json` in `dir`
/// to the GitHub release for `tag`, creating the release if needed.
/// Requires the `gh` CLI (checked via [`require::is_installed`]).
pub fn publish(dir: &Path, tag: &str) -> ReleaseResult<()> {
    if !require::is_installed("gh") {
        return Err(ReleaseError::InvalidInput(
            "`gh` CLI not found on PATH; install it or upload manually".to_string(),
        ));
    }

    // Create the release only when the tag doesn't already have one.
    // `--generate-notes` keeps `gh release create` non-interactive.
    // In CI, `GITHUB_SHA` pins a dispatch-created tag to this commit.
    let view = Command::new("gh").args(["release", "view", tag]).output()?;
    if !view.status.success() {
        let mut create: Vec<String> = vec![
            "release".into(),
            "create".into(),
            tag.into(),
            "--title".into(),
            tag.into(),
            "--generate-notes".into(),
        ];
        if let Ok(sha) = std::env::var("GITHUB_SHA")
            && !sha.trim().is_empty()
        {
            create.push("--target".into());
            create.push(sha);
        }
        run_checked("gh", &create.iter().map(String::as_str).collect::<Vec<_>>())?;
    }

    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .map(|n| {
                        let n = n.to_string_lossy();
                        n.ends_with(".tar.gz")
                            || n.ends_with(".zip")
                            || n == "SHA256SUMS"
                            || n == "manifest.json"
                    })
                    .unwrap_or(false)
        })
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(ReleaseError::InvalidInput(format!(
            "no release files found in {}",
            dir.display()
        )));
    }

    let mut args: Vec<String> = vec![
        "release".into(),
        "upload".into(),
        tag.into(),
        "--clobber".into(),
    ];
    args.extend(files.iter().map(|p| p.display().to_string()));
    run_checked("gh", &args.iter().map(String::as_str).collect::<Vec<_>>())
}

fn run_checked(program: &str, args: &[&str]) -> ReleaseResult<()> {
    let output = Command::new(program).args(args).output()?;
    if !output.status.success() {
        return Err(ReleaseError::Command {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_target_is_a_known_target() {
        assert!(ReleaseTarget::all().contains(&host_target()));
    }

    #[test]
    fn aggregate_writes_sums_and_manifest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a-linux-x86_64.tar.gz"), b"tar-bytes").unwrap();
        std::fs::write(dir.path().join("b-windows-x86_64.zip"), b"zip-bytes").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), b"nope").unwrap();

        let entries = aggregate(dir.path(), "kaladoc", Some("1.2.3")).unwrap();
        assert_eq!(entries.len(), 2);

        let sums = std::fs::read_to_string(dir.path().join("SHA256SUMS")).unwrap();
        assert!(sums.contains("a-linux-x86_64.tar.gz"));
        assert!(sums.contains("b-windows-x86_64.zip"));
        assert!(!sums.contains("ignored.txt"));

        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["name"], "kaladoc");
        assert_eq!(manifest["version"], "1.2.3");
        assert_eq!(manifest["artifacts"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn aggregate_fails_on_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(aggregate(dir.path(), "kaladoc", None).is_err());
    }
}
