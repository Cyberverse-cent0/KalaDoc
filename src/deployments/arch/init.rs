//! Install scripts packaged alongside the binary.
//!
//! Each target gets the script(s) appropriate to its platform so a
//! user can go from "downloaded archive" to "binary on PATH" without
//! reading docs first.

use crate::deployments::package::ReleaseTarget;

/// A generated file placed at the root of a package.
#[derive(Debug, Clone)]
pub struct ArchAsset {
    /// Archive-relative path, e.g. `install.sh`.
    pub name: String,
    pub contents: String,
}

/// Assets to include for `target`. Empty for targets with nothing
/// platform-specific to generate.
pub fn assets(target: ReleaseTarget) -> Vec<ArchAsset> {
    match target {
        ReleaseTarget::LinuxX86_64
        | ReleaseTarget::LinuxAarch64
        | ReleaseTarget::MacosX86_64
        | ReleaseTarget::MacosAarch64 => vec![ArchAsset {
            name: "install.sh".to_string(),
            contents: INSTALL_SH.to_string(),
        }],
        ReleaseTarget::WindowsX86_64 => vec![ArchAsset {
            name: "install.ps1".to_string(),
            contents: INSTALL_PS1.to_string(),
        }],
    }
}

const INSTALL_SH: &str = r#"#!/bin/sh
# KalaDoc installer — run from inside the extracted package directory:
#   ./install.sh
set -eu

BIN_NAME="kaladoc"
SRC="./$BIN_NAME"
if [ ! -f "$SRC" ]; then
    echo "error: $SRC not found — run this from the extracted package dir" >&2
    exit 1
fi

if [ -w /usr/local/bin ]; then
    DEST=/usr/local/bin
elif command -v sudo >/dev/null 2>&1; then
    DEST=/usr/local/bin
    SUDO=sudo
else
    DEST="$HOME/.local/bin"
fi

mkdir -p "$DEST"
$SUDO install -m 0755 "$SRC" "$DEST/$BIN_NAME" 2>/dev/null \
    || install -m 0755 "$SRC" "$DEST/$BIN_NAME"

case ":$PATH:" in
    *":$DEST:"*) ;;
    *) echo "note: add $DEST to your PATH to run 'kaladoc'" ;;
esac
echo "installed: $DEST/$BIN_NAME"
"#;

const INSTALL_PS1: &str = r#"# KalaDoc installer — run from inside the extracted
# package directory in PowerShell:
#   powershell -ExecutionPolicy Bypass -File .\install.ps1
$ErrorActionPreference = 'Stop'

$src = Join-Path $PSScriptRoot 'kaladoc.exe'
if (-not (Test-Path $src)) {
    Write-Error "kaladoc.exe not found — run this from the extracted package dir"
}

$dest = Join-Path $env:LOCALAPPDATA 'Programs\KalaDoc'
New-Item -ItemType Directory -Force -Path $dest | Out-Null
Copy-Item $src (Join-Path $dest 'kaladoc.exe') -Force

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($userPath -notlike "*$dest*") {
    [Environment]::SetEnvironmentVariable(
        'Path', "$userPath;$dest", 'User')
    Write-Host "added $dest to your user PATH (restart your shell)"
}
Write-Host "installed: $dest\kaladoc.exe"
"#;
