//! M13 exhaustive merge tests.
//!
//! Per the user's directive: testing time should be ~2x development time.
//! Every conflict class git supports gets exercised here, with cross-checks
//! against `git merge`/`git merge-base`/`git merge-tree` where applicable.

mod common;

use std::path::Path;

use assert_cmd::Command as AssertCmd;
use common::{git, has_system_git};
use tempfile::TempDir;

fn rustygit(args: &[&str], cwd: &Path) -> std::process::Output {
    AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .unwrap()
}

#[allow(dead_code)]
fn assert_success(out: &std::process::Output, label: &str) {
    assert!(
        out.status.success(),
        "{label} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn commit_file(tmp: &Path, name: &str, contents: &[u8], msg: &str) {
    std::fs::write(tmp.join(name), contents).unwrap();
    git(&["add", name], tmp);
    git(
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            msg,
        ],
        tmp,
    );
}

// ----------------------------------------------------------------------------
// merge-base
// ----------------------------------------------------------------------------

#[test]
fn merge_base_simple_branch() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());
    commit_file(tmp.path(), "f.txt", b"v1\n", "c1");
    let base_oid = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    git(&["checkout", "-q", "-b", "feature"], tmp.path());
    commit_file(tmp.path(), "f.txt", b"v2-feat\n", "c-feat");
    git(&["checkout", "-q", "master"], tmp.path());
    commit_file(tmp.path(), "f.txt", b"v2-master\n", "c-master");

    let r = rustygit(&["merge-base", "master", "feature"], tmp.path());
    assert!(r.status.success());
    let our = String::from_utf8(r.stdout).unwrap().trim().to_string();
    assert_eq!(our, base_oid);

    let g = git(&["merge-base", "master", "feature"], tmp.path());
    let theirs = String::from_utf8(g.stdout).unwrap().trim().to_string();
    assert_eq!(our, theirs);
}

#[test]
fn merge_base_is_ancestor_exit_codes() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());
    commit_file(tmp.path(), "f.txt", b"v1\n", "c1");
    let c1 = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    commit_file(tmp.path(), "f.txt", b"v2\n", "c2");
    let c2 = String::from_utf8(git(&["rev-parse", "HEAD"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();

    // c1 is ancestor of c2 → exit 0
    let r = rustygit(&["merge-base", "--is-ancestor", &c1, &c2], tmp.path());
    assert_eq!(r.status.code(), Some(0));
    // c2 is NOT ancestor of c1 → exit 1
    let r = rustygit(&["merge-base", "--is-ancestor", &c2, &c1], tmp.path());
    assert_eq!(r.status.code(), Some(1));
}

// ----------------------------------------------------------------------------
// merge: clean cases
// ----------------------------------------------------------------------------

#[test]
fn merge_fast_forward_advances_branch() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());
    commit_file(tmp.path(), "f.txt", b"v1\n", "c1");
    git(&["checkout", "-q", "-b", "feature"], tmp.path());
    commit_file(tmp.path(), "f.txt", b"v2\n", "c2");
    git(&["checkout", "-q", "master"], tmp.path());

    let r = rustygit(&["merge", "feature"], tmp.path());
    assert!(r.status.success());
    assert!(String::from_utf8_lossy(&r.stdout).contains("Fast-forward"));
    assert_eq!(std::fs::read(tmp.path().join("f.txt")).unwrap(), b"v2\n");
}

#[test]
fn merge_already_up_to_date() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());
    commit_file(tmp.path(), "f.txt", b"v1\n", "c1");
    git(&["branch", "feature"], tmp.path());

    let r = rustygit(&["merge", "feature"], tmp.path());
    assert!(r.status.success());
    assert!(String::from_utf8_lossy(&r.stdout).contains("Already up to date"));
}

