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
        .args(["start", "--stop-after", "7", "--quiet"])
        .current_dir(&f.root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // Give the watcher time to attach before making changes.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    write(&f.root, "src/lib.rs", "fn a() {}\nfn b() {}\n// recorded\n"); // modified
    write(&f.root, "notes.txt", "new\n"); // created
    write(&f.root, "target/out.txt", "ignored\n"); // gitignored
    std::fs::File::options()
        .append(true)
        .open(f.root.join("README.md"))
        .unwrap(); // mtime only, content unchanged
    std::thread::sleep(std::time::Duration::from_millis(1500));
    std::fs::remove_file(f.root.join("notes.txt")).unwrap(); // deleted
    std::fs::rename(f.root.join("src/old.rs"), f.root.join("src/moved.rs")).unwrap(); // renamed
    std::thread::sleep(std::time::Duration::from_millis(1500));
    write(&f.root, "src/lib.rs", "fn a() {}\nfn b() {}\n// recorded\n"); // same content again

    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let lines = session_lines(&f.root.join(".git/trail/worktrees/main/sessions"));
    assert_eq!(lines[0]["kind"], "session");
    assert_eq!(lines[0]["version"], 1);
    assert_eq!(lines[0]["worktree_id"], "main");
    assert_eq!(lines[0]["branch"], "feature");
    assert_eq!(lines[0]["repository_root"], f.root.to_str().unwrap());
    assert!(lines[0]["start_head"].is_string());
    assert!(lines[0]["base_commit"].is_string());
    assert_eq!(lines.last().unwrap()["kind"], "end");

    let events: Vec<&serde_json::Value> = lines.iter().filter(|l| l["kind"] == "event").collect();
    let find = |ty: &str, path: &str| {
        events
            .iter()
            .find(|e| e["kind"] == "event" && e["type"] == ty && e["path"] == path)
            .unwrap_or_else(|| panic!("missing {ty} {path} in {events:?}"))
    };
    let modified = find("modified", "src/lib.rs");
    assert!(modified["before_hash"].is_string());
    assert!(modified["after_hash"].is_string());
    assert_ne!(modified["before_hash"], modified["after_hash"]);
    let created = find("created", "notes.txt");
    assert!(created["before_hash"].is_null());
    let deleted = find("deleted", "notes.txt");
    assert_eq!(deleted["before_hash"], created["after_hash"]);
    assert!(deleted["after_hash"].is_null());
    let renamed = find("renamed", "src/moved.rs");
    assert_eq!(renamed["from_path"], "src/old.rs");
    assert_eq!(renamed["before_hash"], renamed["after_hash"]);
    // Same content written twice is one event.
    assert_eq!(
        events.iter().filter(|e| e["path"] == "src/lib.rs").count(),
        1
    );
    assert!(!events
        .iter()
        .any(|e| e["path"].as_str().unwrap().starts_with("target/")));
    assert!(!events.iter().any(|e| e["path"] == "README.md"));
    assert!(!events
        .iter()
        .any(|e| e["path"].as_str().unwrap().starts_with(".git")));
}

