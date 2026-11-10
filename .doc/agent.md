# Contributor Guide

Guidelines for humans and AI agents contributing to KalaDoc.

## Project overview

KalaDoc is a Rust (edition 2024) application for document ingest, storage,
and processing. It is a **single crate**: `src/crates/*` are modules wired
through `src/lib.rs` (via `#[path]` attributes), not workspace members —
they have no `Cargo.toml` of their own. External dependencies are used
where they earn their place (sqlx, axum, lopdf, tokio, ...); justify new
crates in the PR description.

## Repository layout

```
Cargo.toml / Cargo.lock   # single-crate manifest
src/main.rs               # binary entry point (kaladoc CLI)
src/lib.rs                # library root — declares all modules
src/config/               # AppConfig: defaults → kaladoc.toml → env
src/core/                 # shared row types (AuthUser)
src/crates/               # reusable library modules
  core/                   #   ProcessingEngine contract
  shared_public/          #   platform detection — no internal deps
  storage/                #   storage backends (local FS, remote S3)
  document_engine/        #   document domain + SQL store
  job_engine/             #   durable job queue
  scheduler/              #   recurring jobs via the queue
  engine-registry/        #   ProcessingEngine registry
  security-engine/        #   upload validation / sniffing
  pdf-engine/             #   PDF ops (100+)
  image-engine/           #   image ops
  multimedia-engine/      #   ffmpeg/ffprobe media ops
  conversion-engine/      #   format conversion backends
  pipeline-engine/        #   multi-step pipelines
  api/                    #   axum HTTP API
  application/            #   services, job handlers, CLI adapter
  observability/          #   logging / health / request IDs
src/deployments/          # deployment/packaging helpers (own binaries)
.doc/                     # project documentation (this directory)
target/                   # build output (gitignored)
```

## Current state

All modules are wired into the library via `lib.rs` and compile/test
cleanly. `main.rs` dispatches the clap command tree under
`crates/application/cli_mcp_server/cli/` (`version`, `doctor`, `init`,
`config`, `auth`, `user`, `jobs`, `engine`, `doc`, `pdf`, `serve`).
`src/bin/kaladoc-release.rs` is an additional auto-discovered binary for
release packaging.

## Conventions

- **Builders for config.** `StorageConfig::new(dir).with_quota(n)...` — add
  new options as `with_*` builder methods, not positional args.
- **Errors as enums.** One error enum per module (`StorageError`, `FsError`),
  with `Display` + `Error::source` impls and a `StorageResult<T>` alias.
  Include enough context (paths, limits, tool names) to debug from logs.
- **Env-based config.** Runtime configuration is read in `from_env()` using
  the `KALADOC_*` prefix; document every variable in the module docs.
- **Platform gating.** Use `#[cfg(...)]` blocks and the `platform()` helper —
  never `std::env::consts` scattered through call sites, and never guess:
  return `Option`/`None` for values that can't be determined.
- **Destructive operations** (format, delete outside root, etc.) get their
  own explicitly named functions and validate all inputs first.
- **No comments unless needed.** Doc comments (`///`) on public items are
  expected; inline `//` comments only for non-obvious reasoning.

## Building and testing

```bash
cargo build          # builds the binary
cargo test           # runs the test suite — keep it green
cargo clippy         # lints
cargo fmt            # formatting — run before committing
```

Fault-injection presets (`FaultInjectionConfig::disk_full_every_3()`,
`KALADOC_FAULT_INJECTION=1 KALADOC_FAULT_EVERY_N=3`) exist so storage
failure paths can be exercised deterministically in tests and debug runs.

## Documentation

Project docs live in `.doc/`. When you change architecture, error types, or
public APIs, update the relevant file there (`Architecture.md`,
`fn_lifecycle.md`, `clean_code.md`).

## Code of conduct

Participation is governed by `.doc/code_of_conduct.md`.