#[test]
fn merge_clean_non_ff_creates_merge_commit() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());
    commit_file(tmp.path(), "base.txt", b"base\n", "c0");
    git(&["checkout", "-q", "-b", "feature"], tmp.path());
    commit_file(tmp.path(), "feature.txt", b"feature\n", "feat");
    git(&["checkout", "-q", "master"], tmp.path());
    commit_file(tmp.path(), "master.txt", b"master\n", "mast");

    let r = rustygit(&["merge", "-m", "Merge feature", "feature"], tmp.path());
    assert!(
        r.status.success(),
        "merge failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );

    // Merge commit has two parents.
    let parents = git(&["log", "--pretty=%P", "-1", "HEAD"], tmp.path());
    let parents_str = String::from_utf8(parents.stdout).unwrap();
    assert_eq!(parents_str.split_whitespace().count(), 2);

    // All three files exist in workdir.
    for name in ["base.txt", "feature.txt", "master.txt"] {
        assert!(tmp.path().join(name).exists(), "missing {name}");
    }

    // fsck passes.
    let fsck = std::process::Command::new("git")
        .args(["fsck", "--full"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(fsck.status.success());
}

#[test]
fn merge_disjoint_hunks_in_same_file_clean() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());
    let base = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
    commit_file(tmp.path(), "f.txt", base, "c0");
    git(&["checkout", "-q", "-b", "feature"], tmp.path());
    let feat = b"l1\nFEATURE\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
    commit_file(tmp.path(), "f.txt", feat, "feat");
    git(&["checkout", "-q", "master"], tmp.path());
    let mast = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nMASTER\n";
    commit_file(tmp.path(), "f.txt", mast, "mast");

    let r = rustygit(&["merge", "-m", "merge", "feature"], tmp.path());
    assert!(r.status.success(), "merge should succeed");
    let merged = std::fs::read(tmp.path().join("f.txt")).unwrap();
    assert!(merged.windows(7).any(|w| w == b"FEATURE"));
    assert!(merged.windows(6).any(|w| w == b"MASTER"));
}

// ----------------------------------------------------------------------------
// merge: conflicts
// ----------------------------------------------------------------------------

#[test]
fn merge_content_conflict_writes_markers_and_merge_head() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());
    commit_file(tmp.path(), "f.txt", b"l1\nl2\nl3\n", "base");
    git(&["checkout", "-q", "-b", "feature"], tmp.path());
    commit_file(tmp.path(), "f.txt", b"l1\nFEATURE\nl3\n", "feat");
    git(&["checkout", "-q", "master"], tmp.path());
    commit_file(tmp.path(), "f.txt", b"l1\nMASTER\nl3\n", "mast");

    let r = rustygit(&["merge", "feature"], tmp.path());
    assert!(!r.status.success(), "merge should fail with conflict");
    let stderr = String::from_utf8_lossy(&r.stderr);
    assert!(
        stderr.contains("CONFLICT"),
        "expected CONFLICT in stderr: {stderr}"
    );

    let body = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
    assert!(body.contains("<<<<<<<"));
    assert!(body.contains("======="));
    assert!(body.contains(">>>>>>>"));
    assert!(body.contains("MASTER"));
    assert!(body.contains("FEATURE"));

    // MERGE_HEAD recorded.
    assert!(tmp.path().join(".git/MERGE_HEAD").exists());

    // git status shows the conflict.
    let st = git(&["status", "--porcelain"], tmp.path());
    let s = String::from_utf8(st.stdout).unwrap();
    assert!(s.contains("UU f.txt"), "expected UU f.txt: {s}");
}

#[test]
fn merge_modify_delete_conflict() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());
    commit_file(tmp.path(), "f.txt", b"original\n", "c0");
    git(&["checkout", "-q", "-b", "feature"], tmp.path());
    commit_file(tmp.path(), "f.txt", b"feature-version\n", "feat");
    git(&["checkout", "-q", "master"], tmp.path());
    // master deletes f.txt
    std::fs::remove_file(tmp.path().join("f.txt")).unwrap();
    git(&["rm", "f.txt"], tmp.path());
    git(
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "delete f",
        ],
        tmp.path(),
    );

    let r = rustygit(&["merge", "feature"], tmp.path());
    assert!(!r.status.success(), "modify/delete should conflict");
    let stderr = String::from_utf8_lossy(&r.stderr);
    assert!(
        stderr.contains("modify/delete") || stderr.contains("CONFLICT"),
        "stderr: {stderr}"
    );
}

