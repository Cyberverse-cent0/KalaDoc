# Contributor Guide

Guidelines for humans and AI agents contributing to KalaDoc.

## Project overview

KalaDoc is a Rust (edition 2024) application for document ingest, storage,
and processing. It currently has **zero external dependencies** — prefer
`std` and justify any new crate in the PR description.

## Repository layout

```
Cargo.toml / Cargo.lock   # single-crate manifest (will become a workspace)
src/main.rs               # binary entry point
src/crates/               # reusable library modules (intended `crate::` paths)
  shared_public/          #   platform detection — no internal deps
  storage/                #   storage engine — depends on shared_public only
src/deployments/          # deployment/packaging helpers (own binaries)
.doc/                     # project documentation (this directory)
target/                   # build output (gitignored)
```

## Current state

`main.rs` is a stub and does not declare the modules under `src/crates/`
or `src/deployments/` yet — those files compile standalone but are not yet
wired into the crate. When wiring them up:

- Add `mod crates;` / `mod deployments;` (or convert to a workspace with a
  `lib.rs`) rather than editing import paths.
- `src/deployments/require.rs` contains its own `fn main()` — it must become
  a separate `[[bin]]` target or its `main` must be renamed before it can
  live inside the library crate.

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
cargo test           # runs tests (none yet — add as you go)
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
