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

## Staying in sync with the remote

Push rejections (`non-fast-forward`) mean the remote branch has commits
your local branch does not. This happens easily when multiple agents or
tooling sessions work on the same branch — e.g. the same work gets
committed twice under different SHAs, producing "diverged" histories
with identical commit messages but different trees.

Rules:

- **Fetch before you push.** `git fetch` then check
  `git rev-list --left-right --count HEAD...@{u}`. `0  N` means you are
  simply behind — integrate first, never force-push over someone
  else's commits.
- **Identical messages ≠ identical commits.** Two branches can carry
  the same commit subjects with different content. Compare trees
  (`git diff HEAD origin/<branch>`) before assuming one side is a
  duplicate; the remote may hold work your local lacks.
- **Prefer merge or rebase over force-push.** `git pull --rebase`
  replays local commits on top of the remote; a merge commit preserves
  both histories. Reach for `--force-with-lease` only when you are
  certain the remote commits are disposable duplicates of your own
  work — and say so explicitly before doing it.
- **Never force-push `main` or shared release branches**, and never
  plain `--force` when `--force-with-lease` works.
- **Commit or stash WIP before integrating.** Rebasing/merging with a
  dirty tree is how changes get lost. Large uncommitted work should be
  committed as a WIP commit first.
- **After a rejected push, stop and inspect.** Do not retry the same
  push in a loop — each retry fails identically. Fetch, diff, then
  choose merge / rebase / force-push deliberately.