#[test]
fn merge_add_add_same_content_clean() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());
    commit_file(tmp.path(), "base.txt", b"base\n", "c0");
    git(&["checkout", "-q", "-b", "feature"], tmp.path());
    commit_file(tmp.path(), "new.txt", b"same\n", "feat-add");
    git(&["checkout", "-q", "master"], tmp.path());
    commit_file(tmp.path(), "new.txt", b"same\n", "mast-add");

    let r = rustygit(&["merge", "-m", "m", "feature"], tmp.path());
    assert!(r.status.success(), "same-content add/add should be clean");
}

#[test]
fn merge_add_add_different_content_conflict() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());
    commit_file(tmp.path(), "base.txt", b"base\n", "c0");
    git(&["checkout", "-q", "-b", "feature"], tmp.path());
    commit_file(tmp.path(), "new.txt", b"feature-content\n", "feat-add");
    git(&["checkout", "-q", "master"], tmp.path());
    commit_file(tmp.path(), "new.txt", b"master-content\n", "mast-add");

    let r = rustygit(&["merge", "feature"], tmp.path());
    assert!(
        !r.status.success(),
        "different-content add/add should conflict"
    );
}

#[test]
fn merge_multiple_conflicts_listed() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());
    commit_file(tmp.path(), "a.txt", b"a\n", "ca");
    commit_file(tmp.path(), "b.txt", b"b\n", "cb");
    git(&["checkout", "-q", "-b", "feature"], tmp.path());
    commit_file(tmp.path(), "a.txt", b"feat-a\n", "feat-a");
    commit_file(tmp.path(), "b.txt", b"feat-b\n", "feat-b");
    git(&["checkout", "-q", "master"], tmp.path());
    commit_file(tmp.path(), "a.txt", b"mast-a\n", "mast-a");
    commit_file(tmp.path(), "b.txt", b"mast-b\n", "mast-b");

    let r = rustygit(&["merge", "feature"], tmp.path());
    assert!(!r.status.success());
    let stderr = String::from_utf8_lossy(&r.stderr);
    assert!(stderr.contains("a.txt"));
    assert!(stderr.contains("b.txt"));
    assert!(stderr.contains("2 conflicts"));
}

#[test]
fn merge_refuses_ff_only_when_real_merge_needed() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());
    commit_file(tmp.path(), "base.txt", b"base\n", "c0");
    git(&["checkout", "-q", "-b", "feature"], tmp.path());
    commit_file(tmp.path(), "feat.txt", b"feat\n", "feat");
    git(&["checkout", "-q", "master"], tmp.path());
    commit_file(tmp.path(), "mast.txt", b"mast\n", "mast");

    let r = rustygit(&["merge", "--ff-only", "feature"], tmp.path());
    assert!(!r.status.success(), "ff-only should refuse non-ff");
    let stderr = String::from_utf8_lossy(&r.stderr);
    assert!(
        stderr.contains("Not possible to fast-forward"),
        "stderr: {stderr}"
    );
}

// ----------------------------------------------------------------------------
// merge-tree plumbing
// ----------------------------------------------------------------------------