fn session_lines(dir: &Path) -> Vec<serde_json::Value> {
    let log = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .expect("session file");
    std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[cfg(unix)]
#[test]
fn start_closes_the_session_on_ctrl_c() {
    let f = Fixture::with_feature("main");
    let child = Command::new(env!("CARGO_BIN_EXE_trail"))
        .args(["start", "--quiet"])
        .current_dir(&f.root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    write(&f.root, "sig.txt", "x\n");
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let ok = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let lines = session_lines(&f.root.join(".git/trail/worktrees/main/sessions"));
    assert!(lines
        .iter()
        .any(|l| l["kind"] == "event" && l["path"] == "sig.txt"));
    let end = lines.last().unwrap();
    assert_eq!(end["kind"], "end");
    assert_eq!(end["events"], 1);
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
    assert!(!f.root.join(".git/worktrees/wt/trail").exists());
    let dir = f.root.join(".git/trail/worktrees/wt/sessions");
    let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
    assert_eq!(entries.len(), 1, "one session file in the common dir");
    let lines = session_lines(&dir);
    assert_eq!(lines[0]["worktree_id"], "wt");
    assert_eq!(lines[0]["branch"], "rec");
    assert_eq!(lines[0]["worktree_path"], wt.to_str().unwrap());
    assert_eq!(lines[0]["repository_root"], f.root.to_str().unwrap());
    assert_eq!(lines.last().unwrap()["kind"], "end");
}

/// Records a session in `dir`: modify, commit, modify. Returns after the
/// recorder has stopped.
fn record_session_with_commit(dir: &Path) {
    let child = Command::new(env!("CARGO_BIN_EXE_trail"))
        .args(["start", "--stop-after", "7", "--quiet"])
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    write(dir, "src/lib.rs", "fn a() {}\nfn b() {}\n// session one\n");
    write(dir, "src/one.rs", "one\n");
    std::thread::sleep(std::time::Duration::from_millis(1500));
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", "recorded commit"]);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    write(dir, "src/lib.rs", "fn a() {}\nfn b() {}\n// session two\n");
    write(dir, "src/two.rs", "two\n");
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn history_integrates_recorded_checkpoints_around_commits() {
    let f = Fixture::with_feature("main");
    record_session_with_commit(&f.root);
    // A file touched after the session ended keeps its mtime-based event.
    std::thread::sleep(std::time::Duration::from_millis(2500));
    write(&f.root, "later.txt", "after the session\n");

    let json = trail_json(&f.root, &["history"]);
    let events = json["events"].as_array().unwrap();
    let kinds: Vec<String> = events
        .iter()
        .filter(|e| e["type"] == "checkpoint" || e["type"] == "commit")
        .map(|e| {
            if e["type"] == "commit" {
                format!("commit:{}", e["summary"].as_str().unwrap())
            } else {
                "checkpoint".to_string()
            }
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "commit:add feature module",
            "checkpoint",
            "commit:recorded commit",
            "checkpoint"
        ],
        "{events:#?}"
    );

    let checkpoints: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e["type"] == "checkpoint")
        .collect();
    assert!(checkpoints
        .iter()
        .all(|c| c["source"] == "trail_recorder" && c["confidence"] == "exact"));
    assert!(checkpoints[0]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["path"] == "src/one.rs" && c["kind"] == "created"));
    assert!(checkpoints[1]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["path"] == "src/lib.rs" && c["kind"] == "modified"));

    // Inferred working tree events for recorded files are replaced ...
    let inferred: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e["confidence"] == "inferred")
        .collect();
    assert!(
        !inferred.iter().any(|e| e["files"][0] == "src/lib.rs"),
        "{inferred:?}"
    );
    assert!(
        !inferred.iter().any(|e| e["files"][0] == "src/two.rs"),
        "{inferred:?}"
    );
    // ... but a change made after the recorder stopped is still inferred.
    assert!(
        inferred.iter().any(|e| e["files"][0] == "later.txt"),
        "{inferred:?}"
    );

    let text = trail_ok(&f.root, &["history"]);
    assert!(text.contains("Session "));
    assert!(text.contains("+ src/one.rs"));
    assert!(text.contains("~ src/lib.rs"));
    assert!(!text.contains("Modified src/lib.rs"));
    assert!(text.contains("Added later.txt"));

    // The overview shows checkpoints too.
    let overview = trail_ok(&f.root, &[]);
    assert!(overview.contains("1 created, 1 modified"));
}

