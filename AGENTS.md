# Agent Guide

Read `.doc/agent.md` (contributor guide) and `.doc/development_workflow.md`
(branching, commits, remote sync) before making changes. Key rules:

## Git

- Never work on `main`; one branch per concern (`fix/`, `feat/`,
  `refactor/`, `chore/`, `docs/`). Branch before editing.
- Fetch before pushing. On `non-fast-forward`, inspect the divergence
  (`git rev-list --left-right --count HEAD...@{u}` and
  `git diff HEAD origin/<branch>`) — identical commit messages can hide
  different content. Merge or rebase by default; force-push only with
  `--force-with-lease` and only when the remote commits are confirmed
  disposable duplicates.
- Commit or stash WIP before merging/rebasing. Do not retry a rejected
  push without fetching and diffing first.
- `cargo` lives at `~/.cargo/bin/cargo` if not on `PATH`.

## Build / verify

```bash
cargo check --all-targets   # fast compile check incl. tests
cargo clippy                # lints
cargo fmt                   # format before committing
```

The project is a single crate; `src/crates/*` are modules, not workspace
members — they have no `Cargo.toml` of their own.
