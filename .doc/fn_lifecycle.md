# Function Design Rules

Conventions for writing functions in KalaDoc. These rules are derived from
the existing codebase — follow them for consistency.

## Naming

- `snake_case` for functions and variables, `CamelCase` for types.
- Names describe *what*, not *how*: `create_fs_storage`, `probe_mount`,
  `check_quota`, `format_disk`.
- Detection/probing functions return data, not actions: `probe_size`,
  `probe_mount`, `fs_is_available`, `path_on_supported_fs`.
- Destructive operations are unmistakable: `format_disk`, `delete`.
- Detection helpers are named `detect_*`/`probe_*` — never `debug_*`
  (a past misnomer: `debug_linux` detected the distro; it is now
  `detect_linux`).

## Signatures

- Return `Result<T, E>` with the module's error type for anything that can
  fail; use the module's `*Result<T>` alias (`StorageResult<T>`).
- Take configuration by reference (`&StorageConfig`), not by value, unless
  the function consumes it.
- Paths are `&Path`/`PathBuf` at boundaries; `&str` names inside the
  storage root.
- Prefer small, single-purpose functions. `format_disk` delegates to
  `run_tool`; `create_fs_storage` delegates to `probe_mount`/`probe_size`.

## Error handling

- Never `unwrap()`/`expect()`/`panic!()` on fallible I/O in library code.
- Map errors into the module error enum at the boundary, preserving the
  source via `std::error::Error::source`.
- Distinguish error kinds callers must react to differently — e.g.
  `StorageError::NotFound` vs `StorageError::Io`, and
  `StorageError::InjectedFault` so tests can tell simulated faults from
  real failures.
- Probing functions that can legitimately fail return `Option`
  (`probe_mount`, `probe_size`); operations that must succeed return
  `Result`.

## Control flow

- Early returns for validation and error cases (`?` propagation).
- `#[cfg]` blocks isolate platform-specific implementations inside one
  function body (see `probe_mount`, `probe_size`, `format_disk`) rather
  than scattering `cfg_if` or duplicate functions.
- Keep platform fallbacks explicit — the final `#[cfg(not(any(...)))]`
  arm in `format_disk` returns `UnsupportedFilesystem` instead of
  compiling to nothing.

## State and side effects

- `FaultInjectingFs` methods take `&mut self` because the injector keeps
  an operation counter; pure queries (`check_quota`, `resolve`) take
  `&self`.
- External commands go through one helper (`run_tool`) that maps spawn
  failures and non-zero exits into the error enum uniformly.
- Functions that shell out validate inputs *before* spawning —
  `format_disk` rejects empty names and path separators, and maps
  filesystems to an explicit allowlist of tool names rather than
  interpolating user input into a command line.

## Documentation

- `///` doc comments on every `pub` item: what it does, when it fails,
  and any destructive behavior (see `format_disk`'s "Destructive — erases
  all data" warning).
- Note platform limitations honestly ("Returns `None` on non-Linux").