#[test]
fn sessions_are_listed_after_the_worktree_is_removed() {
    let f = Fixture::with_feature("main");
    let wt_dir = TempDir::new().unwrap();
    let wt = wt_dir.path().join("agent-wt");
    git(
        &f.root,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "agent"],
    );
    let wt = wt.canonicalize().unwrap();
    record_session_with_commit(&wt);

    let json = trail_json(&f.root, &["sessions"]);
    assert_eq!(json["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(json["sessions"][0]["worktree_exists"], true);
    assert_eq!(json["sessions"][0]["checkpoints"], 2);

    git(
        &f.root,
        &["worktree", "remove", "--force", wt.to_str().unwrap()],
    );
    assert!(!wt.exists());
    assert!(!f.root.join(".git/worktrees/agent-wt").exists());

    let json = trail_json(&f.root, &["sessions"]);
    let s = &json["sessions"][0];
    assert_eq!(s["worktree_exists"], false);
    assert_eq!(s["branch"], "agent");
    assert_eq!(s["worktree_id"], "agent-wt");
    assert_eq!(s["checkpoints"], 2);
    assert!(s["ended_at"].is_string());
    let text = trail_ok(&f.root, &["sessions"]);
    assert!(text.contains("Development Sessions"));
    assert!(text.contains("agent  (worktree removed)"));
    assert!(text.contains("2 checkpoints"));

    // The branch still exists, so its trail can be viewed from the main
    // worktree after checking it out.
    git(&f.root, &["checkout", "-q", "agent"]);
    let history = trail_json(&f.root, &["history"]);
    assert_eq!(
        history["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == "checkpoint")
            .count(),
        2
    );

    // A new, unrelated branch that reuses the name does not inherit the
    // trail: the session's start_head is not part of its history.
    git(&f.root, &["checkout", "-q", "main"]);
    git(&f.root, &["branch", "-D", "agent"]);
    git(&f.root, &["checkout", "-q", "--orphan", "agent"]);
    git(&f.root, &["commit", "-qm", "fresh start"]);
    git(&f.root, &["branch", "-f", "main", "agent"]); // give the orphan a base
    write(&f.root, "x.txt", "x\n");
    git(&f.root, &["add", "-A"]);
    git(&f.root, &["commit", "-qm", "on the new agent branch"]);
    let history = trail_json(&f.root, &["history"]);
    assert_eq!(
        history["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == "checkpoint")
            .count(),
        0,
        "{history:#?}"
    );
}

#[test]
fn sessions_survive_corrupt_and_truncated_logs() {
    let f = Fixture::with_feature("main");
    let dir = f.root.join(".git/trail/worktrees/main/sessions");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("session-good.jsonl"),
        concat!(
            "{\"kind\":\"session\",\"version\":1,\"session_id\":\"good\",\"repository_root\":\"/r\",\"worktree_id\":\"main\",\"worktree_path\":\"/r\",\"branch\":\"feature\",\"base_commit\":null,\"start_head\":null,\"started_at\":\"2026-09-22T01:00:00Z\"}\n",
            "{\"kind\":\"event\",\"timestamp\":\"2026-09-22T01:00:01Z\",\"path\":\"a.rs\",\"type\":\"modified\",\"before_hash\":\"1\",\"after_hash\":\"2\"}\n",
            "this line is garbage\n",
            "{\"kind\":\"event\",\"timestamp\":\"2026-09-22T01:00:02Z\",\"path\":\"b.rs\",\"type\":\"created\",\"before_hash\":null,\"after_hash\":\"3\"}\n",
            "{\"kind\":\"end\",\"ended_at\":\"2026-09-22T01:00:0"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("session-bad.jsonl"), "not a session at all\n").unwrap();
    std::fs::write(dir.join("notes.txt"), "ignored\n").unwrap();

    let json = trail_json(&f.root, &["sessions"]);
    let sessions = json["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["session_id"], "good");
    assert_eq!(sessions[0]["events"], 2);
    assert_eq!(sessions[0]["checkpoints"], 1);
    assert!(
        sessions[0]["ended_at"].is_null(),
        "truncated end line is not an end"
    );
    let text = trail_ok(&f.root, &["sessions"]);
    assert!(text.contains("(open)"));
}

/// Writes a synthetic session with two checkpoints (40 s apart) so edit tests
/// have stable ids: `synth.1` (src/lib.rs, src/one.rs) and `synth.2`
/// (src/lib.rs, tests/t.rs).
fn write_synthetic_session(root: &Path) {
    let dir = root.join(".git/trail/worktrees/main/sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let now = chrono::Utc::now();
    let t = |secs: i64| (now + chrono::Duration::seconds(secs)).to_rfc3339();
    let header = format!(
        "{{\"kind\":\"session\",\"version\":1,\"session_id\":\"synth\",\"repository_root\":\"{r}\",\"worktree_id\":\"main\",\"worktree_path\":\"{r}\",\"branch\":\"feature\",\"base_commit\":null,\"start_head\":null,\"started_at\":\"{s}\"}}\n",
        r = root.display(),
        s = t(0)
    );
    let ev = |secs: i64, path: &str, kind: &str, before: &str, after: &str| {
        format!(
            "{{\"kind\":\"event\",\"timestamp\":\"{}\",\"path\":\"{path}\",\"type\":\"{kind}\",\"before_hash\":{before},\"after_hash\":{after}}}\n",
            t(secs)
        )
    };
    let text = header
        + &ev(1, "src/lib.rs", "modified", "\"a\"", "\"b\"")
        + &ev(2, "src/one.rs", "created", "null", "\"c\"")
        + &ev(45, "src/lib.rs", "modified", "\"b\"", "\"d\"")
        + &ev(46, "tests/t.rs", "created", "null", "\"e\"")
        + &format!(
            "{{\"kind\":\"end\",\"ended_at\":\"{}\",\"events\":4}}\n",
            t(50)
        );
    std::fs::write(dir.join("session-synth.jsonl"), text).unwrap();
}

fn checkpoint_ids(json: &serde_json::Value) -> Vec<(String, Vec<String>)> {
    json["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "checkpoint")
        .map(|e| {
            (
                e["id"].as_str().unwrap().to_string(),
                e["changes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| c["path"].as_str().unwrap().to_string())
                    .collect(),
            )
        })
        .collect()
}

#[test]
fn edit_print_renders_checkpoints_with_commit_context() {
    let f = Fixture::with_feature("main");
    write_synthetic_session(&f.root);
    let text = trail_ok(&f.root, &["edit", "--print"]);
    assert!(text.starts_with("# trail://feature\n"));
    assert!(text.contains("# commit ") && text.contains("add feature module"));
    assert!(
        text.contains("[checkpoint:synth.1]\ntitle = \nhidden = false\n\nsrc/lib.rs\nsrc/one.rs\n")
    );
    assert!(
        text.contains("[checkpoint:synth.2]\ntitle = \nhidden = false\n\nsrc/lib.rs\ntests/t.rs\n")
    );
}

#[test]
fn edit_from_file_updates_title_note_grouping_order_and_hidden() {
    let f = Fixture::with_feature("main");
    write_synthetic_session(&f.root);
    let edited = "\
# comments are fine
[checkpoint:synth.2]
title = Tests
hidden = false
note = written after lunch

src/lib.rs
tests/t.rs
src/one.rs

[checkpoint:synth.1]
title = Foundation
hidden = true

src/lib.rs
";
    let file = f.root.join("edit.trail");
    std::fs::write(&file, edited).unwrap();
    let out = trail_ok(&f.root, &["edit", "--from", file.to_str().unwrap()]);
    assert!(out.contains("2 title(s)"), "{out}");
    assert!(out.contains("1 note(s)"), "{out}");
    assert!(out.contains("1 visibility change(s)"), "{out}");
    assert!(out.contains("1 file(s) regrouped"), "{out}");
    assert!(out.contains("checkpoints reordered"), "{out}");

    // Overlay is stored separately from the raw log, which is untouched.
    let meta: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(f.root.join(".git/trail/metadata/checkpoints.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(meta["version"], 1);
    assert_eq!(meta["checkpoints"]["synth.1"]["title"], "Foundation");
    assert_eq!(meta["checkpoints"]["synth.1"]["hidden"], true);
    assert_eq!(
        meta["checkpoints"]["synth.2"]["annotation"],
        "written after lunch"
    );
    assert_eq!(meta["moves"][0]["path"], "src/one.rs");
    assert_eq!(meta["order"], serde_json::json!(["synth.2", "synth.1"]));
    let raw = std::fs::read_to_string(
        f.root
            .join(".git/trail/worktrees/main/sessions/session-synth.jsonl"),
    )
    .unwrap();
    assert!(!raw.contains("Foundation"));
    assert_eq!(raw.lines().count(), 6);

    // The trail reflects the overlay: order swapped, file moved, title set.
    let json = trail_json(&f.root, &["history"]);
    let cps = checkpoint_ids(&json);
    assert_eq!(cps[0].0, "synth.2");
    assert_eq!(
        cps[0].1,
        vec!["src/one.rs", "src/lib.rs", "tests/t.rs"],
        "changes stay chronological"
    );
    assert_eq!(cps[1].0, "synth.1");
    assert_eq!(cps[1].1, vec!["src/lib.rs"]);
    let hidden = json["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "synth.1")
        .unwrap();
    assert_eq!(hidden["hidden"], true);
    assert_eq!(hidden["title"], "Foundation");
    let text = trail_ok(&f.root, &["history"]);
    assert!(text.contains("\"Tests\""));
    assert!(text.contains("written after lunch"));
    assert!(
        !text.contains("Foundation"),
        "hidden checkpoints are not rendered"
    );

    // Editing again starts from the overlaid state and is idempotent.
    let again = trail_ok(&f.root, &["edit", "--print"]);
    assert!(again.contains("[checkpoint:synth.2]\ntitle = Tests\nhidden = false\nnote = written after lunch\n\nsrc/one.rs\nsrc/lib.rs\ntests/t.rs\n"));
    let file2 = f.root.join("edit2.trail");
    std::fs::write(&file2, &again).unwrap();
    let out = trail_ok(&f.root, &["edit", "--from", file2.to_str().unwrap()]);
    assert!(out.contains("No changes."), "{out}");
    let meta2: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(f.root.join(".git/trail/metadata/checkpoints.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(meta2, meta);
}

#[test]
fn edit_rejects_invalid_input_and_keeps_metadata_intact() {
    let f = Fixture::with_feature("main");
    write_synthetic_session(&f.root);
    let good = f.root.join("good.trail");
    std::fs::write(
        &good,
        "[checkpoint:synth.1]\ntitle = Keep me\n\nsrc/lib.rs\nsrc/one.rs\n[checkpoint:synth.2]\n\nsrc/lib.rs\ntests/t.rs\n",
    )
    .unwrap();
    trail_ok(&f.root, &["edit", "--from", good.to_str().unwrap()]);
    let meta_path = f.root.join(".git/trail/metadata/checkpoints.json");
    let before = std::fs::read_to_string(&meta_path).unwrap();

    let cases = [
        ("src/lib.rs\n", "before the first"),
        ("[checkpoint:synth.1]\ntitle = x\n[checkpoint:synth.2]\n", "was removed"),
        ("[checkpoint:nope]\n", "unknown checkpoint"),
        ("[checkpoint:synth.1]\ncolor = red\n", "unknown field"),
        ("[commit:abc]\n", "cannot be edited"),
        (
            "[checkpoint:synth.1]\nsrc/lib.rs\nsrc/one.rs\nnew.rs\n[checkpoint:synth.2]\nsrc/lib.rs\ntests/t.rs\n",
            "invented",
        ),
        (
            "[checkpoint:synth.1]\nsrc/lib.rs\nsrc/one.rs\nsrc/lib.rs\n[checkpoint:synth.2]\ntests/t.rs\n",
            "listed twice",
        ),
    ];
    for (text, expected) in cases {
        let bad = f.root.join("bad.trail");
        std::fs::write(&bad, text).unwrap();
        let out = trail(&f.root, &["edit", "--from", bad.to_str().unwrap()]);
        assert!(!out.ok, "{text:?} should fail");
        assert!(out.stderr.contains(expected), "{text:?}: {}", out.stderr);
        assert!(out.stderr.contains("no changes were applied"));
        assert_eq!(
            std::fs::read_to_string(&meta_path).unwrap(),
            before,
            "{text:?} changed metadata"
        );
    }
    let leftovers: Vec<_> = std::fs::read_dir(meta_path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != "checkpoints.json")
        .collect();
    assert!(
        leftovers.is_empty(),
        "atomic write leaves no temp files: {leftovers:?}"
    );
}

#[cfg(unix)]
fn fake_editor(dir: &Path, script_body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("fake-editor.sh");
    std::fs::write(&script, format!("#!/bin/sh\n{script_body}\n")).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[cfg(unix)]
#[test]
fn edit_launches_visual_then_editor_and_applies_the_saved_buffer() {
    let f = Fixture::with_feature("main");
    write_synthetic_session(&f.root);
    // The "editor" rewrites the title of synth.1 in place.
    let script = fake_editor(
        &f.root,
        "sed -i.bak 's/^title = $/title = From the editor/' \"$1\" && rm -f \"$1.bak\"",
    );
    let out = Command::new(env!("CARGO_BIN_EXE_trail"))
        .arg("edit")
        .current_dir(&f.root)
        .env("VISUAL", &script)
        .env("EDITOR", "/nonexistent/editor")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("2 title(s)"));
    let json = trail_json(&f.root, &["history"]);
    let titles: Vec<&str> = json["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "checkpoint")
        .map(|e| e["title"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(titles, vec!["From the editor", "From the editor"]);

    // A failing editor applies nothing.
    let failing = fake_editor(&f.root, "exit 3");
    let out = Command::new(env!("CARGO_BIN_EXE_trail"))
        .arg("edit")
        .current_dir(&f.root)
        .env_remove("VISUAL")
        .env("EDITOR", &failing)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("editor failed"));

    // No editor at all is a clear error. PATH holds only git so the
    // platform fallback (vi) cannot be found either.
    let git_path =
        String::from_utf8(Command::new("which").arg("git").output().unwrap().stdout).unwrap();
    let bin = f.root.join("only-git");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(git_path.trim(), bin.join("git")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_trail"))
        .arg("edit")
        .current_dir(&f.root)
        .env_remove("VISUAL")
        .env_remove("EDITOR")
        .env("PATH", &bin)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no editor configured"));
}

#[cfg(unix)]
#[test]
fn open_launches_the_editor_on_the_worktree_file() {
    let f = Fixture::with_feature("main");
    let log = f.root.join("opened.log");
    let script = fake_editor(&f.root, &format!("echo \"$1\" >> {}", log.display()));
    let out = Command::new(env!("CARGO_BIN_EXE_trail"))
        .args(["open", "lib.rs"])
        .current_dir(f.root.join("src"))
        .env("EDITOR", &script)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&log).unwrap().trim(),
        f.root.join("src/lib.rs").to_str().unwrap()
    );

    let out = trail(&f.root, &["open", "missing.rs"]);
    assert!(!out.ok);
    let out = trail(&f.root, &["open", "src/lib.rs", "--at", "synth.1"]);
    assert!(!out.ok);
    assert!(out.stderr.contains("unknown checkpoint"), "{}", out.stderr);
}

#[test]
fn snapshots_restore_file_content_at_a_checkpoint() {
    let f = Fixture::with_feature("main");
    record_session_with_commit(&f.root);

    let json = trail_json(&f.root, &["history"]);
    let cps = checkpoint_ids(&json);
    assert_eq!(cps.len(), 2, "{json:#?}");
    let (cp1, cp2) = (cps[0].0.as_str(), cps[1].0.as_str());
    let session_id = cp1.rsplit_once('.').unwrap().0;

    // The session's snapshots are protected by a ref chain, one commit per
    // protection point (commit boundary, then session end).
    let refname = format!("refs/trail/sessions/{session_id}");
    let log = Command::new("git")
        .args(["log", "--format=%s", &refname])
        .current_dir(&f.root)
        .output()
        .unwrap();
    assert!(log.status.success());
    let subjects = String::from_utf8_lossy(&log.stdout);
    assert!(subjects.lines().count() >= 2, "{subjects}");
    assert!(subjects.contains("trail snapshots for session"));

    let open = |args: &[&str]| trail_ok(&f.root, args);
    assert_eq!(
        open(&["open", "src/lib.rs", "--at", cp1, "--print"]),
        "fn a() {}\nfn b() {}\n// session one\n"
    );
    assert_eq!(
        open(&["open", "src/lib.rs", "--at", cp2, "--print"]),
        "fn a() {}\nfn b() {}\n// session two\n"
    );
    // A file untouched in cp2 shows its latest earlier version.
    assert_eq!(
        open(&["open", "src/one.rs", "--at", cp2, "--print"]),
        "one\n"
    );
    // Relative paths from a subdirectory resolve like everywhere else.
    assert_eq!(
        trail_ok(
            &f.root.join("src"),
            &["open", "lib.rs", "--at", cp1, "--print"]
        ),
        "fn a() {}\nfn b() {}\n// session one\n"
    );
    // Without --at, --print shows the worktree file.
    assert_eq!(
        open(&["open", "src/lib.rs", "--print"]),
        "fn a() {}\nfn b() {}\n// session two\n"
    );

    let out = trail(&f.root, &["open", "src/two.rs", "--at", cp1, "--print"]);
    assert!(!out.ok);
    assert!(
        out.stderr.contains("not recorded in or before"),
        "{}",
        out.stderr
    );
    let out = trail(
        &f.root,
        &["open", "src/lib.rs", "--at", "nope.1", "--print"],
    );
    assert!(!out.ok);
    assert!(out.stderr.contains("unknown checkpoint"), "{}", out.stderr);

    // Snapshots survive an aggressive gc because the ref keeps them reachable.
    git(&f.root, &["gc", "-q", "--prune=now"]);
    assert_eq!(
        open(&["open", "src/lib.rs", "--at", cp1, "--print"]),
        "fn a() {}\nfn b() {}\n// session one\n"
    );
    assert_eq!(
        open(&["open", "src/one.rs", "--at", cp1, "--print"]),
        "one\n"
    );

    let sessions = trail_json(&f.root, &["sessions"]);
    assert_eq!(sessions["sessions"][0]["snapshots"], true);
}

#[test]
fn sessions_without_snapshots_are_marked() {
    let f = Fixture::with_feature("main");
    write_synthetic_session(&f.root);
    let sessions = trail_json(&f.root, &["sessions"]);
    assert_eq!(sessions["sessions"][0]["snapshots"], false);
    assert!(trail_ok(&f.root, &["sessions"]).contains("(no snapshots)"));
    let out = trail(
        &f.root,
        &["open", "src/lib.rs", "--at", "synth.1", "--print"],
    );
    assert!(!out.ok);
    assert!(
        out.stderr.contains("no snapshot for src/lib.rs"),
        "{}",
        out.stderr
    );
}

/// Adds a bare remote and pushes the current branch to it (with -u).
fn push_to_bare_remote(f: &Fixture) -> TempDir {
    let remote = TempDir::new().unwrap();
    git(remote.path(), &["init", "-q", "--bare"]);
    git(
        &f.root,
        &["remote", "add", "origin", remote.path().to_str().unwrap()],
    );
    git(&f.root, &["push", "-q", "-u", "origin", "HEAD"]);
    remote
}

#[test]
fn changes_falls_back_from_last_push_to_upstream_to_base() {
    let f = Fixture::with_feature("main");

    // No remote at all: the base branch is the baseline, and says so.
    let json = trail_json(&f.root, &["changes"]);
    assert_eq!(json["repository"]["since"]["kind"], "base_branch");
    assert_eq!(json["commits"].as_array().unwrap().len(), 1);
    assert!(trail_ok(&f.root, &["changes"]).contains("Changes since base main"));

    // After a push: last push wins, nothing has changed since.
    let _remote = push_to_bare_remote(&f);
    let json = trail_json(&f.root, &["changes"]);
    assert_eq!(json["repository"]["since"]["kind"], "last_push");
    assert_eq!(
        json["repository"]["since"]["label"],
        "last push to origin/feature"
    );
    assert_eq!(json["commits"].as_array().unwrap().len(), 0);
    assert_eq!(json["files"].as_array().unwrap().len(), 0);

    // Commit + staged + unstaged + untracked after the push are all included.
    write(
        &f.root,
        "src/lib.rs",
        "fn a() {}\nfn b() {}\n// committed after push\n",
    );
    git(&f.root, &["commit", "-qam", "after push"]);
    write(&f.root, "README.md", "hello\nstaged\n");
    git(&f.root, &["add", "README.md"]);
    write(
        &f.root,
        "src/feature.rs",
        "pub fn feature() {}\n// unstaged\n",
    );
    write(&f.root, "notes.txt", "untracked\n");
    let json = trail_json(&f.root, &["changes"]);
    assert_eq!(json["commits"][0]["summary"], "after push");
    let files = json["files"].as_array().unwrap();
    let find = |p: &str| {
        files
            .iter()
            .find(|x| x["path"] == p)
            .unwrap_or_else(|| panic!("{p} missing: {files:?}"))
    };
    assert_eq!(find("src/lib.rs")["kind"], "modified");
    assert!(
        find("src/lib.rs")["status"].is_null(),
        "committed change has no worktree status"
    );
    assert_eq!(find("README.md")["status"]["staged"], "modified");
    assert_eq!(find("src/feature.rs")["status"]["unstaged"], "modified");
    assert_eq!(find("notes.txt")["kind"], "added");
    assert_eq!(find("notes.txt")["status"]["untracked"], true);
    assert_eq!(json["counts"]["staged"], 1);
    assert_eq!(json["counts"]["unstaged"], 1);
    assert_eq!(json["counts"]["untracked"], 1);
    assert_eq!(json["stats"]["additions"], 4);
    let text = trail_ok(&f.root, &["changes"]);
    assert!(text.contains("Changes since last push to origin/feature"));
    assert!(text.contains("~ src/lib.rs\n      +1  committed"));
    assert!(text.contains("+ notes.txt\n      +1  untracked"));
    assert!(text.contains("1 commit, 0 checkpoints"));

    // Upstream moved without a push (a fetch): upstream and last push differ.
    let head = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&f.root)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    git(
        &f.root,
        &[
            "update-ref",
            "-m",
            "fetch: fast-forward",
            "refs/remotes/origin/feature",
            head.trim(),
        ],
    );
    let upstream = trail_json(&f.root, &["changes", "--since", "upstream"]);
    assert_eq!(upstream["repository"]["since"]["kind"], "upstream");
    assert_eq!(upstream["commits"].as_array().unwrap().len(), 0);
    let push = trail_json(&f.root, &["changes", "--since", "push"]);
    assert_eq!(push["repository"]["since"]["kind"], "last_push");
    assert_eq!(push["commits"].as_array().unwrap().len(), 1);

    // Explicit revision and errors.
    let rev = trail_json(&f.root, &["changes", "--since", "HEAD~1"]);
    assert_eq!(rev["repository"]["since"]["kind"], "commit");
    assert_eq!(rev["commits"].as_array().unwrap().len(), 1);
    let out = trail(&f.root, &["changes", "--since", "nope"]);
    assert!(!out.ok);
    assert!(out.stderr.contains("not a known revision"));
    let g = Fixture::with_feature("main");
    let out = trail(&g.root, &["changes", "--since", "push"]);
    assert!(!out.ok);
    assert!(
        out.stderr.contains("no push of this branch"),
        "{}",
        out.stderr
    );
}

#[test]
fn since_is_a_view_filter_for_history_and_edit() {
    let f = Fixture::with_feature("main");
    let _remote = push_to_bare_remote(&f);
    write_synthetic_session(&f.root); // two checkpoints, timestamps = now
    write(
        &f.root,
        "src/lib.rs",
        "fn a() {}\nfn b() {}\n// after push\n",
    );
    git(&f.root, &["commit", "-qam", "after push"]);

    // Default view: everything since the base branch.
    let all = trail_json(&f.root, &["history"]);
    assert_eq!(all["repository"]["since"]["kind"], "base_branch");
    assert_eq!(
        all["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == "commit")
            .count(),
        2
    );

    // Since the push: only the later commit; recorded checkpoints are still
    // there because they happened after the pushed commit.
    let since = trail_json(&f.root, &["history", "--since", "push"]);
    assert_eq!(since["repository"]["since"]["kind"], "last_push");
    let commits: Vec<&str> = since["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "commit")
        .map(|e| e["summary"].as_str().unwrap())
        .collect();
    assert_eq!(commits, vec!["after push"]);
    assert_eq!(checkpoint_ids(&since).len(), 2);
    let text = trail_ok(&f.root, &["history", "--since", "push"]);
    assert!(text.contains("Since\n  last push to origin/feature"));
    assert!(
        !trail_ok(&f.root, &["history"]).contains("Since\n"),
        "default view does not show a Since line"
    );

    // The overview honours --since too, and edit renders the same window.
    assert!(trail_ok(&f.root, &["--since", "push"]).contains("Since\n  last push"));
    let doc = trail_ok(&f.root, &["edit", "--print", "--since", "push"]);
    assert!(doc.contains("# commit ") && doc.contains("after push"));
    assert!(!doc.contains("add feature module"));
    assert!(doc.contains("[checkpoint:synth.1]"));

    // Raw log and metadata are untouched by any of this.
    assert_eq!(
        std::fs::read_to_string(
            f.root
                .join(".git/trail/worktrees/main/sessions/session-synth.jsonl")
        )
        .unwrap()
        .lines()
        .count(),
        6
    );
    assert!(!f.root.join(".git/trail/metadata/checkpoints.json").exists());
}

#[test]
fn baseline_after_amend_uses_the_common_ancestor() {
    let f = Fixture::with_feature("main");
    let _remote = push_to_bare_remote(&f);
    git(
        &f.root,
        &[
            "commit",
            "-q",
            "--amend",
            "-m",
            "add feature module (amended)",
        ],
    );
    let json = trail_json(&f.root, &["changes"]);
    let since = &json["repository"]["since"];
    assert_eq!(since["kind"], "last_push");
    assert_ne!(since["commit"], since["start"]);
    assert_eq!(
        json["commits"][0]["summary"],
        "add feature module (amended)"
    );
    assert!(trail_ok(&f.root, &["changes"]).contains("common ancestor"));
}
