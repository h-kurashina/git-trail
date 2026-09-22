# trail

Understand how your code changed, not just what changed.

trail is a local-first CLI that reconstructs the development history of a Git worktree.

## Install

```bash
cargo install trail
```

Or from source:

```bash
git clone https://github.com/h-kurashina/trail
cd trail
cargo install --path .
```

trail needs a `git` executable on `PATH`.

## Usage

```bash
trail
trail status
trail history
trail diff
trail inspect src/auth/service.ts
```

Global options:

```bash
trail --base develop   # compare against a branch other than main/master
trail --json           # machine readable output for any command
trail -C path/to/repo  # run against another directory
```

### `trail`

Shows the Development Trail of the current branch: commits since it diverged
from the base branch, followed by the changes still sitting in the working tree,
in chronological order.

```text
Development Trail
────────────────────────────────────

Repository
  my-project

Branch
  feature/auth

Base
  main

2026-09-22
12:31  Committed a82fbc1 Add authentication service (2 files)
12:43  Modified src/routes/auth.ts
12:51  Modified src/session/store.ts
13:02  Added tests/auth.test.ts

────────────────────────────────────
4 changes
5 files changed
+284 -41
```

### `trail status`

A compact view of the worktree: branch, base, how many commits you are ahead,
and counts for modified / added / deleted / renamed / untracked as well as
staged / unstaged.

### `trail history`

The detailed trail. In addition to commits and working tree changes it shows
HEAD movements from the reflog (checkout, rebase, reset, amend, merge) and tags
each working tree event with its line stats, staging state and whether its time
came from the file's mtime.

### `trail diff`

Line statistics against the base branch, grouped by directory. Untracked files
are included so the view matches what a pull request would contain.

### `trail inspect <file>`

Status, line stats (vs HEAD and vs base) and the commits that touched a single
file, following renames. Commits that are on the current branch but not on the
base are marked with `*`.

## How it works

trail never talks to the network and never sends anything anywhere. It reads:

* commits reachable from HEAD but not from the base branch (via [gix](https://github.com/GitoxideLabs/gitoxide))
* the HEAD reflog
* the index and working tree (`git status`, `git diff --numstat`)
* file modification times

Every event carries a `source` and a `confidence`. Commit and reflog times are
exact. Working tree events only have the file's mtime, which is recorded as
`inferred`; `trail --json` exposes both fields so you can tell them apart.

### Base branch detection

1. `--base <branch>` if given
2. `main`
3. `master`
4. the remote default branch recorded in `refs/remotes/origin/HEAD`

### Worktrees

trail works in the main worktree and in linked worktrees created with
`git worktree add`. The `.git` file of a linked worktree is resolved
automatically and the output notes which worktree you are in.

## Development

```bash
cargo build
cargo test
cargo run -- status
cargo run -- diff
cargo run -- history
```

Integration tests create temporary repositories and worktrees with the `git`
CLI, so `git` must be installed to run them.

## License

MIT
