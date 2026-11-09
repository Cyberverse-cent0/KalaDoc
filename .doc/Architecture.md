# KalaDoc Architecture

KalaDoc is a Rust application for ingesting, storing, and processing documents.
This document describes the **intended** architecture and the modules that exist
today.

## High-level view

```
                 ┌─────────────┐
   client  ────▶ │  API layer  │   (planned)
                 └──────┬──────┘
                        │ jobs / commands
                 ┌──────▼──────┐
                 │ Job engine  │   ← exists today (`crates/job_engine`)
                 └──────┬──────┘
                        │ dispatch
        ┌───────────────┼───────────────┐
        ▼               ▼               ▼
   ┌─────────┐    ┌──────────┐    ┌─────────┐
   │ storage │    │ document │    │ deploy  │
   │         │    │ engine   │    │         │
   └─────────┘    └──────────┘    └─────────┘
        ▲                            ▲
        └── shared_public (platform detection)
```

Dependency rule: lower layers never depend on higher layers.
`shared_public` has no dependencies. `storage` depends only on
`shared_public`. `job_engine` and `document_engine` depend downward
only. The API layer remains planned.

## Current module tree

```
src/
├── main.rs                          # binary entry point
├── lib.rs                           # library root — declares all modules
├── config/
│   └── mod.rs                       # AppConfig: defaults → TOML file → env
├── core/
│   └── auth.rs                      # AuthUser row type
├── crates/
│   ├── shared_public/
│   │   └── platform.rs              # Platform detection (compile-time OS/arch/family)
│   ├── application/
│   │   ├── Organization/
│   │   │   ├── init_db.rs           # DbConfig/SessionConfig (env), pool, schema
│   │   │   ├── auth_user.rs         # User CRUD + Argon2id credential checks
│   │   │   └── session.rs           # Bearer-token sessions, expiry, cleanup task
│   │   └── cli_mcp_server/
│   │       └── cli/                 # CLI adapter (version/doctor/init/config/auth/user/jobs)
│   ├── document_engine/             # Document domain model (docs, versions, audit)
│   ├── job_engine/                  # Durable job queue + dispatcher (jobs_tbl)
│   └── storage/
│       ├── config.rs                # StorageConfig, FaultInjectionConfig, FaultKind
│       ├── debug_when_full.rs       # StorageError, FaultInjector, FaultInjectingFs
│       └── create_fs.rs             # StorageTarget probing, format_disk, FsError
└── deployments/
    └── require.rs                   # OS/distro detection and dependency checks
```

> **Status note:** all modules above are wired into the library via
> `lib.rs` and compile/test cleanly. `main.rs` currently handles
> `--init-db` and the requirements report; the CLI adapter under
> `cli_mcp_server/cli/` is a placeholder for the upcoming command
> surface.

## Module responsibilities

### `shared_public::platform`
Compile-time platform facts: `os`, `arch`, `family`, plus `is_linux()`,
`is_windows()`, `is_macos()`, `describe()`. Used by storage to pick
OS-appropriate tools and supported filesystem lists.

### `storage::config`
`StorageConfig` is the single configuration object for the storage engine:
`root_dir`, optional `max_size_bytes` quota, optional target `filesystem`,
and `fault_injection`. Built via `StorageConfig::new()` + builders
(`with_quota`, `with_filesystem`, `with_faults`) or `StorageConfig::from_env()`
(`KALADOC_STORAGE_DIR`, `KALADOC_STORAGE_MAX_BYTES`, `KALADOC_FS_TYPE`,
`KALADOC_FAULT_INJECTION`, `KALADOC_FAULT_EVERY_N`).

### `storage::debug_when_full`
`FaultInjectingFs` wraps `std::fs` with three cross-cutting concerns:

- **Path confinement** — `resolve()` normalizes `.`/`..` and rejects paths
  escaping `root_dir` (`StorageError::PathOutsideRoot`).
- **Quota enforcement** — an in-memory byte counter is seeded once by
  `dir_size(root_dir)` at open (only when `max_size_bytes` is set), then
  adjusted in O(1) on each `write`/`delete`. Every 256 mutations (or an
  explicit `reconcile()`) a full `dir_size` scan re-syncs the counter,
  bounding drift from out-of-band changes or multiple instances sharing
  the root. Writes exceeding `max_size_bytes` are rejected with
  `StorageError::QuotaExceeded`.
- **Fault injection** — `FaultInjector` fails every Nth op deterministically
  (xorshift/LCG seeded constant) with a `FaultKind`
  (`DiskFull`, `PermissionDenied`, `IoError`, `Timeout`, `CorruptedWrite`),
  restricted per operation type (`write_faults`, `read_faults`,
  `delete_faults`). Disabled by default; intended for debug builds/tests.

Public surface: `write`, `read`, `delete`, `exists`, `used_bytes`,
`reconcile`, plus `dir_size`.

