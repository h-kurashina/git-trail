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
trail start
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

### `trail start`

Records what changes in the worktree while you (or a coding agent) work, until
Ctrl+C. This is the first information trail holds that Git does not.

```text
$ trail start

Recording development trail...

Repository
  my-project

Worktree
  feature/auth

Session
  01K5V9W4YV7A3ZK6QH0M8R2B4C

Press Ctrl+C to stop.

14:19:27  modified src/auth/service.ts
14:19:28  created tests/auth.test.ts
14:19:40  renamed src/auth/util.ts -> src/auth/helpers.ts
```

Only real content changes are recorded: every notification is verified by
hashing the file and comparing it with the last known content, so editor
saves without changes and mtime-only touches are dropped. Renames are
recognised by content (a path that vanished and a path that appeared with the
same content), not by trusting the watcher. Paths matched by `.gitignore`,
`.git/info/exclude` or the global ignore file, and everything under `.git`,
are skipped.

Sessions are append-only JSONL files under
`<common git dir>/trail/worktrees/<worktree id>/sessions/`, so they survive
`git worktree remove`. Each event carries the git blob id of the content
before and after the change:

```json
{"kind":"session","version":1,"session_id":"01K5V9W4YV7A3ZK6QH0M8R2B4C","repository_root":"/…/my-project","worktree_id":"main","worktree_path":"/…/my-project","branch":"feature/auth","base_commit":"17c881b…","start_head":"2b86c0e…","started_at":"…"}
{"kind":"event","timestamp":"…","path":"src/auth/service.ts","type":"modified","before_hash":"70e5878…","after_hash":"40fcf65…"}
{"kind":"event","timestamp":"…","path":"src/auth/helpers.ts","type":"renamed","from_path":"src/auth/util.ts","before_hash":"9a1…","after_hash":"9a1…"}
{"kind":"end","ended_at":"…","events":2}
```

`--stop-after <seconds>` stops automatically, `--quiet` suppresses the live
output. Recorded sessions are not yet shown by `trail history`; that is the
next step.

## How it works

trail never talks to the network and never sends anything anywhere. It reads:

* commits reachable from HEAD but not from the base branch (via [gix](https://github.com/GitoxideLabs/gitoxide))
* the HEAD reflog
* the index and working tree (`git status`, `git diff --numstat`)
* file modification times
* sessions recorded by `trail start`

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
