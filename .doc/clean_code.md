# Clean Code Guidelines

How to keep KalaDoc readable and maintainable. Applies to all Rust code
in `src/`.

## Names carry meaning

- A reader should understand a function from its name and signature
  alone. If you need a comment to explain what `foo()` does, rename it.
- Avoid abbreviations except universal ones (`fs`, `os`, `io`, `cfg`).
- Boolean-returning functions read as predicates: `fs_is_available`,
  `path_on_supported_fs`, `is_installed`.

## Functions stay small and flat

- One function, one job. `create_fs_storage` probes; it does not format.
  `format_disk` formats; it does not probe.
- Prefer guard clauses and `?` over deep nesting.
- Extract helpers when a block has a single coherent purpose
  (`probe_mount`, `probe_size`, `run_tool`, `resolve`, `check_quota`).
- Platform-specific arms may make a function long — that's acceptable
  when each arm is a flat dispatch table (the `match fs.as_str()` blocks).

## Comments explain *why*, not *what*

- Doc comments (`///`) state contract: inputs, outputs, failure modes,
  destructiveness.
- Inline comments are for non-obvious reasoning only — e.g. "Pick the
  mount point that is the longest prefix of the target", "xorshift-ish
  LCG for a reproducible 'random' choice".
- Never leave commented-out code. Git has history.

## Errors are part of the design

- Every public function has a documented failure mode.
- Error variants are specific (`QuotaExceeded { path, limit }`), not
  generic (`Error(String)`).
- Implement `Display` for humans and `Error::source` for chaining.
- Provide a `From` impl only when the conversion is lossless and obvious
  (see `From<StorageError> for io::Error`).

## No hidden magic

- No global mutable state. `FaultInjectingFs` owns its `FaultInjector`;
  there are no `static mut` or thread-locals.
- No environment reads scattered through the code — config enters via
  `StorageConfig::from_env()` at the edge.
- No implicit side effects: probing never writes, `create_dir_all` in
  `FaultInjectingFs::new` is the single documented exception.

## Dead code and TODOs

- Delete unused code rather than commenting it out. `install_dependencies`
  and the `detect_windows`/`detect_macos` stubs in `require.rs` are
  placeholders pending a real deployment flow — keep them compiling and
  honest (they return `Other` rather than pretending to detect).
- A `// TODO` must name the missing capability, not just "fix later".

## Dependencies

- Zero external crates today. Adding one requires justification:
  does `std` genuinely not cover it? Prefer well-established crates
  published well in the past over brand-new releases.

## Formatting

- `cargo fmt` is the source of truth for layout. Don't hand-format.
- Match surrounding style: 4-space indent, `use` ordering as rustfmt
  produces, doc comments above items.
