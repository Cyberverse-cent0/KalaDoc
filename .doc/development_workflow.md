# Development Workflow

How work moves through the repository: isolation rules, branch naming,
and commit discipline for humans and AI agents.

## Branching rules

- **Never work directly on `main` / the base branch.** Every change —
  fix, feature, refactor, docs — happens on its own branch.
- **One branch = one concern.** A branch carries a single major change
  or feature. Do not mix unrelated work (e.g. a storage fix plus a CLI
  feature) in the same branch.
- **Major workflows/movements are major changes.** Restructures, engine
  rewrites, dependency migrations, or anything that touches many crates
  get their own dedicated branch and their own commit(s). They are not
  bundled into feature branches.
- **Create a branch before starting.** Branch first, then edit. Never
  accumulate uncommitted changes and sort them out later.

## Branch naming

```
fix/<short-description>          # bug fixes
feat/<short-description>         # new features
refactor/<short-description>     # restructuring / cleanup
chore/<short-description>        # tooling, deps, housekeeping
docs/<short-description>         # documentation-only changes
```

Examples:

- `fix/storage-engine-problems`
- `feat/document-versioning`
- `refactor/split-storage-local-remote`

## Commit discipline

- **Commit only what belongs to the branch's concern.** Stage files
  deliberately (`git add <paths>`); avoid `git add -A` when the working
  tree contains unrelated changes.
- **One logical change per commit.** Prefer several small commits over
  one giant commit on a long-lived branch.
- **Commit messages describe the why.** First line: imperative summary
  under ~72 chars. Body explains motivation, trade-offs, and any
  follow-ups.
- **Authors stay as configured.** Do not change git identity per commit;
  the configured `user.name`/`user.email` is the author.
- **Check `git status` before committing.** Untracked files (build
  output, scratch files, secrets) must not be swept in. `.env`,
  `*.sqlite*`, and `target/` are already gitignored — keep it that way.

## Before committing

1. `git status` — confirm only intended files are staged.
2. `git diff --cached` — review the staged diff for secrets or noise.
3. Verify the branch is not the base branch.
4. Commit, then push only when asked.

## Merging

- Branches merge back via pull request or merge commit once the
  feature/fix is complete and verified.
- Delete the branch after merge.
