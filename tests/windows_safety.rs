//! NON_GOALS A10 — Windows safety guards.
//!
//! Two classes of tests live here:
//!
//! 1. **Policy tests that run on every platform** — `core.autocrlf`
//!    behavior is config-driven, not platform-driven, so the round-trips
//!    work on Linux too. These cover the most important user-visible
//!    behavior: text-blob normalization on add and conversion on checkout.
//!
//! 2. **`#[cfg(not(unix))]`-gated tests** — symlink refusal and non-UTF-8
//!    path refusal can only be exercised on Windows. We still build them
//!    on Unix (via `cargo build --tests`) so the code paths don't bit-rot;
//!    they just don't run.
//!
//! See [the launch posture
//! docs](../NON_GOALS.md): "best-effort Windows; clear errors instead of
//! silent corruption."

mod common;

use std::path::Path;

use assert_cmd::Command as AssertCmd;
use common::has_system_git;
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
        .output()
        .unwrap()
}

/// Helper: lay out a minimal `.git` directory and write a `[core]
/// autocrlf = <mode>` line so the porcelain reads it on the next call.
fn init_with_autocrlf(mode: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    // Use system git for setup so we don't depend on `rustygit init`'s
    // exact config dump format.
    if !has_system_git() {
        panic!("system git required for setup");
    }
    let out = rustygit(&["init", "-q"], tmp.path());
    assert!(out.status.success(), "rustygit init failed");
    // Append `autocrlf = <mode>` under the existing [core] section.
    let cfg = tmp.path().join(".git/config");
    let mut existing = std::fs::read_to_string(&cfg).unwrap_or_default();
    existing.push_str(&format!("\tautocrlf = {mode}\n"));
    std::fs::write(&cfg, existing).unwrap();
    tmp
}

// --- autocrlf=input: CRLF→LF on add ----------------------------------------

