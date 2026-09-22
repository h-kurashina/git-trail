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
trail changes
trail history
trail diff
trail inspect src/auth/service.ts
trail start
trail sessions
trail edit
trail open src/auth/service.ts
trail review --since push
```

Global options:

```bash
trail --base develop   # compare against a branch other than main/master
trail --since push     # start the trail at the last push (or upstream, base, a revision)
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

### `trail changes`

What changed since the last push: commits, files (committed, staged,
unstaged and untracked alike) and how many checkpoints were recorded in that
window. The baseline is chosen in this order and always shown: last push
(from the remote-tracking ref's reflog), upstream tip, base branch.

```text
Changes since last push to origin/feature/auth (bdc9c5d)
my-project on feature/auth
────────────────────

Commits
  4da208c Wire session validation

Files
  ~ src/auth/service.ts
      +2 -1  unstaged
  ~ src/routes/auth.ts
      +1  committed
  + tests/auth.test.ts
      +84  untracked

────────────────────
1 commit, 3 checkpoints
3 files changed, +87 -1  (staged 0, unstaged 1, untracked 1)
```

`--since` accepts `push`, `upstream`, `base`, `auto` or any revision and
works with `trail`, `trail history` and `trail edit` as well. It is a view
filter: nothing recorded is removed, the commands just start later. After a
rebase or amend the pushed commit is no longer an ancestor of HEAD; the trail
then starts at their common ancestor and the header says so.

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
output.

When HEAD moves while recording (a commit, for example), the recorder confirms
the new object id with gix and writes a `commit` record, so checkpoints never
straddle a commit.

### Checkpoints

Raw events are never shown directly. When a trail is built they are folded
into checkpoints:

* edits to the same file closer than 500 ms are one edit
* a gap of more than 30 seconds between events starts a new checkpoint
* a commit always closes the open checkpoint
* checkpoints touching more than 20 files are flagged `bulk` and collapsed

Checkpoints appear in `trail` and `trail history` as `Session` events with
`source: trail_recorder` and `confidence: exact`. Where a checkpoint covers a
file, the mtime-based guess for that file is dropped, so exact observations
replace inferred ones instead of being shown twice.

```text
14:33  Session 20260922-053305-c02.1  2 modified
         ~ src/auth/service.ts
         ~ src/routes/auth.ts
14:33  Committed 6e3cba8 Auth route work (2 files)
14:33  Session 20260922-053305-c02.2  1 created, 1 modified
         ~ src/auth/service.ts
         + tests/more.test.ts
```

### `trail sessions`

Lists every recorded session of the repository, including sessions whose
worktree has since been removed.

```text
Development Sessions

20260922-053305-c02
  feature/auth
  2026-09-22 14:33 - 14:41
  2 checkpoints

20260922-060102-1f4
  agent/payments  (worktree removed)
  2026-09-22 15:01 - 15:58
  7 checkpoints
```

### `trail edit`

Opens the Development Trail of the current branch in `$VISUAL` (then
`$EDITOR`) as plain text, the way oil.nvim turns a directory into a buffer.
Your editor runs exactly as configured; trail adds nothing to it. The
variable is parsed with shell word rules, so `nvim --cmd "set signcolumn=no"`
works.

```text
# trail://feature/auth
# Editing this file changes Trail metadata.
# File contents and Git history are not modified.

# commit a82fbc1  Add authentication foundation

[checkpoint:20260922-053305-c02.1]
title = Session storage
hidden = false

src/session/store.ts
src/auth/service.ts

[checkpoint:20260922-053305-c02.2]
title = Tests
hidden = false
note = written after the API was wired

tests/auth.test.ts
```

Save and quit, and trail parses the buffer back:

* `title =` and `note =` label a checkpoint
* `hidden = true` removes it from the text views (it stays in `--json`)
* move a file line into another checkpoint to regroup it
* reorder the checkpoint blocks to change their reading order

Files cannot be removed or invented and commits cannot be edited; every such
attempt, and any syntax error, is rejected before anything is written. Edits
go to `<common git dir>/trail/metadata/checkpoints.json`, written atomically.
The raw session logs are never modified, so deleting the metadata file
restores the recorder's view. `trail edit --print` shows the buffer,
`trail edit --from <file>` applies one without an editor.

### `trail open`

```bash
trail open src/auth/service.ts
trail open src/auth/service.ts --at 01K5V9W4YV7A3ZK6QH0M8R2B4C.1
```

Opens a file of the worktree in `$VISUAL` / `$EDITOR`. With `--at`, the file
is restored as it was when that checkpoint ended (its latest recorded version
up to that point) into a temporary file and opened there; `--print` writes it
to stdout instead. Checkpoint ids are shown by `trail history`.

### `trail review`

Reads the changes checkpoint by checkpoint instead of as one final diff.

```bash
trail review --since push          # numbered checkpoints in reading order
trail review 2                     # files of checkpoint 2
trail review 2 src/auth/service.ts # what checkpoint 2 changed in that file
trail review 2 src/auth/service.ts --open   # the file at the end of checkpoint 2
```

```text
Review
feature/auth
since last push to origin/feature/auth (8fa19d2)

[1] Authentication foundation
    13:04 - 13:10  01K5V9W4YV7A3ZK6QH0M8R2B4C.1
    3 files  +91 -12

    + src/auth/service.ts  +58
    + src/session/store.ts  +31
    ~ src/db/schema.ts  +2 -12

[2] API integration
    13:11 - 13:18  01K5V9W4YV7A3ZK6QH0M8R2B4C.2
    2 files  +43 -16

    ~ src/routes/auth.ts  +18 -4
    ~ src/auth/service.ts  +25 -12

────────────────────
2 checkpoints, 4 files, +134 -28
```

The per-file view is a unified diff between the file's content just before
the checkpoint and at its end, rebuilt from the snapshots. Git can only show
`baseline..HEAD`; this shows each step in between. Numbers are stable for one
`--since` window; hidden checkpoints (see `trail edit`) are skipped. Files
recorded without snapshots are listed as `snapshot unavailable`.

### Snapshots

While recording, every observed version of a file is written into the Git
object database as a blob (files over 8 MiB are recorded but not
snapshotted). So that `git gc` keeps them, the recorder points
`refs/trail/sessions/<session id>` at a small commit whose tree lists the
session's blobs; the ref is refreshed at every commit boundary, every 25 new
blobs and when the session ends. Nothing under `refs/heads` or `refs/tags`
is touched, and deleting the ref only drops the snapshots, never the log.

## How it works

trail never talks to the network and never sends anything anywhere. It reads:

* commits reachable from HEAD but not from the base branch (via [gix](https://github.com/GitoxideLabs/gitoxide))
* the HEAD reflog
* the index and working tree (`git status`, `git diff --numstat`)
* file modification times
* sessions recorded by `trail start`, with the edits made in `trail edit`

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
