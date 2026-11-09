/// Describes the platform the storage engine is running on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Platform {
    /// `std::env::consts::OS` — "linux", "windows", "macos", ...
    pub os: &'static str,
    /// `std::env::consts::ARCH` — "x86_64", "aarch64", ...
    pub arch: &'static str,
    /// `std::env::consts::FAMILY` — "unix" or "windows".
    pub family: &'static str,
}

impl Platform {
    /// Returns true on Linux and Android.
    pub fn is_linux(&self) -> bool {
        matches!(self.os, "linux" | "android")
    }

    pub fn is_windows(&self) -> bool {
        self.os == "windows"
    }

    pub fn is_macos(&self) -> bool {
        self.os == "macos"
    }

    /// Human-readable one-liner, e.g. `linux/x86_64 (unix)`.
    pub fn describe(&self) -> String {
        format!("{}/{} ({})", self.os, self.arch, self.family)
    }
}

/// Returns the current platform. Compile-time constants, so this is
/// free and always accurate for the build target.
pub fn platform() -> Platform {
    Platform {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        family: std::env::consts::FAMILY,
    }
}