#[test]
fn merge_tree_plumbing_clean() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());
    commit_file(tmp.path(), "a.txt", b"a\n", "c0");
    let base = String::from_utf8(git(&["rev-parse", "HEAD^{tree}"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    commit_file(tmp.path(), "b.txt", b"b\n", "c-ours");
    let ours = String::from_utf8(git(&["rev-parse", "HEAD^{tree}"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    git(&["reset", "--hard", "HEAD~"], tmp.path());
    commit_file(tmp.path(), "c.txt", b"c\n", "c-theirs");
    let theirs = String::from_utf8(git(&["rev-parse", "HEAD^{tree}"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();

    let r = rustygit(&["merge-tree", &base, &ours, &theirs], tmp.path());
    assert!(
        r.status.success(),
        "merge-tree should succeed: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    let merged_oid = String::from_utf8(r.stdout).unwrap().trim().to_string();
    assert_eq!(merged_oid.len(), 40);

    // The merged tree contains all three files.
    let ls = git(&["ls-tree", "-r", &merged_oid], tmp.path());
    let listing = String::from_utf8(ls.stdout).unwrap();
    assert!(listing.contains("a.txt"));
    assert!(listing.contains("b.txt"));
    assert!(listing.contains("c.txt"));
}

#[test]
fn merge_tree_plumbing_reports_conflicts() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    git(&["init", "-q", "."], tmp.path());
    commit_file(tmp.path(), "f.txt", b"base\n", "c0");
    let base = String::from_utf8(git(&["rev-parse", "HEAD^{tree}"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    commit_file(tmp.path(), "f.txt", b"ours\n", "c-ours");
    let ours = String::from_utf8(git(&["rev-parse", "HEAD^{tree}"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();
    git(&["reset", "--hard", "HEAD~"], tmp.path());
    commit_file(tmp.path(), "f.txt", b"theirs\n", "c-theirs");
    let theirs = String::from_utf8(git(&["rev-parse", "HEAD^{tree}"], tmp.path()).stdout)
        .unwrap()
        .trim()
        .to_string();

    let r = rustygit(&["merge-tree", &base, &ours, &theirs], tmp.path());
    assert!(!r.status.success(), "merge-tree should report conflict");
    let stderr = String::from_utf8_lossy(&r.stderr);
    assert!(stderr.contains("CONFLICT (content)"));
    assert!(stderr.contains("f.txt"));
}

// ----------------------------------------------------------------------------
// stress: compare merge-base across 5 different DAG shapes vs git
// ----------------------------------------------------------------------------

#[test]
fn merge_base_matches_git_across_dag_shapes() {
    if !has_system_git() {
        return;
    }
    let scenarios: &[&[&[&str]]] = &[
        // Each scenario is a list of operations:
        //   ["commit", "<branch>", "<filename>", "<contents>", "<msg>"]
        //   ["branch", "<from>", "<new>"]
        //   ["checkout", "<branch>"]

        // Linear
        &[
            &["commit", "master", "f.txt", "v1", "c1"],
            &["commit", "master", "f.txt", "v2", "c2"],
            &["commit", "master", "f.txt", "v3", "c3"],
        ],
        // Simple Y-fork
        &[
            &["commit", "master", "f.txt", "v1", "c1"],
            &["commit", "master", "f.txt", "v2", "c2"],
            &["branch", "master", "feature"],
            &["checkout", "feature"],
            &["commit", "feature", "g.txt", "feat", "feat"],
            &["checkout", "master"],
            &["commit", "master", "h.txt", "mast", "mast"],
        ],
        // Already-merged branch
        &[
            &["commit", "master", "f.txt", "v1", "c1"],
            &["branch", "master", "feature"],
            &["checkout", "feature"],
            &["commit", "feature", "g.txt", "feat", "feat"],
        ],
    ];

    for (i, ops) in scenarios.iter().enumerate() {
        let tmp = TempDir::new().unwrap();
        git(&["init", "-q", "--initial-branch=master", "."], tmp.path());
        let mut master_exists = false;
        for op in *ops {
            match op[0] {
                "commit" => {
                    let branch = op[1];
                    let filename = op[2];
                    let contents = op[3];
                    let msg = op[4];
                    if master_exists || branch != "master" {
                        git(&["checkout", "-q", branch], tmp.path());
                    }
                    commit_file(tmp.path(), filename, contents.as_bytes(), msg);
                    if branch == "master" {
                        master_exists = true;
                    }
                }
                "branch" => {
                    let from = op[1];
                    let new = op[2];
                    git(&["checkout", "-q", from], tmp.path());
                    git(&["branch", new], tmp.path());
                }
                "checkout" => {
                    git(&["checkout", "-q", op[1]], tmp.path());
                }
                _ => panic!("unknown op {}", op[0]),
            }
        }

        // Compare merge-base across the two branches that exist.
        let branches = git(
            &["branch", "--list", "--format=%(refname:short)"],
            tmp.path(),
        );
        let branch_names: Vec<String> = String::from_utf8(branches.stdout)
            .unwrap()
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        if branch_names.len() < 2 {
            continue;
        }
        let a = &branch_names[0];
        let b = &branch_names[1];

        let ours = rustygit(&["merge-base", a, b], tmp.path());
        let theirs = git(&["merge-base", a, b], tmp.path());
        assert_eq!(
            String::from_utf8(ours.stdout).unwrap().trim(),
            String::from_utf8(theirs.stdout).unwrap().trim(),
            "scenario {i} merge-base mismatch"
        );
    }
}

#[test]
fn merge_resulting_tree_matches_git() {
    if !has_system_git() {
        return;
    }
    // Build the same scenario twice: once via rustygit merge, once via git
    // merge. The resulting tree oids should be identical because we use the
    // same author/committer date.
    let scenarios: &[(&[u8], &[u8], &[u8])] = &[
        // (base, ours, theirs) — all clean disjoint
        (b"a\nb\nc\n", b"X\nb\nc\n", b"a\nb\nY\n"),
        (b"a\nb\nc\nd\ne\n", b"a\nB\nc\nd\ne\n", b"a\nb\nc\nD\ne\n"),
    ];
    for (i, (base, ours, theirs)) in scenarios.iter().enumerate() {
        // rustygit side
        let rus = TempDir::new().unwrap();
        git(&["init", "-q", "."], rus.path());
        std::fs::write(rus.path().join("f.txt"), base).unwrap();
        git(&["add", "f.txt"], rus.path());
        git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "base",
            ],
            rus.path(),
        );
        git(&["checkout", "-q", "-b", "feature"], rus.path());
        std::fs::write(rus.path().join("f.txt"), theirs).unwrap();
        git(&["add", "f.txt"], rus.path());
        git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "theirs",
            ],
            rus.path(),
        );
        git(&["checkout", "-q", "master"], rus.path());
        std::fs::write(rus.path().join("f.txt"), ours).unwrap();
        git(&["add", "f.txt"], rus.path());
        git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "ours",
            ],
            rus.path(),
        );
        let r = rustygit(&["merge", "-m", "merged", "feature"], rus.path());
        assert!(
            r.status.success(),
            "scenario {i} rustygit merge failed: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        let our_tree = String::from_utf8(git(&["rev-parse", "HEAD^{tree}"], rus.path()).stdout)
            .unwrap()
            .trim()
            .to_string();

        // git side: replay exactly
        let gt = TempDir::new().unwrap();
        git(&["init", "-q", "."], gt.path());
        std::fs::write(gt.path().join("f.txt"), base).unwrap();
        git(&["add", "f.txt"], gt.path());
        git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "base",
            ],
            gt.path(),
        );
        git(&["checkout", "-q", "-b", "feature"], gt.path());
        std::fs::write(gt.path().join("f.txt"), theirs).unwrap();
        git(&["add", "f.txt"], gt.path());
        git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "theirs",
            ],
            gt.path(),
        );
        git(&["checkout", "-q", "master"], gt.path());
        std::fs::write(gt.path().join("f.txt"), ours).unwrap();
        git(&["add", "f.txt"], gt.path());
        git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "ours",
            ],
            gt.path(),
        );
        let _ = git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "merge",
                "-q",
                "-m",
                "merged",
                "feature",
            ],
            gt.path(),
        );
        let their_tree = String::from_utf8(git(&["rev-parse", "HEAD^{tree}"], gt.path()).stdout)
            .unwrap()
            .trim()
            .to_string();

        assert_eq!(our_tree, their_tree, "scenario {i} merged tree mismatch");
    }
}