### `storage::create_fs`
Device/filesystem operations, orthogonal to `FaultInjectingFs`:

- `create_fs_storage(path, config)` probes a target and returns a
  `StorageTarget` (filesystem, mount point, total/free bytes, read-only flag,
  platform). Mount info comes from `/proc/self/mounts` on Linux; size from
  `df -B1` on Unix. Undeterminable fields are `None`, never guessed.
- `fs_is_available` / `path_on_supported_fs` validate filesystem names
  against per-OS allowlists.
- `format_disk(target, filesystem, mount_point)` is **destructive** — shells
  out to `mkfs.*` (Linux), `Format-Volume` (Windows), or
  `diskutil eraseVolume` (macOS). Filesystem names are validated to reject
  path separators before reaching the shell.

### `config`
Global [`AppConfig`] resolved in layers: built-in defaults → optional
TOML file (`KALADOC_CONFIG`, or `kaladoc.toml` in the cwd) → environment
variables. Secrets (`APP_SECRET_KEY`, `APP_DB_USER`, `APP_DB_PASSWORD`) are
env-only. `load()` validates the merged result — zero session intervals,
zero pool sizes, malformed `log_level`, and path-like filesystem names are
rejected with [`ConfigError`]. Converts into [`DbConfig`] and
[`StorageConfig`] via `to_db_config()` / `to_storage_config()`.

### `core::auth`
`AuthUser` — the `users_tbl` row shape shared by the application layer.

### `application::Organization`
User accounts and sessions over SQLite (`sqlx`):

- `init_db` loads `DbConfig` via `crate::config::AppConfig` (defaults →
  optional `kaladoc.toml`/`KALADOC_CONFIG` → env), opens a tuned WAL
  pool, and creates `users_tbl`/`sessions_tbl`. Session knobs:
  `SESSION_TTL_SECS`, `SESSION_ABSOLUTE_TTL_SECS`, `SESSION_RENEW_ON_USE`,
  `SESSION_TOUCH_INTERVAL_SECS`, `SESSION_CLEANUP_INTERVAL_SECS`.
- `auth_user` handles registration, lookup, Argon2id password
  verification, and deletion.
- `session` issues opaque bearer tokens (only `SHA-256(token)` is
  stored), validates them with sliding expiry capped by an optional
  absolute TTL, throttles `last_seen_at` writes via `touch_interval`
  (validation is read-only when nothing changed), supports per-token,
  per-id, and per-user revocation plus `list_active_sessions`, deletes
  expired rows via `cleanup_expired_sessions`, and `spawn_cleanup_task`
  repeats that on the configured interval.

### `job_engine`
Durable job queue over SQLite (`jobs_tbl`, created by `init_db`).
Producers call `JobEngine::enqueue`; the `run()` poll loop claims due
jobs with an atomic `UPDATE ... RETURNING` and dispatches them to
`JobHandler`s registered by kind. Failures retry with exponential
backoff (`backoff_base * 2^attempts`, cap 1 h) up to `max_attempts`.

Crash recovery is built in: `run()` calls `recover()` once at startup,
requeueing every `running` job left by a dead process, and
`reap_stale` returns jobs orphaned mid-run (older than `stale_after`)
to `pending` on every poll tick. The CLI surface lives under
`kaladoc jobs` (`worker`, `list`, `enqueue`, `cancel`, `retry`,
`recover`); built-in handlers (`document.ingest`,
`document.metadata_extract`, `document.index`, `thumbnail.generate`,
`pdf.inspect`, `storage.gc_staging`, `system.ping`) are registered by
`application::job_handlers::register_builtin`.
Config: `[jobs]` section / `JOB_*` env vars → `AppConfig::to_job_config`.
See `.doc/job_engine.md` for usage.

### `document_engine`
Document domain model: documents, versions, artifacts, lifecycle
state machine, metadata, relationships, audit, and organization types.
Pure domain layer — persistence/execution wiring is still planned.

### `deployments::require`
Standalone deployment prerequisite checker: verifies `rust`, `cargo`, `tar`,
`zip` are on PATH (`which`/`where`), detects OS via `std::env::consts::OS`
and Linux distro via `/etc/os-release`, and prints per-distro install hints.
Ships as its own binary (has its own `main`).

## Design principles

1. **`std`-first.** No external crates yet; storage delegates to `std::fs`
   and intercepts at the operation level, so it works on any filesystem
   `std` supports.
2. **Probe, don't guess.** `StorageTarget` fields are `Option` — unknown
   values stay `None`.
3. **Errors carry context.** `StorageError`/`FsError` implement `Display` +
   `Error::source`, tagging failures with the offending path and
   distinguishing real I/O errors from injected faults.
4. **Destructive ops are explicit.** `format_disk` is a separate, clearly
   named function — never invoked implicitly by probing or config.
5. **Fault injection is opt-in.** Disabled by default; enabled via config
   or `KALADOC_FAULT_INJECTION` env vars for reproducible failure testing.