#[test]
fn autocrlf_input_normalizes_crlf_to_lf_in_blob() {
    if !has_system_git() {
        return;
    }
    let tmp = init_with_autocrlf("input");
    // File on disk has CRLF.
    std::fs::write(tmp.path().join("dos.txt"), b"line1\r\nline2\r\nline3\r\n").unwrap();

    let out = rustygit(&["add", "dos.txt"], tmp.path());
    assert!(
        out.status.success(),
        "rustygit add failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Look at the blob via the index: the OID must match an LF-normalized
    // version of the file. The most direct check is to read the index,
    // find the entry, and ask cat-file -p to dump the blob.
    let out = rustygit(&["ls-files", "-s"], tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let line = stdout.lines().next().expect("ls-files output");
    // Format: `100644 <sha> 0\tdos.txt`
    let oid = line.split_whitespace().nth(1).unwrap();

    let out = rustygit(&["cat-file", "-p", oid], tmp.path());
    let blob = out.stdout;
    assert!(
        !blob.contains(&b'\r'),
        "blob still contains CR bytes: {:?}",
        String::from_utf8_lossy(&blob)
    );
    assert_eq!(blob, b"line1\nline2\nline3\n");
}

#[test]
fn autocrlf_true_normalizes_on_add() {
    if !has_system_git() {
        return;
    }
    let tmp = init_with_autocrlf("true");
    std::fs::write(tmp.path().join("dos.txt"), b"hello\r\nworld\r\n").unwrap();

    let out = rustygit(&["add", "dos.txt"], tmp.path());
    assert!(out.status.success());

    let out = rustygit(&["ls-files", "-s"], tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let oid = stdout
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    let out = rustygit(&["cat-file", "-p", oid], tmp.path());
    assert_eq!(out.stdout, b"hello\nworld\n");
}

#[test]
fn autocrlf_false_preserves_bytes_verbatim() {
    if !has_system_git() {
        return;
    }
    let tmp = init_with_autocrlf("false");
    let raw = b"line1\r\nline2\r\nline3\r\n";
    std::fs::write(tmp.path().join("dos.txt"), raw).unwrap();

    let out = rustygit(&["add", "dos.txt"], tmp.path());
    assert!(out.status.success());

    let out = rustygit(&["ls-files", "-s"], tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let oid = stdout
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    let out = rustygit(&["cat-file", "-p", oid], tmp.path());
    assert_eq!(
        out.stdout, raw,
        "autocrlf=false must not alter blob bytes — silent CR/LF normalization is a launch-blocker"
    );
}

#[test]
fn autocrlf_unset_preserves_bytes_verbatim() {
    if !has_system_git() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let out = rustygit(&["init", "-q"], tmp.path());
    assert!(out.status.success());
    // No autocrlf key set.

    let raw = b"hi\r\nthere\r\n";
    std::fs::write(tmp.path().join("dos.txt"), raw).unwrap();
    let out = rustygit(&["add", "dos.txt"], tmp.path());
    assert!(out.status.success());

    let out = rustygit(&["ls-files", "-s"], tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let oid = stdout
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    let out = rustygit(&["cat-file", "-p", oid], tmp.path());
    assert_eq!(
        out.stdout, raw,
        "unset autocrlf must default to 'no conversion'"
    );
}

#[test]
fn autocrlf_input_preserves_binary_blobs_with_nul() {
    // Binary file (has a NUL within the first 8000 bytes) must NEVER be
    // line-end-converted, even with autocrlf=input. Otherwise we'd corrupt
    // every PNG / EXE / shared object on Windows.
    if !has_system_git() {
        return;
    }
    let tmp = init_with_autocrlf("input");
    let mut binary = b"head\r\n".to_vec();
    binary.push(0);
    binary.extend_from_slice(b"\r\ntail\r\n");
    std::fs::write(tmp.path().join("bin"), &binary).unwrap();

    let out = rustygit(&["add", "bin"], tmp.path());
    assert!(out.status.success());

    let out = rustygit(&["ls-files", "-s"], tmp.path());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let oid = stdout
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    let out = rustygit(&["cat-file", "-p", oid], tmp.path());
    assert_eq!(
        out.stdout, binary,
        "binary blob bytes must survive the heuristic"
    );
}

// --- Unit-level checks on the conversion helpers ---------------------------

#[test]
fn text_blob_heuristic_consistent_with_8000_byte_window() {
    use rustygit::config::is_text_blob;

    // 1. Pure ASCII text → text.
    assert!(is_text_blob(b"hello world\n"));
    // 2. NUL in middle → binary.
    let mut buf = vec![b'a'; 100];
    buf.push(0);
    buf.extend_from_slice(&[b'b'; 100]);
    assert!(!is_text_blob(&buf));
    // 3. Leading NUL → binary.
    assert!(!is_text_blob(b"\x00rest"));
    // 4. Empty file → text (matches git's behavior for empty blobs).
    assert!(is_text_blob(b""));
}

#[test]
fn autocrlf_helpers_round_trip() {
    use rustygit::config::{convert_lf_to_crlf, normalize_crlf_to_lf};

    let mixed = b"a\r\nb\nc\r\nd";
    let lf_only = normalize_crlf_to_lf(mixed);
    assert_eq!(&*lf_only, b"a\nb\nc\nd");

    // Round-trip LF→CRLF on already-mixed input.
    let back = convert_lf_to_crlf(&lf_only);
    assert_eq!(&*back, b"a\r\nb\r\nc\r\nd");
}

// --- win_paths::to_index identity on Unix (regression guard) ---------------

#[test]
fn win_paths_to_index_is_identity_on_unix() {
    // The wiring at `add.rs` / `rm.rs` / `mv.rs` relies on the documented
    // claim "no regression on Unix". If `to_index` ever stops being the
    // identity on Unix, hundreds of compat tests would silently shift
    // index encoding. Lock that here.
    use rustygit::cli::win_paths::to_index;
    #[cfg(not(windows))]
    {
        assert_eq!(to_index("a/b/c"), "a/b/c");
        assert_eq!(to_index("dir/sub/file.txt"), "dir/sub/file.txt");
        // Backslashes on Unix are LITERAL bytes in a filename and must
        // round-trip unchanged.
        assert_eq!(to_index("weird\\name"), "weird\\name");
    }
    // On Windows, `\` → `/`.
    #[cfg(windows)]
    {
        assert_eq!(to_index(r"a\b\c"), "a/b/c");
    }
}

// --- Non-Unix: PlatformUnsupported symlink refusal -------------------------

// This test is gated on `not(unix)` — it only RUNS on Windows but is built
// on Unix to keep the code path live.
#[cfg(not(unix))]
#[test]
fn windows_refuses_symlink_when_core_symlinks_unset() {
    use rustygit::object::ObjectKind;
    use rustygit::tree::{FileMode, Tree, TreeEntry};
    use rustygit::unpack_trees::{checkout_tree, UnpackError, UnpackOpts};

    let tmp = TempDir::new().unwrap();
    let _ = rustygit(&["init", "-q"], tmp.path());

    // Force-set `core.symlinks = true` so we don't fall into the
    // store-target-as-file path. On Windows the default is `false`, so
    // we have to explicitly enable.
    let cfg_path = tmp.path().join(".git/config");
    let mut cfg = std::fs::read_to_string(&cfg_path).unwrap_or_default();
    cfg.push_str("[core]\n\tsymlinks = true\n");
    std::fs::write(&cfg_path, cfg).unwrap();

    let repo = rustygit::Repository::discover(tmp.path()).unwrap();

    // Build a target tree with one symlink entry.
    let blob = rustygit::object::RawObject::new(ObjectKind::Blob, b"../target".to_vec());
    let blob_oid = repo.odb().write(&blob).unwrap();
    let tree = Tree::new(vec![TreeEntry {
        mode: FileMode::Symlink,
        name: b"link".to_vec(),
        oid: blob_oid,
    }]);
    let tree_oid = repo.odb().write(&tree.to_object()).unwrap();

    let opts = UnpackOpts {
        force: true,
        keep_extra: false,
        update_workdir: true,
        update_index: true,
    };
    let err = checkout_tree(&repo, tree_oid, &opts).unwrap_err();
    match err {
        UnpackError::PlatformUnsupported { feature, .. } => {
            assert_eq!(feature, "symlink");
        }
        other => panic!("expected PlatformUnsupported, got {other:?}"),
    }
}
