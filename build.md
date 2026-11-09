# Build & Release Guide

How to build KalaDoc locally and cut a release — for developers.

## Prerequisites

- **Rust** (stable, edition 2024): `rustup` toolchain on `PATH`.
- **`tar`** — required by the packager for `.tar.gz` archives. Present on
  Linux, macOS (bsdtar), and Windows 10+ runners.
- **`gh` CLI** — only needed for `publish` (GitHub Releases upload).
  Authenticate with `gh auth login`.
- No other tools: ZIP archives are written by an in-tree dependency-free
  writer, and SHA-256 uses the existing `sha2` crate. There are no
  `zip`/`tar`/`flate2` crate dependencies.

Verify the toolchain is ready:

```bash
cargo run --release --bin kaladoc-release -- check
# or after installing:
kaladoc-release check
```

## Release targets

| Slug             | Rust triple                    | CI runner          |
|------------------|--------------------------------|--------------------|
| `linux-x86_64`   | `x86_64-unknown-linux-gnu`     | `ubuntu-latest`    |
| `linux-aarch64`  | `aarch64-unknown-linux-gnu`    | `ubuntu-24.04-arm` |
| `macos-x86_64`   | `x86_64-apple-darwin`          | `macos-13`         |
| `macos-aarch64`  | `aarch64-apple-darwin`         | `macos-latest`     |
| `windows-x86_64` | `x86_64-pc-windows-msvc`       | `windows-latest`   |

Every target produces **both** `kaladoc-<version>-<slug>.tar.gz` and
`kaladoc-<version>-<slug>.zip`, each containing:

```
kaladoc-<version>-<slug>/
├── kaladoc[.exe]     # the binary (mode 0755 on unix)
├── install.sh        # unix-likes only
└── install.ps1       # windows only
```

## The `kaladoc-release` binary

A second `[[bin]]` target (`src/bin/kaladoc-release.rs`) drives the
library modules in `src/deployments/`.

```text
kaladoc-release check                     # verify tar/gh/etc on PATH
kaladoc-release targets                   # list target slugs + triples
kaladoc-release package --target SLUG --binary PATH [--version V] [--out DIR] [--name NAME]
kaladoc-release build   --target SLUG[,SLUG..] [--version V] [--out DIR] [--name NAME]
kaladoc-release aggregate --dir DIR [--version V] [--name NAME]
kaladoc-release publish --tag TAG --dir DIR
```

### `package`

Packages an already-built binary — no cargo invocation. Use this in CI
per-target jobs after `cargo build --release --target <triple>`.

```bash
cargo build --release --target x86_64-unknown-linux-gnu --bin kaladoc
cargo run --release --bin kaladoc-release -- package \
  --target linux-x86_64 \
  --binary target/x86_64-unknown-linux-gnu/release/kaladoc \
  --version 0.2.0 --out dist
```

### `build`

Runs `cargo build --release --target <triple>` then packages. Only works
for targets whose toolchain is installed locally — cross-compiling
macOS/Windows from Linux is not possible; CI builds each target natively.

```bash
kaladoc-release build --target linux-x86_64 --version 0.2.0
```

### `aggregate`

Scans a directory of `*.tar.gz`/`*.zip` archives and writes a combined
`SHA256SUMS` (standard `sha256sum -c` format) plus `manifest.json`
(name, version, per-file sha256 + size). Run this after all per-target
artifacts are collected in one directory.

### `publish`

Creates the GitHub release for `--tag` if missing (`gh release create
--generate-notes`; in Actions, `GITHUB_SHA` pins a dispatch-created tag
to the triggering commit), then uploads every archive plus `SHA256SUMS`
and `manifest.json` with `--clobber`.

```bash
kaladoc-release aggregate --dir dist --version 0.2.0
kaladoc-release publish --tag v0.2.0 --dir dist
```

## Environment configuration

`ReleaseConfig::from_env()` (used by `build`/`aggregate`/`publish`
defaults) reads `KALADOC_RELEASE_*` variables; explicit CLI flags win:

