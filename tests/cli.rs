//! End-to-end tests. Each test builds a throwaway repository with the git CLI
//! and runs the compiled `trail` binary against it.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git must be installed");
    assert!(status.success(), "git {args:?} failed");
}

fn write(dir: &Path, rel: &str, content: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

struct Output {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn trail(dir: &Path, args: &[&str]) -> Output {
    let out = Command::new(env!("CARGO_BIN_EXE_trail"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .output()
        .expect("trail binary runs");
    Output {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn trail_ok(dir: &Path, args: &[&str]) -> String {
    let out = trail(dir, args);
    assert!(out.ok, "trail {args:?} failed: {}", out.stderr);
    out.stdout
}

fn trail_json(dir: &Path, args: &[&str]) -> serde_json::Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    let out = trail_ok(dir, &full);
    serde_json::from_str(&out).expect("valid json")
}

/// A repository with `main` (two commits) and `feature` (one commit) plus a
/// working tree that exercises every status kind:
///   modified (unstaged), staged modified, added (staged), deleted (staged),
///   renamed (staged), untracked.
struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn base(main_branch: &str) -> Self {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        git(&root, &["init", "-q", "-b", main_branch]);
        write(&root, "README.md", "hello\n");
        write(&root, "src/lib.rs", "fn a() {}\n");
        write(&root, "src/old.rs", "old\n");
        write(&root, "src/gone.rs", "gone\n");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "initial commit"]);
        write(&root, "src/lib.rs", "fn a() {}\nfn b() {}\n");
        git(&root, &["commit", "-qam", "add b"]);
        Fixture { _tmp: tmp, root }
    }

    fn with_feature(main_branch: &str) -> Self {
        let f = Self::base(main_branch);
        git(&f.root, &["checkout", "-qb", "feature"]);
        write(&f.root, "src/feature.rs", "pub fn feature() {}\n");
        git(&f.root, &["add", "-A"]);
        git(&f.root, &["commit", "-qm", "add feature module"]);
        f
    }

    fn dirty(self) -> Self {
        let r = &self.root;
        write(r, "src/lib.rs", "fn a() {}\nfn b() {}\nfn c() {}\n"); // unstaged modify
        write(r, "README.md", "hello\nworld\n"); // staged modify
        git(r, &["add", "README.md"]);
        write(r, "src/new.rs", "new\n"); // staged add
        git(r, &["add", "src/new.rs"]);
        git(r, &["rm", "-q", "src/gone.rs"]); // staged delete
        git(r, &["mv", "src/old.rs", "src/renamed.rs"]); // staged rename
        write(r, "notes.txt", "untracked\n"); // untracked
        self
    }
}

#[test]
fn works_in_a_normal_repository() {
    let f = Fixture::with_feature("main").dirty();
    let out = trail_ok(&f.root, &[]);
    assert!(out.contains("Development Trail"));
    assert!(out.contains("Branch\n  feature"));
    assert!(out.contains("Base\n  main"));
    assert!(out.contains("add feature module"));
    assert!(out.contains("Modified src/lib.rs"));
    assert!(out.contains("Added src/new.rs"));
    assert!(out.contains("Added notes.txt"));
    assert!(out.contains("Deleted src/gone.rs"));
    assert!(out.contains("Renamed src/old.rs -> src/renamed.rs"));
    assert!(out.contains("files changed"));
}

#[test]
fn status_distinguishes_every_kind() {
    let f = Fixture::with_feature("main").dirty();
    let out = trail_ok(&f.root, &["status"]);
    assert!(out.contains("Branch: feature"));
    assert!(out.contains("Base: main"));
    assert!(out.contains("Modified: 2"));
    assert!(out.contains("Added: 1"));
    assert!(out.contains("Deleted: 1"));
    assert!(out.contains("Renamed: 1"));
    assert!(out.contains("Untracked: 1"));
    assert!(out.contains("Staged: 4"));
    assert!(out.contains("Unstaged: 1"));

    let json = trail_json(&f.root, &["status"]);
    let entries = json["entries"].as_array().unwrap();
    let find = |p: &str| entries.iter().find(|e| e["path"] == p).unwrap().clone();
    assert_eq!(find("src/lib.rs")["unstaged"], "modified");
    assert!(find("src/lib.rs")["staged"].is_null());
    assert_eq!(find("README.md")["staged"], "modified");
    assert!(find("README.md")["unstaged"].is_null());
    assert_eq!(find("src/new.rs")["staged"], "added");
    assert_eq!(find("src/gone.rs")["staged"], "deleted");
    assert_eq!(find("src/renamed.rs")["staged"], "renamed");
    assert_eq!(find("src/renamed.rs")["orig_path"], "src/old.rs");
    assert_eq!(find("notes.txt")["untracked"], true);
    assert_eq!(json["counts"]["staged"], 4);
    assert_eq!(json["counts"]["unstaged"], 1);
    assert_eq!(json["stats"]["additions"], 4); // lib +1, README +1, new +1, notes +1
    assert_eq!(json["stats"]["deletions"], 1); // gone -1
}

#[test]
fn modified_added_deleted_renamed_appear_as_events() {
    let f = Fixture::with_feature("main").dirty();
    let json = trail_json(&f.root, &[]);
    let events = json["events"].as_array().unwrap();
    let of = |ty: &str, path: &str| {
        events
            .iter()
            .find(|e| e["type"] == ty && e["files"][0] == path)
            .unwrap_or_else(|| panic!("missing {ty} {path}"))
            .clone()
    };
    assert_eq!(of("file_modified", "src/lib.rs")["staged"], false);
    assert_eq!(of("file_modified", "README.md")["staged"], true);
    assert_eq!(of("file_added", "src/new.rs")["confidence"], "inferred");
    assert!(of("file_deleted", "src/gone.rs")["timestamp"].is_null());
    assert_eq!(of("file_renamed", "src/renamed.rs")["from"], "src/old.rs");
    assert!(of("file_added", "notes.txt")["staged"].is_null());
    let commit = events.iter().find(|e| e["type"] == "commit").unwrap();
    assert_eq!(commit["confidence"], "exact");
    assert_eq!(commit["source"], "git_commit");
    assert_eq!(commit["summary"], "add feature module");
    assert_eq!(commit["files"][0], "src/feature.rs");
    assert_eq!(json["summary"]["changes"], 7);
}

#[test]
fn works_in_a_linked_worktree() {
    let f = Fixture::with_feature("main");
    let wt_dir = TempDir::new().unwrap();
    let wt = wt_dir.path().join("linked-wt");
    git(
        &f.root,
        &[
            "worktree",
            "add",
            "-q",
            wt.to_str().unwrap(),
            "-b",
            "wt-branch",
        ],
    );
    let wt = wt.canonicalize().unwrap();
    assert!(
        wt.join(".git").is_file(),
        ".git must be a file in a linked worktree"
    );

    write(&wt, "src/lib.rs", "changed in worktree\n");
    write(&wt, "wt-only.txt", "x\n");
    git(&wt, &["add", "wt-only.txt"]);

    let json = trail_json(&wt, &["status"]);
    assert_eq!(json["repository"]["worktree"]["kind"], "linked");
    assert_eq!(json["repository"]["head"]["name"], "wt-branch");
    assert_eq!(json["repository"]["base"], "main");
    assert_eq!(json["counts"]["modified"], 1);
    assert_eq!(json["counts"]["added"], 1);
    assert_eq!(json["repository"]["worktree"]["root"], wt.to_str().unwrap());

    // The main worktree is untouched by the linked one.
    let main_json = trail_json(&f.root, &["status"]);
    assert_eq!(main_json["repository"]["worktree"]["kind"], "main");
    assert_eq!(main_json["counts"]["modified"], 0);
    assert_eq!(main_json["counts"]["added"], 0);

    let out = trail_ok(&wt, &[]);
    assert!(out.contains("linked worktree"));
    assert!(out.contains("Modified src/lib.rs"));
    assert!(out.contains("Added wt-only.txt"));
}

#[test]
fn diff_groups_by_directory_and_counts_lines() {
    let f = Fixture::with_feature("main").dirty();
    let out = trail_ok(&f.root, &["diff"]);
    assert!(out.contains("src\n────"));
    assert!(out.contains(".\n────"));
    assert!(out.contains("src/feature.rs\n  +1"));
    assert!(out.contains("notes.txt\n  +1"));
    assert!(out.contains("src/old.rs -> src/renamed.rs"));
    let json = trail_json(&f.root, &["diff"]);
    assert_eq!(json["files_changed"], 7);
    assert_eq!(json["stats"]["additions"], 5);
    assert_eq!(json["stats"]["deletions"], 1);
}

#[test]
fn inspect_reports_status_diff_and_commits() {
    let f = Fixture::with_feature("main").dirty();
    let out = trail_ok(&f.root, &["inspect", "src/lib.rs"]);
    assert!(out.contains("src/lib.rs"));
    assert!(out.contains("Modified (unstaged)"));
    assert!(out.contains("+1  vs HEAD"));
    assert!(out.contains("+1  vs main"));
    assert!(out.contains("add b"));
    assert!(out.contains("initial commit"));

    let out = trail_ok(&f.root, &["inspect", "src/feature.rs"]);
    assert!(out.contains("* ") && out.contains("add feature module"));
    assert!(out.contains("no uncommitted changes"));

    let out = trail_ok(&f.root, &["inspect", "notes.txt"]);
    assert!(out.contains("Untracked"));

    // Relative path from a subdirectory.
    let out = trail_ok(&f.root.join("src"), &["inspect", "lib.rs"]);
    assert!(out.contains("Modified (unstaged)"));

    let out = trail(&f.root, &["inspect", "does/not/exist.rs"]);
    assert!(!out.ok);
    assert!(out.stderr.contains("not tracked"));
}

#[test]
fn history_includes_reflog_and_tags() {
    let f = Fixture::with_feature("main").dirty();
    let out = trail_ok(&f.root, &["history"]);
    assert!(out.contains("Development Trail (detailed)"));
    assert!(out.contains("HEAD checkout: moving from main to feature"));
    assert!(out.contains("[+1, unstaged, time from mtime]"));
    assert!(out.contains("[+1, untracked, time from mtime]"));
    assert!(out.contains("         src/feature.rs"));
    let json = trail_json(&f.root, &["history"]);
    let reflog = json["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "ref_update")
        .expect("reflog event");
    assert_eq!(reflog["source"], "git_reflog");
    assert_eq!(reflog["action"], "checkout");

    let limited = trail_json(&f.root, &["history", "--limit", "2"]);
    assert_eq!(limited["events"].as_array().unwrap().len(), 2);
}

#[test]
fn explicit_base_branch_is_respected() {
    let f = Fixture::with_feature("main");
    git(&f.root, &["branch", "develop", "main"]);
    git(&f.root, &["checkout", "-q", "develop"]);
    write(&f.root, "dev.txt", "d\n");
    git(&f.root, &["add", "-A"]);
    git(&f.root, &["commit", "-qm", "develop commit"]);
    git(&f.root, &["checkout", "-q", "feature"]);

    let json = trail_json(&f.root, &["--base", "develop", "status"]);
    assert_eq!(json["repository"]["base"], "develop");
    assert_eq!(json["commits_ahead"], 1);
    let out = trail_ok(&f.root, &["--base", "develop"]);
    assert!(out.contains("Base\n  develop"));

    let out = trail(&f.root, &["--base", "nope"]);
    assert!(!out.ok);
    assert!(out.stderr.contains("base branch 'nope' was not found"));
}

#[test]
fn falls_back_to_master_and_origin_head() {
    let f = Fixture::with_feature("master");
    let json = trail_json(&f.root, &["status"]);
    assert_eq!(json["repository"]["base"], "master");

    // No main/master: use refs/remotes/origin/HEAD.
    let g = Fixture::with_feature("trunk");
    git(
        &g.root,
        &["update-ref", "refs/remotes/origin/trunk", "trunk"],
    );
    git(
        &g.root,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/trunk",
        ],
    );
    let json = trail_json(&g.root, &["status"]);
    assert_eq!(json["repository"]["base"], "origin/trunk");

    // Nothing to detect at all.
    let h = Fixture::with_feature("trunk");
    let out = trail(&h.root, &["status"]);
    assert!(!out.ok);
    assert!(out.stderr.contains("could not detect a base branch"));
}

#[test]
fn errors_outside_a_repository() {
    let tmp = TempDir::new().unwrap();
    let out = trail(tmp.path(), &[]);
    assert!(!out.ok);
    assert!(out.stderr.contains("not inside a Git repository"));
    assert!(!out.stderr.contains("panicked"));
}

#[test]
fn errors_are_human_readable_for_edge_cases() {
    // Empty repository
    let tmp = TempDir::new().unwrap();
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    let out = trail(tmp.path(), &["status"]);
    assert!(!out.ok);
    assert!(out.stderr.contains("no commits yet"));

    // Bare repository
    let bare = TempDir::new().unwrap();
    git(bare.path(), &["init", "-q", "--bare"]);
    let out = trail(bare.path(), &["status"]);
    assert!(!out.ok);
    assert!(out.stderr.contains("bare repository"));

    // Detached HEAD is reported, not fatal.
    let f = Fixture::with_feature("main");
    git(&f.root, &["checkout", "-q", "--detach"]);
    let out = trail_ok(&f.root, &["status"]);
    assert!(out.contains("detached HEAD at"));

    // Base branch that does not share history.
    let g = Fixture::with_feature("main");
    git(&g.root, &["checkout", "-q", "--orphan", "island"]);
    git(&g.root, &["commit", "-qm", "orphan"]);
    let out = trail(&g.root, &["status"]);
    assert!(!out.ok);
    assert!(out.stderr.contains("no common ancestor"));
}

#[test]
fn shallow_clone_is_handled() {
    // main: 5 commits, feature: 2 commits on top of it.
    let f = Fixture::base("main");
    for i in 0..3 {
        write(&f.root, &format!("main{i}.txt"), "m\n");
        git(&f.root, &["add", "-A"]);
        git(&f.root, &["commit", "-qm", "main commit"]);
    }
    git(&f.root, &["checkout", "-qb", "feature"]);
    for i in 0..2 {
        write(&f.root, &format!("feature{i}.txt"), "f\n");
        git(&f.root, &["add", "-A"]);
        git(&f.root, &["commit", "-qm", "feature commit"]);
    }
    let clone = |depth: &str, dst: &Path| {
        let ok = Command::new("git")
            .args(["clone", "-q", "--depth", depth, "--no-single-branch"])
            .arg(format!("file://{}", f.root.display()))
            .arg(dst)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let dst = dst.canonicalize().unwrap();
        git(&dst, &["checkout", "-q", "feature"]);
        dst
    };

    // Deep enough: works, but the shallow flag is surfaced.
    let deep_dir = TempDir::new().unwrap();
    let deep = clone("2", &deep_dir.path().join("shallow-deep"));
    let json = trail_json(&deep, &["status"]);
    assert_eq!(json["repository"]["shallow"], true);
    assert_eq!(json["commits_ahead"], 2);
    assert!(trail_ok(&deep, &[]).contains("shallow clone"));

    // Depth 1 cuts the history below the branch point: no merge base.
    let cut_dir = TempDir::new().unwrap();
    let cut = clone("1", &cut_dir.path().join("shallow-cut"));
    let out = trail(&cut, &["status"]);
    assert!(!out.ok);
    assert!(out.stderr.contains("shallow clone"), "{}", out.stderr);
}

#[test]
fn json_output_is_stable_for_piping() {
    let f = Fixture::with_feature("main").dirty();
    let json = trail_json(&f.root, &[]);
    assert!(json["repository"]["name"].is_string());
    assert!(json["events"].is_array());
    assert!(json["summary"]["files_changed"].is_number());
}

#[test]
fn start_records_content_changes_as_jsonl() {
    let f = Fixture::with_feature("main");
    write(&f.root, ".gitignore", "target/\n");
    git(&f.root, &["add", ".gitignore"]);
    git(&f.root, &["commit", "-qm", "ignore target"]);

    let child = Command::new(env!("CARGO_BIN_EXE_trail"))
        .args(["start", "--stop-after", "6", "--quiet"])
        .current_dir(&f.root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // Give the watcher time to attach before making changes.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    write(&f.root, "src/lib.rs", "fn a() {}\nfn b() {}\n// recorded\n"); // modified
    write(&f.root, "notes.txt", "new\n"); // added
    write(&f.root, "target/out.txt", "ignored\n"); // gitignored
    std::fs::File::options()
        .append(true)
        .open(f.root.join("README.md"))
        .unwrap(); // mtime only, content unchanged
    std::thread::sleep(std::time::Duration::from_millis(1500));
    std::fs::remove_file(f.root.join("notes.txt")).unwrap(); // deleted

    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let dir = f.root.join(".git/trail/worktrees/main");
    let log = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .expect("session file");
    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    assert_eq!(lines[0]["kind"], "session");
    assert_eq!(lines[0]["worktree_id"], "main");
    assert_eq!(lines[0]["branch"], "feature");
    assert_eq!(lines.last().unwrap()["kind"], "end");

    let events: Vec<&serde_json::Value> = lines.iter().filter(|l| l["kind"] == "event").collect();
    let find = |ty: &str, path: &str| {
        events
            .iter()
            .find(|e| e["type"] == ty && e["path"] == path)
            .unwrap_or_else(|| panic!("missing {ty} {path} in {events:?}"))
    };
    let modified = find("modified", "src/lib.rs");
    assert!(modified["before_hash"].is_string());
    assert!(modified["after_hash"].is_string());
    assert_ne!(modified["before_hash"], modified["after_hash"]);
    let added = find("added", "notes.txt");
    assert!(added["before_hash"].is_null());
    let deleted = find("deleted", "notes.txt");
    assert_eq!(deleted["before_hash"], added["after_hash"]);
    assert!(deleted["after_hash"].is_null());
    assert!(!events
        .iter()
        .any(|e| e["path"].as_str().unwrap().starts_with("target/")));
    assert!(!events.iter().any(|e| e["path"] == "README.md"));
    assert!(!events
        .iter()
        .any(|e| e["path"].as_str().unwrap().starts_with(".git")));
}

#[test]
fn start_in_linked_worktree_stores_under_common_dir() {
    let f = Fixture::with_feature("main");
    let wt_dir = TempDir::new().unwrap();
    let wt = wt_dir.path().join("wt");
    git(
        &f.root,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "rec"],
    );
    let wt = wt.canonicalize().unwrap();

    let out = trail(&wt, &["start", "--stop-after", "1", "--quiet"]);
    assert!(out.ok, "{}", out.stderr);
    let dir = f.root.join(".git/trail/worktrees/wt");
    let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
    assert_eq!(entries.len(), 1, "one session file in the common dir");
    let text = std::fs::read_to_string(entries[0].as_ref().unwrap().path()).unwrap();
    let header: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(header["worktree_id"], "wt");
    assert_eq!(header["branch"], "rec");
    assert_eq!(header["worktree_path"], wt.to_str().unwrap());
    assert!(text.lines().last().unwrap().contains("\"end\""));
}
