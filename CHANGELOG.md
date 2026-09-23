# Changelog

## Unreleased

### Breaking: `trail review` selectors name sections, then checkpoints

The review is now organised by commit. Selectors follow that shape, and this
notation is meant to stay:

```text
1        section: a commit, or the working tree (always last)
1.1      first checkpoint that led to section 1
1.2      second checkpoint of section 1
2        next section
```

Before this change `trail review 2` meant "the second checkpoint of the
review". It now means "the second section". The second checkpoint of the
second commit is `trail review 2.2`. Checkpoint ids and commit id prefixes
are accepted as before.

`--json` output of `trail review` is schema version 2 (`version` field):
`sections`, each `commit` or `working_tree`, with their `checkpoints` and
`files`, plus a `summary`. The version 1 fields are kept: `checkpoints`
(flat, in reading order), `files_changed`, `stats`. Within a checkpoint,
`number` is now its position inside its section; `label` (`"2.2"`) is the
selector.

### Added

- `trail review --commit <rev>`: one commit's development path (its
  checkpoints) and its final diff. Works for commits outside the window.
- `trail review <n> <file>`: commit diff (parent → commit) of a file;
  `trail review <n>.<m> <file>`: checkpoint diff (before → after);
  the working tree section shows HEAD → disk.
- `trail worktrees`: worktrees git has plus removed ones with recorded
  history.
- `trail review --worktree <id|branch>`: review another worktree, including
  removed ones. Ambiguous branch names are an error.
- `trail review -i`: the left pane is a Development tree (commits and
  their checkpoints, then the working tree). New key `c` shows the commit
  diff of the selection's commit.
- Checkpoints carry `commit` and `attachment` (`recorded` / `inferred`) in
  `trail --json`; the root command summarises commits, working tree
  checkpoints and development history.

### Fixed

- `trail history` could miss the checkout that created the branch when it
  landed a second before the first commit: the reflog window now starts at
  the branch point.
