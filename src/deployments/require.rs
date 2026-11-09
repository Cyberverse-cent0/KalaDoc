//! Cross-platform dependency and OS detection utilities.
//!
//! This module is designed to be called from other code. It does not
//! print to stdout or manipulate the environment; it returns structured
//! results that callers can act on.
//!
//! # Example
//! ```
//! use kaladoc::deployments::require::check_requirements;
//!
//! let report = check_requirements();
//! for dep in &report.dependencies {
//!     if !dep.installed {
//!         eprintln!("missing: {}", dep.name);
//!     }
//! }
//! ```

use std::process::{Command, Stdio};
use std::sync::OnceLock;

// ============================================================
// Public data types
// ============================================================

/// A single dependency and its detection result.
#[derive(Debug, Clone)]
pub struct Dependency {
    pub name: String,
    /// Minimum required version. Empty string means "any version".
    pub min_version: String,
    pub installed: bool,
    /// Version string parsed from `<tool> --version`, if available.
    pub detected_version: Option<String>,
}

impl Dependency {
    pub fn new(name: &str, min_version: &str) -> Self {
        Self {
            name: name.to_string(),
            min_version: min_version.to_string(),
            installed: false,
            detected_version: None,
        }
    }
}

/// Full result of a requirements check.
#[derive(Debug, Clone)]
pub struct RequirementReport {
    pub os: Os,
    pub distro: Option<Distro>,
    pub dependencies: Vec<Dependency>,
    /// Names of dependencies that were not found.
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    Macos,
    Linux,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distro {
    Ubuntu,
    Debian,
    Arch,
    Fedora,
    CentOS,
    RedHat,
    OpenSUSE,
    SLES,
    Alpine,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsVersion {
    Windows10,
    Windows11,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacosVersion {
    Macos10,
    Macos11,
    Macos12,
    Macos13,
    Macos14,
    Macos15,
    Other,
}

// ============================================================
// Public API
// ============================================================

/// Default dependency list: Rust toolchain + archive tools.
/// Callers can build their own list and call [`check_dependencies`] instead.
pub fn default_dependencies() -> Vec<Dependency> {
    vec![
        Dependency::new("rustc", "1.0.0"),
        Dependency::new("cargo", "1.0.0"),
        Dependency::new("tar", ""),
        Dependency::new("zip", ""),
    ]
}

/// Checks the default dependency list and returns a full report.
pub fn check_requirements() -> RequirementReport {
    check_dependencies(default_dependencies())
}

/// Checks a caller-supplied dependency list and returns a full report.
pub fn check_dependencies(mut dependencies: Vec<Dependency>) -> RequirementReport {
    let os = detect_os();
    let distro = (os == Os::Linux).then(detect_linux);

    let mut missing = Vec::new();

    for dep in &mut dependencies {
        dep.installed = is_installed(&dep.name);
        if dep.installed {
            dep.detected_version = detect_version(&dep.name);
        } else {
            missing.push(dep.name.clone());
        }
    }

    RequirementReport {
        os,
        distro,
        dependencies,
        missing,
    }
}

/// Returns the install command(s) the user should run to satisfy
/// the missing dependencies. Does **not** execute anything.
pub fn install_hint(report: &RequirementReport) -> Vec<String> {
    let mut hints = Vec::new();
    match report.os {
        Os::Linux => match report.distro.unwrap_or(Distro::Other) {
            Distro::Ubuntu | Distro::Debian => {
                hints.push("sudo apt update && sudo apt install -y tar zip curl".into());
                hints.push("curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh".into());
            }
            Distro::Arch => {
                hints.push("sudo pacman -S --needed tar zip rustup".into());
            }
            Distro::Fedora | Distro::CentOS | Distro::RedHat => {
                hints.push("sudo dnf install -y tar zip".into());
                hints.push("curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh".into());
            }
            Distro::OpenSUSE | Distro::SLES => {
                hints.push("sudo zypper install -y tar zip".into());
                hints.push("curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh".into());
            }
            Distro::Alpine => {
                hints.push("sudo apk add tar zip curl".into());
                hints.push("curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh".into());
            }
            Distro::Other => {
                hints.push("Unknown distro; install tar, zip, and Rust manually.".into());
            }
        },
        Os::Macos => {
            hints.push("xcode-select --install".into());
            hints.push("brew install rustup && rustup-init".into());
        }
        Os::Windows => {
            hints.push("tar and zip are built into Windows 10+ (bsdtar).".into());
            hints.push("Install Rust via https://rustup.rs/".into());
        }
        Os::Other => {
            hints.push("Unsupported OS; install tar, zip, and Rust manually.".into());
        }
    }
    hints
}

// ============================================================
// OS / Distro detection (public)
// ============================================================

pub fn detect_os() -> Os {
    match std::env::consts::OS {
        "windows" => Os::Windows,
        "macos" => Os::Macos,
        "linux" | "android" => Os::Linux,
        _ => Os::Other,
    }
}

/// Detects the Linux distribution by parsing `/etc/os-release`.
/// Returns `Distro::Other` on non-Linux targets.
pub fn detect_linux() -> Distro {
    static CACHE: OnceLock<Distro> = OnceLock::new();
    *CACHE.get_or_init(|| {
        #[cfg(target_os = "linux")]
        {
            let Ok(contents) = std::fs::read_to_string("/etc/os-release") else {
                return Distro::Other;
            };

            let mut id = "";
            let mut id_like = "";
            for line in contents.lines() {
                if let Some(v) = line.strip_prefix("ID=") {
                    id = v.trim_matches('"');
                } else if let Some(v) = line.strip_prefix("ID_LIKE=") {
                    id_like = v.trim_matches('"');
                }
            }

            match id {
                "ubuntu" => Distro::Ubuntu,
                "debian" | "linuxmint" | "pop" | "kali" => Distro::Debian,
                "arch" | "manjaro" | "endeavouros" => Distro::Arch,
                "fedora" | "rocky" | "almalinux" | "ol" => Distro::Fedora,
                "rhel" => Distro::RedHat,
                "centos" => Distro::CentOS,
                "opensuse" | "opensuse-leap" | "opensuse-tumbleweed" => Distro::OpenSUSE,
                "sles" | "sled" => Distro::SLES,
                "alpine" => Distro::Alpine,
                _ => {
                    let like = id_like.to_lowercase();
                    if like.contains("debian") || like.contains("ubuntu") {
                        Distro::Debian
                    } else if like.contains("rhel") || like.contains("fedora") {
                        Distro::Fedora
                    } else if like.contains("arch") {
                        Distro::Arch
                    } else if like.contains("suse") {
                        Distro::OpenSUSE
                    } else {
                        Distro::Other
                    }
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Distro::Other
        }
    })
}

pub fn detect_windows() -> WindowsVersion {
    #[cfg(target_os = "windows")]
    {
        let out = Command::new("cmd").args(["/C", "ver"]).output().ok();
        if let Some(out) = out {
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(start) = text.find("Version ") {
                let after = &text[start + 8..];
                let ver: String = after
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .collect();
                let parts: Vec<&str> = ver.split('.').collect();
                if parts.len() >= 3 {
                    if let Ok(build) = parts[2].parse::<u32>() {
                        return match build {
                            22000..=22699 => WindowsVersion::Windows11,
                            10240..=21999 => WindowsVersion::Windows10,
                            _ => WindowsVersion::Other,
                        };
                    }
                }
            }
        }
        WindowsVersion::Other
    }
    #[cfg(not(target_os = "windows"))]
    {
        WindowsVersion::Other
    }
}

pub fn detect_macos() -> MacosVersion {
    #[cfg(target_os = "macos")]
    {
        let out = Command::new("sw_vers")
            .args(["-productVersion"])
            .output()
            .ok();
        if let Some(out) = out {
            let text = String::from_utf8_lossy(&out.stdout);
            let major: u32 = text
                .trim()
                .split('.')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            return match major {
                10 => MacosVersion::Macos10,
                11 => MacosVersion::Macos11,
                12 => MacosVersion::Macos12,
                13 => MacosVersion::Macos13,
                14 => MacosVersion::Macos14,
                15 => MacosVersion::Macos15,
                _ => MacosVersion::Other,
            };
        }
        MacosVersion::Other
    }
    #[cfg(not(target_os = "macos"))]
    {
        MacosVersion::Other
    }
}

// ============================================================
// Tool detection (public)
// ============================================================

/// Returns `true` if `cmd` is found on `PATH` and is executable.
/// Works on Windows (honors `PATHEXT`) and Unix (checks exec bit).
pub fn is_installed(cmd: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };

    let candidates: Vec<String> = if cfg!(windows) {
        let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into());
        exts.split(';')
            .filter(|e| !e.is_empty())
            .map(|ext| format!("{}{}", cmd, ext.to_lowercase()))
            .collect()
    } else {
        vec![cmd.to_string()]
    };

    for dir in std::env::split_paths(&path_var) {
        for candidate in &candidates {
            let full = dir.join(candidate);
            if full.is_file() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(meta) = std::fs::metadata(&full) {
                        if meta.permissions().mode() & 0o111 != 0 {
                            return true;
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Runs `<cmd> --version` and extracts the first version-looking token.
pub fn detect_version(cmd: &str) -> Option<String> {
    let out = Command::new(cmd)
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let text = if stdout.trim().is_empty() {
        stderr
    } else {
        stdout
    };

    for token in text.split_whitespace() {
        let cleaned: String = token
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if cleaned.matches('.').count() >= 1 && cleaned.starts_with(|c: char| c.is_ascii_digit()) {
            return Some(cleaned);
        }
    }
    None
}

// ============================================================
// Tests (replace the old `main` demo harness)
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_os_returns_known_variant() {
        let os = detect_os();
        // On this build target it should match the compile target.
        #[cfg(target_os = "linux")]
        assert_eq!(os, Os::Linux);
        #[cfg(target_os = "windows")]
        assert_eq!(os, Os::Windows);
        #[cfg(target_os = "macos")]
        assert_eq!(os, Os::Macos);
    }

    #[test]
    fn check_requirements_returns_report() {
        let report = check_requirements();
        assert!(!report.dependencies.is_empty());
        // Missing list must correspond exactly to uninstalled deps.
        let expected_missing: Vec<String> = report
            .dependencies
            .iter()
            .filter(|d| !d.installed)
            .map(|d| d.name.clone())
            .collect();
        assert_eq!(report.missing, expected_missing);
    }

    #[test]
    fn install_hint_never_empty() {
        let report = check_requirements();
        assert!(!install_hint(&report).is_empty());
    }
}
