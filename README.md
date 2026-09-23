# trail

Understand how your code changed, not just what changed.

trail is a local-first CLI that reconstructs the development history of a Git worktree.

## Install

```bash
cargo install git-trail
```

Or from source:

```bash
git clone https://github.com/h-kurashina/git-trail
cd git-trail
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
trail review --since push -i
trail review --commit a82fbc1
trail review --worktree agent-wt
trail worktrees
```

Global options:

```bash
trail --base develop   # compare against a branch other than main/master
trail --since push     # start the trail at the last push (or upstream, base, a revision)
trail --json           # machine readable output for any command
trail -C path/to/repo  # run against another directory
```

### `trail`

Shows the Development Trail of the current worktree: commits since it
diverged from the base branch, followed by the changes still sitting in the
working tree, in chronological order.

```text
Development Trail
────────────────────────────────────

Repository
  my-project

Worktree
  feature/auth

Base
  main

Commits
  1

Working tree
  2 checkpoints

Development history
  5 checkpoints

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

Reads how the worktree came to be: commit by commit, and inside each commit
checkpoint by checkpoint. A checkpoint says how something was made; a commit
says what was confirmed. trail shows both.

```text
Worktree
  Commit / Working tree     what was confirmed (or not yet)
    Checkpoint              how it was made
      File → diff
```

```bash
trail review --since push          # sections: commits, then the working tree
trail review 2                     # one commit: development path + final diff
trail review 2.1                   # one checkpoint: its files
trail review 2 src/auth/service.ts     # commit diff of a file (parent → commit)
trail review 2.1 src/auth/service.ts   # checkpoint diff (before → after)
trail review 2.1 src/auth/service.ts --open   # the file at the end of the checkpoint
trail review 3 src/auth/service.ts     # working tree diff (HEAD → disk)
trail review --commit a82fbc1      # any commit, also outside the window
```

```text
Review
feature/auth
since last push to origin/feature/auth (8fa19d2)

────────────────────────────────────

[1] a82fbc1 Add authentication model
    Hinata  2026-09-22 13:10
    2 checkpoints  5 files  +184 -31

    [1.1] Schema and domain model  +91 -12
    [1.2] Service integration  +93 -19

[2] c19e712 Add auth API
    Hinata  2026-09-22 13:40
    0 checkpoints  4 files  +121 -44

    no recorded checkpoints

[3] Working tree
    1 checkpoint  2 files  +23 -4  (staged 0, unstaged 1, untracked 1)

    [3.1] Checkpoint 3.1  +23 -4

────────────────────────────────────
2 commits, 3 checkpoints (1 uncommitted), 11 files, +328 -79
```

Sections are numbered, checkpoints are `<section>.<n>`. A commit made while
the recorder was not running, or before any session, simply has
`no recorded checkpoints`: git facts are never turned into checkpoints.

**Which commit a checkpoint belongs to.** The recorder writes a `commit`
record whenever HEAD moves. A checkpoint is attached to

1. the commit the recorder watched being made (HEAD moved from its parent
   to it): `recorded`;
2. otherwise, when that commit was amended or rebased since, the reviewed
   commit with the same author time (and summary or parent): `inferred`;
3. otherwise the first reviewed commit created after the checkpoint ended,
   which covers commits made while the recorder was stopped: `inferred`;
4. otherwise the working tree.

`--json` carries `attachment` on every checkpoint, and inferred ones are
tagged `[inferred]` in the text. A checkout or reset moves HEAD too, but is
not a commit and never claims a checkpoint.

**Commit diff and checkpoint diff are different things.** A checkpoint diff
is `before checkpoint → after checkpoint`, rebuilt from snapshots. A commit
diff is `parent tree → commit tree`, computed from git objects; for a merge
it is the diff against the first parent, and the output says so. Files
recorded without snapshots are listed as `snapshot unavailable`.

The JSON output is version 2: a `sections` list (`commit` or
`working_tree`), each with its `checkpoints` and `files`, plus a `summary`.
The version 1 fields (`checkpoints` as a flat list, `files_changed`,
`stats`) are still present.

### `trail review -i`

The same review as an interactive browser. Wide terminals show three panes
(Development, Files, Diff); narrow ones show one level at a time and step
through them. The state machine is the same in both, only the layout
differs.

```text
┌ Development ────────────────┬ Files  [1.2] ────────┬ Diff  src/auth/service.ts ─┐
│ ▼ [1] a82fbc1 Add model     │ > ~ src/auth/service │ @@ -12,7 +12,9 @@          │
│   ├ [1.1] Schema            │   ~ src/routes/auth  │ -  return session;         │
│ > └ [1.2] Service integ.    │                      │ +  return validate(session)│
│ ▼ [2] c19e712 Add auth API  │                      │                            │
│ ▼ [3] Working tree          │                      │                            │
│   └ [3.1] Checkpoint 3.1    │                      │                            │
└─────────────────────────────┴──────────────────────┴────────────────────────────┘
```

| key | action |
|---|---|
| `j` / `k`, arrows | move (scroll in the diff) |
| `Enter` | expand a collapsed commit; otherwise into files, then into the diff |
| `d` | diff of the selection: a commit, a checkpoint, or a file |
| `c` | commit diff of the commit the selection belongs to |
| `o` | open the selected file: at the end of the checkpoint, as committed, or in the worktree |
| `h` / `Esc` | back one level; on the tree, collapse or jump to the commit |
| `q` | quit |

The browser holds no review logic of its own: it shows the same model as
the text and JSON output and asks the review module for diffs.

### `trail worktrees`

Every worktree of the repository: the ones git has, and the ones that were
removed but left recorded history under the common git dir.

```text
Worktrees
my-project

main  (current)
  path: /Users/…/my-project
  branch: main
  0 commits
  0 checkpoints

agent-wt  (removed)
  path: /Users/…/my-project-agent
  branch: agent/payments
  4 commits
  11 checkpoints
```

### `trail review --worktree <id|branch>`

Reviews another worktree, by its id (the directory name under
`.git/worktrees/`, or `main`) or its branch. A branch checked out in two
worktrees, or reused after a worktree was removed, is ambiguous and an
error; the id always works.

A removed worktree is rebuilt from what is left: its commits (the branch
ref, or the last HEAD its sessions recorded), its checkpoints, the metadata
overlay and the snapshots. The working tree section is marked as
unavailable, since nothing on disk can be read any more; checkpoint and
commit diffs still work.

Sessions remain the recorder's storage unit (`trail sessions` lists them
raw), but the review is about worktrees, commits and checkpoints.

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