| Variable                        | Effect                                            |
|---------------------------------|---------------------------------------------------|
| `KALADOC_RELEASE_VERSION`       | version, default `CARGO_PKG_VERSION` (no `v`)     |
| `KALADOC_RELEASE_TARGETS`       | comma-separated slugs                             |
| `KALADOC_RELEASE_OUT_DIR`       | output dir, default `dist/`                       |
| `KALADOC_RELEASE_PACKAGE_NAME`  | dist name, default `kaladoc`                      |
| `KALADOC_RELEASE_TAG`           | git tag for publish                               |

## Cutting a release (CI — normal path)

The pipeline lives in `.github/workflows/release.yml`.

1. Merge to the base branch and make sure `cargo check --all-targets`
   and `cargo test` are green.
2. Tag and push:

   ```bash
   git tag v0.2.0
   git push origin v0.2.0
   ```

3. The `build` matrix compiles `kaladoc` natively on 5 runners, packages
   both archive formats per target, and uploads `dist-<slug>` artifacts.
4. The `release` job downloads all artifacts into one directory, runs
   `kaladoc-release aggregate`, then `kaladoc-release publish` against
   the tag. Release notes are auto-generated (`--generate-notes`).

### Manual dispatch

Actions → release → *Run workflow*. Optionally supply `version`
(defaults to `Cargo.toml`'s version); the tag `v<version>` is created
on the dispatched commit.

> Note: `ubuntu-24.04-arm` runners are free for **public** repos. On a
> private repo they require a paid plan — remove that matrix leg or
> replace it with a cross-compile job if needed.

## Local end-to-end (no GitHub)

```bash
cargo build --release --bin kaladoc
cargo run --release --bin kaladoc-release -- build \
  --target "$(rustc -vV | sed -n 's/host: //p' | sed 's/-unknown.*//;s/x86_64/linux-x86_64/;s/aarch64/linux-aarch64/')" \
  --version 0.2.0 --out dist
cargo run --release --bin kaladoc-release -- aggregate --dir dist --version 0.2.0
```

`dist/` then contains both archives, `SHA256SUMS`, and `manifest.json`.
Verify with `sha256sum -c SHA256SUMS` inside `dist/`.

## Verifying an archive

```bash
tar -tzvf kaladoc-<ver>-<slug>.tar.gz     # list; kaladoc should be -rwxr-xr-x
unzip -l kaladoc-<ver>-<slug>.zip         # list
python3 -c "import zipfile; zipfile.ZipFile('x.zip').testzip()"  # CRC check
```

## Install scripts shipped in packages

- **`install.sh`** (Linux/macOS): copies `kaladoc` to `/usr/local/bin`
  (via `sudo` when needed) or `~/.local/bin`, `chmod 0755`, prints a PATH
  hint. Run from inside the extracted package dir.
- **`install.ps1`** (Windows): copies `kaladoc.exe` to
  `%LOCALAPPDATA%\Programs\KalaDoc` and appends it to the user `Path`.

## Module map (`src/deployments/`)

| Module      | Responsibility                                            |
|-------------|-----------------------------------------------------------|
| `require`   | OS/distro detection, `is_installed`, `install_hint`       |
| `package`   | `ReleaseTarget`, `PackageSpec`, staging, archive emit     |
| `archive`   | dependency-free ZIP writer (stored entries + CRC-32)      |
| `release`   | `ReleaseConfig` env, `build_release`, `aggregate`, `publish` |
| `arch`      | per-OS generated assets (install scripts)                 |

`.tar.gz` is produced by the system `tar`; `.zip` by `archive.rs`
(stored/uncompressed entries — deterministic, no new crates).

## Troubleshooting

| Symptom                                  | Fix                                                        |
|------------------------------------------|------------------------------------------------------------|
| `required tool not found: tar`           | install tar (`apt install tar`, bsdtar, or Windows builtin)|
| `unknown target slug`                    | run `kaladoc-release targets` for valid slugs              |
| `cargo build --target ... failed`        | `rustup target add <triple>`; macOS/Windows can't cross-build from Linux — use CI |
| `gh release create` fails                | `gh auth login`; check `contents: write` permission        |
| Empty `dist/` in CI                      | check the `Package archives` step log for the target slug  |
| Zip won't open                           | shouldn't happen — `archive.rs` is unit-tested; report a bug |
