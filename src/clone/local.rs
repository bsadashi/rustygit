//! Local-only clone — bytewise copy of a source repository's `objects/` and
//! `refs/`, init of a fresh destination, then a working-tree checkout.
//!
//! `clone_local` is the entry point. It is engineered to be defensive: if any
//! step fails, the partially-written destination is removed (when we created
//! it) so the user is left with the prior on-disk state.
//!
//! Out of scope for M8:
//!   - `--bare`, `--mirror`
//!   - Setting up `[remote "origin"]` / `[branch "<branch>"] remote = origin`
//!     in the destination's config (we DO write the remote-tracking refs the
//!     remote section would normally produce, but the config blocks themselves
//!     are deferred until M9 lands a real config writer).
//!   - Hardlinking objects (M10)
//!   - Non-local URLs

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::hash::{HashError, HashKind, ObjectId};
use crate::refs::{ExpectedOldValue, FullName, NewValue, RefError, ReflogMessage};
use crate::repo::{RepoError, Repository};
use crate::unpack_trees::{self, UnpackError, UnpackOpts};

/// Per-call options. Defaults match `git clone`'s no-flag behavior except that
/// quiet is currently a no-op (M8 doesn't emit per-object progress).
#[derive(Debug, Clone, Default)]
pub struct CloneOpts {
    /// Suppress the `Cloning into '<dst>'...` / `done.` lines.
    pub quiet: bool,
    /// Skip the working-tree checkout step. Equivalent to `git clone -n`.
    pub no_checkout: bool,
}

/// Errors that can arise during `clone_local`.
#[derive(thiserror::Error, Debug)]
pub enum CloneError {
    #[error("source is not a git repository: {0}")]
    NotARepo(PathBuf),
    #[error("destination exists and is not empty: {0}")]
    DestNotEmpty(PathBuf),
    #[error("io on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Repo(#[from] RepoError),
    #[error(transparent)]
    Refs(#[from] RefError),
    #[error(transparent)]
    Unpack(#[from] UnpackError),
}

impl From<HashError> for CloneError {
    fn from(e: HashError) -> Self {
        // HashError is a sub-error of RefError, so we can route through it
        // without needing a dedicated `Hash` variant on `CloneError` itself.
        // (The CloneError API contract only exposes Refs/Repo/Unpack/Io/etc.)
        CloneError::Refs(RefError::Hash(e))
    }
}

/// Clone the repository at `src` into `dst`. See module docs for caveats.
///
/// `dst` may or may not exist; if it doesn't we create it. If it does and is
/// non-empty we refuse with `DestNotEmpty`.
pub fn clone_local(src: &Path, dst: &Path, opts: &CloneOpts) -> Result<(), CloneError> {
    // 1. Resolve and validate the source.
    let src_gitdir = resolve_source_gitdir(src)?;

    // 2. Decide what we need to mutate, and validate the destination.
    let dst_existed = dst.exists();
    if dst_existed {
        if !dst.is_dir() {
            return Err(CloneError::DestNotEmpty(dst.to_path_buf()));
        }
        if dir_is_nonempty(dst).map_err(|e| CloneError::Io {
            path: dst.to_path_buf(),
            source: e,
        })? {
            return Err(CloneError::DestNotEmpty(dst.to_path_buf()));
        }
    } else {
        fs::create_dir_all(dst).map_err(|e| CloneError::Io {
            path: dst.to_path_buf(),
            source: e,
        })?;
    }

    if !opts.quiet {
        // git uses single-quotes around the destination path verbatim — no
        // canonicalization for display.
        println!("Cloning into '{}'...", dst.display());
        let _ = io::stdout().flush();
    }

    // From here on, undo the destination on any error. We track whether we
    // created `dst` itself; if we did, removal is full-tree, else we only nuke
    // the `.git` we may have written.
    match clone_inner(&src_gitdir, dst, opts) {
        Ok(()) => {
            if !opts.quiet {
                println!("done.");
            }
            Ok(())
        }
        Err(e) => {
            // Best-effort cleanup. Don't surface secondary errors — the user
            // cares about the original failure.
            if dst_existed {
                let _ = fs::remove_dir_all(dst.join(".git"));
            } else {
                let _ = fs::remove_dir_all(dst);
            }
            Err(e)
        }
    }
}

fn clone_inner(src_gitdir: &Path, dst: &Path, opts: &CloneOpts) -> Result<(), CloneError> {
    // The destination's gitdir is always `<dst>/.git` for non-bare clones.
    let dst_gitdir = dst.join(".git");

    // 3. Init the destination layout.
    let src_hash_kind = read_object_format_from_gitdir(src_gitdir)?;
    create_dst_layout(&dst_gitdir).map_err(|e| CloneError::Io {
        path: dst_gitdir.clone(),
        source: e,
    })?;
    write_dst_config(&dst_gitdir, src_hash_kind).map_err(|e| CloneError::Io {
        path: dst_gitdir.join("config"),
        source: e,
    })?;
    write_dst_description(&dst_gitdir).map_err(|e| CloneError::Io {
        path: dst_gitdir.join("description"),
        source: e,
    })?;
    write_dst_info_exclude(&dst_gitdir).map_err(|e| CloneError::Io {
        path: dst_gitdir.join("info").join("exclude"),
        source: e,
    })?;

    // 4. Copy every regular file under `objects/`.
    copy_objects(&src_gitdir.join("objects"), &dst_gitdir.join("objects"))?;

    // 5. Resolve source HEAD before opening the destination — we need it both
    //    for ref propagation and to know what to check out.
    let src_head = read_head_target(src_gitdir, src_hash_kind)?;

    // 5a. Mirror source's refs/heads into dst's refs/remotes/origin/. Open the
    //     destination as a Repository now that the layout + objects exist;
    //     RefStore will let us batch-write through a transaction. (We can't
    //     open it earlier because objects/ must exist for ObjectDb::new.)
    let dst_repo = Repository::open(dst_gitdir.clone())?;

    propagate_refs(src_gitdir, &dst_repo, src_hash_kind, &src_head)?;

    // 5b. Set the destination's local branch (or detached HEAD) to mirror
    //     source's HEAD. `propagate_refs` already populated the local branch
    //     when HEAD is symbolic; here we just write HEAD itself.
    write_dst_head(&dst_repo, &src_head)?;

    // 6. Working-tree checkout. Unless --no-checkout, materialize HEAD's tree.
    if !opts.no_checkout {
        if let Some(head_oid) = src_head.resolved_oid() {
            let tree_oid = peel_to_tree(&dst_repo, head_oid)?;
            let unpack_opts = UnpackOpts {
                update_workdir: true,
                update_index: true,
                force: false,
                keep_extra: false,
            };
            unpack_trees::checkout_tree(&dst_repo, tree_oid, &unpack_opts)?;
        }
        // If the source has an unborn HEAD (a fresh `git init` with no commits),
        // there's nothing to check out. The destination ends up in the same
        // state — HEAD points at a non-existent branch, the index is empty, the
        // workdir has no tracked files. Matches `git clone` of an empty repo.
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Source resolution
// ---------------------------------------------------------------------------

/// Strip a `file://` prefix, canonicalize, then decide whether `src` is a
/// working tree (look for `.git/` inside) or already a gitdir.
fn resolve_source_gitdir(src: &Path) -> Result<PathBuf, CloneError> {
    // `file://` may arrive as a Path because the CLI passes it as a string we
    // converted with `Path::new`. Pull the prefix off if it's there.
    let src_str = src.to_string_lossy();
    let stripped: PathBuf = if let Some(rest) = src_str.strip_prefix("file://") {
        PathBuf::from(rest)
    } else {
        src.to_path_buf()
    };

    let canonical = stripped
        .canonicalize()
        .map_err(|_| CloneError::NotARepo(stripped.clone()))?;

    // Case 1: <src>/.git exists and is a directory → use that as the gitdir.
    let dot_git = canonical.join(".git");
    if dot_git.is_dir() {
        return Ok(dot_git);
    }
    // Case 2: <src> itself contains HEAD + objects/ → it IS a gitdir
    //         (covers bare repos and direct .git path).
    if canonical.join("HEAD").is_file() && canonical.join("objects").is_dir() {
        return Ok(canonical);
    }
    Err(CloneError::NotARepo(canonical))
}

fn dir_is_nonempty(p: &Path) -> io::Result<bool> {
    let mut entries = fs::read_dir(p)?;
    Ok(entries.next().is_some())
}

// ---------------------------------------------------------------------------
// Layout creation (mirror cli::init's layout)
// ---------------------------------------------------------------------------

fn create_dst_layout(gitdir: &Path) -> io::Result<()> {
    for sub in [
        "",
        "objects",
        "objects/info",
        "objects/pack",
        "refs",
        "refs/heads",
        "refs/tags",
        "refs/remotes",
        "refs/remotes/origin",
        "info",
        "hooks",
    ] {
        fs::create_dir_all(gitdir.join(sub))?;
    }
    Ok(())
}

fn write_dst_description(gitdir: &Path) -> io::Result<()> {
    let body = "Unnamed repository; edit this file 'description' to name the repository.\n";
    write_atomic(&gitdir.join("description"), body.as_bytes())
}

fn write_dst_info_exclude(gitdir: &Path) -> io::Result<()> {
    let body = "\
# git ls-files --others --exclude-from=.git/info/exclude
# Lines that start with '#' are comments.
# For a project mostly in C, the following would be a good set of
# exclude patterns (uncomment them if you want to use them):
# *.[oa]
# *~
";
    write_atomic(&gitdir.join("info").join("exclude"), body.as_bytes())
}

/// Write a minimal `[core]` block matching the source's hash kind. We don't
/// probe filemode/ignorecase here; on a fresh clone destination the workdir
/// hasn't been written yet, so probing would land in `<dst>` not `<dst>/..`
/// which is what the existing init does. Match init's defaults reasonably:
/// filemode = true on Unix, ignorecase from a quick probe of the parent.
fn write_dst_config(gitdir: &Path, hash_kind: HashKind) -> io::Result<()> {
    let format_version = match hash_kind {
        HashKind::Sha1 => 0,
        HashKind::Sha256 => 1,
    };
    let mut s = String::new();
    s.push_str("[core]\n");
    s.push_str(&format!("\trepositoryformatversion = {format_version}\n"));
    // We default to `filemode = true` on Unix, `false` elsewhere. A real
    // implementation would probe; the destination workdir exists so it could
    // run the same probe init does, but the simpler default suffices for M8.
    let filemode = cfg!(unix);
    s.push_str(&format!("\tfilemode = {}\n", b2s(filemode)));
    s.push_str("\tbare = false\n");
    s.push_str("\tlogallrefupdates = true\n");
    if cfg!(target_os = "macos") {
        s.push_str("\tprecomposeunicode = true\n");
    }
    if matches!(hash_kind, HashKind::Sha256) {
        s.push_str("[extensions]\n");
        s.push_str("\tobjectformat = sha256\n");
    }
    write_atomic(&gitdir.join("config"), s.as_bytes())
}

fn b2s(b: bool) -> &'static str {
    if b {
        "true"
    } else {
        "false"
    }
}

fn write_atomic(target: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = target.parent().expect("target has parent");
    fs::create_dir_all(parent)?;
    let tmp = target.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, target)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Object copy
// ---------------------------------------------------------------------------

/// Walk every regular file under `<src>/objects/` and copy it into
/// `<dst>/objects/` with the same relative path. Both loose objects (under
/// `aa/bb...`) and packs (under `pack/*.pack`, `pack/*.idx`) are picked up.
fn copy_objects(src_objects: &Path, dst_objects: &Path) -> Result<(), CloneError> {
    copy_dir_files(src_objects, dst_objects)
}

fn copy_dir_files(src: &Path, dst: &Path) -> Result<(), CloneError> {
    fs::create_dir_all(dst).map_err(|e| CloneError::Io {
        path: dst.to_path_buf(),
        source: e,
    })?;
    let entries = match fs::read_dir(src) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(CloneError::Io {
                path: src.to_path_buf(),
                source: e,
            })
        }
    };
    for entry in entries {
        let entry = entry.map_err(|e| CloneError::Io {
            path: src.to_path_buf(),
            source: e,
        })?;
        let ft = entry.file_type().map_err(|e| CloneError::Io {
            path: entry.path(),
            source: e,
        })?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_dir_files(&from, &to)?;
        } else if ft.is_file() {
            fs::copy(&from, &to).map_err(|e| CloneError::Io {
                path: from.clone(),
                source: e,
            })?;
        }
        // Symlinks under objects/ aren't standard; skip them silently.
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Source HEAD reading (without opening as Repository — the source might be
// bare, or use a different layout we don't fully support).
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum SourceHead {
    /// HEAD is `ref: refs/heads/<branch>` and that branch exists at <oid>.
    Symbolic { branch: FullName, oid: ObjectId },
    /// HEAD is a direct oid (detached).
    Detached(ObjectId),
    /// HEAD points at a branch that doesn't yet exist (unborn).
    Unborn { branch: FullName },
}

impl SourceHead {
    fn resolved_oid(&self) -> Option<ObjectId> {
        match self {
            SourceHead::Symbolic { oid, .. } => Some(*oid),
            SourceHead::Detached(oid) => Some(*oid),
            SourceHead::Unborn { .. } => None,
        }
    }
}

/// Parse `<src-gitdir>/HEAD`. If it's symbolic, follow into refs/heads to find
/// the oid (which may live in loose `refs/heads/<branch>` or in `packed-refs`).
fn read_head_target(src_gitdir: &Path, hash_kind: HashKind) -> Result<SourceHead, CloneError> {
    let head_path = src_gitdir.join("HEAD");
    let bytes = fs::read(&head_path).map_err(|e| CloneError::Io {
        path: head_path.clone(),
        source: e,
    })?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| CloneError::NotARepo(src_gitdir.to_path_buf()))?
        .trim();
    if let Some(rest) = text.strip_prefix("ref: ") {
        let branch_name = rest.trim().to_string();
        let branch = FullName::new(branch_name.clone()).map_err(|e| {
            // Treat malformed HEAD ref content as "not a repo" — clone refuses.
            CloneError::Refs(RefError::Name(e))
        })?;
        match read_oid_for_loose_or_packed(src_gitdir, &branch, hash_kind)? {
            Some(oid) => Ok(SourceHead::Symbolic { branch, oid }),
            None => Ok(SourceHead::Unborn { branch }),
        }
    } else {
        let oid = ObjectId::parse_hex(hash_kind, text)?;
        Ok(SourceHead::Detached(oid))
    }
}

/// Look up `branch` in the source repo: first as a loose ref file, then in
/// `packed-refs`. Returns None if absent in both. We don't follow symbolic
/// chains here — `branch` is already the leaf name HEAD pointed at.
fn read_oid_for_loose_or_packed(
    src_gitdir: &Path,
    branch: &FullName,
    hash_kind: HashKind,
) -> Result<Option<ObjectId>, CloneError> {
    let loose_path = src_gitdir.join(branch.loose_path_relative());
    if let Ok(bytes) = fs::read(&loose_path) {
        let s = std::str::from_utf8(&bytes)
            .map_err(|_| CloneError::NotARepo(src_gitdir.to_path_buf()))?
            .trim();
        // Could itself be symbolic. We only handle one indirection level here —
        // mirroring git's MAXDEPTH=5 would be over-engineering for HEAD chase.
        if let Some(rest) = s.strip_prefix("ref: ") {
            let next = FullName::new(rest.trim().to_string())
                .map_err(|e| CloneError::Refs(RefError::Name(e)))?;
            return read_oid_for_loose_or_packed(src_gitdir, &next, hash_kind);
        }
        let oid = ObjectId::parse_hex(hash_kind, s)?;
        return Ok(Some(oid));
    }
    // Fall back to packed-refs.
    let packed_path = src_gitdir.join("packed-refs");
    if let Ok(text) = fs::read_to_string(&packed_path) {
        for line in text.lines() {
            if line.starts_with('#') || line.starts_with('^') || line.is_empty() {
                continue;
            }
            if let Some((hex, name)) = line.split_once(' ') {
                if name.trim() == branch.as_str() {
                    let oid = ObjectId::parse_hex(hash_kind, hex.trim())?;
                    return Ok(Some(oid));
                }
            }
        }
    }
    Ok(None)
}

/// Read `extensions.objectFormat` from the source's config, defaulting to
/// SHA-1. Functionally identical to `repo::read_object_format`, duplicated
/// here so we can call it before opening the source as a Repository (which we
/// never do — we copy bytes wholesale).
fn read_object_format_from_gitdir(gitdir: &Path) -> Result<HashKind, CloneError> {
    let cfg_path = gitdir.join("config");
    let bytes = match fs::read(&cfg_path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(HashKind::Sha1),
        Err(e) => {
            return Err(CloneError::Io {
                path: cfg_path,
                source: e,
            })
        }
    };
    let text = std::str::from_utf8(&bytes).map_err(|_| CloneError::Io {
        path: cfg_path.clone(),
        source: io::Error::new(io::ErrorKind::InvalidData, "config is not utf-8"),
    })?;
    let mut in_extensions = false;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(stripped) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_extensions = stripped.trim().eq_ignore_ascii_case("extensions");
            continue;
        }
        if !in_extensions {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim().eq_ignore_ascii_case("objectformat") {
                let v = v.trim().trim_matches('"');
                return Ok(HashKind::parse(v)?);
            }
        }
    }
    Ok(HashKind::Sha1)
}

// ---------------------------------------------------------------------------
// Ref propagation
// ---------------------------------------------------------------------------

/// For every `refs/heads/<name>` in the source, write `refs/remotes/origin/<name>`
/// to the destination. When source HEAD is symbolic, also create the local
/// `refs/heads/<branch>` mirroring the source's tip.
fn propagate_refs(
    src_gitdir: &Path,
    dst_repo: &Repository,
    hash_kind: HashKind,
    head: &SourceHead,
) -> Result<(), CloneError> {
    // Collect `refs/heads/<name> -> oid` pairs from the source. We deduplicate
    // names — loose wins over packed (matches git's `CompositeRefStore` order).
    let heads = read_source_heads(src_gitdir, hash_kind)?;

    let mut tx = dst_repo.refs().transaction();
    for (branch_full, oid) in &heads {
        // Strip "refs/heads/" → mirror as "refs/remotes/origin/".
        let short = branch_full
            .as_str()
            .strip_prefix("refs/heads/")
            .expect("branch name starts with refs/heads/");
        let remote_name = format!("refs/remotes/origin/{short}");
        let remote_full = FullName::new(remote_name).map_err(RefError::Name)?;
        tx.update(
            &remote_full,
            ExpectedOldValue::Any,
            NewValue::Direct(*oid),
            ReflogMessage::none(),
        )?;
    }

    // When source HEAD is symbolic, also create the local `refs/heads/<name>`
    // so the destination has a checked-out branch.
    if let SourceHead::Symbolic { branch, oid } = head {
        tx.update(
            branch,
            ExpectedOldValue::Any,
            NewValue::Direct(*oid),
            ReflogMessage::from(format!("clone: from {}", oid)),
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Enumerate every `refs/heads/<name>` in the source: loose first, then any
/// packed entry not already covered by loose.
fn read_source_heads(
    src_gitdir: &Path,
    hash_kind: HashKind,
) -> Result<Vec<(FullName, ObjectId)>, CloneError> {
    let mut out: Vec<(FullName, ObjectId)> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    let heads_dir = src_gitdir.join("refs").join("heads");
    walk_heads(
        &heads_dir,
        &heads_dir,
        hash_kind,
        src_gitdir,
        &mut |name, oid| {
            if !seen.contains(name.as_str()) {
                seen.insert(name.as_str().to_string());
                out.push((name, oid));
            }
        },
    )?;

    // packed-refs (skip branches loose already covers).
    let packed_path = src_gitdir.join("packed-refs");
    if let Ok(text) = fs::read_to_string(&packed_path) {
        for line in text.lines() {
            if line.starts_with('#') || line.starts_with('^') || line.is_empty() {
                continue;
            }
            let (hex, name) = match line.split_once(' ') {
                Some(p) => p,
                None => continue,
            };
            let name = name.trim();
            if !name.starts_with("refs/heads/") {
                continue;
            }
            if seen.contains(name) {
                continue;
            }
            let full = FullName::new(name.to_string()).map_err(RefError::Name)?;
            let oid = ObjectId::parse_hex(hash_kind, hex.trim())?;
            seen.insert(name.to_string());
            out.push((full, oid));
        }
    }

    Ok(out)
}

fn walk_heads(
    root: &Path,
    cur: &Path,
    hash_kind: HashKind,
    src_gitdir: &Path,
    sink: &mut dyn FnMut(FullName, ObjectId),
) -> Result<(), CloneError> {
    let entries = match fs::read_dir(cur) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(CloneError::Io {
                path: cur.to_path_buf(),
                source: e,
            })
        }
    };
    for entry in entries {
        let entry = entry.map_err(|e| CloneError::Io {
            path: cur.to_path_buf(),
            source: e,
        })?;
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            walk_heads(root, &path, hash_kind, src_gitdir, sink)?;
        } else if ft.is_file() {
            let rel = path.strip_prefix(src_gitdir).map_err(|_| CloneError::Io {
                path: path.clone(),
                source: io::Error::other("ref path outside gitdir"),
            })?;
            let rel_str = rel
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            let bytes = fs::read(&path).map_err(|e| CloneError::Io {
                path: path.clone(),
                source: e,
            })?;
            let text = match std::str::from_utf8(&bytes) {
                Ok(s) => s.trim(),
                Err(_) => continue, // skip malformed ref files; clone is best-effort here
            };
            if text.starts_with("ref: ") {
                // Symbolic head ref: skip — we're collecting tips, not symrefs.
                continue;
            }
            let oid = ObjectId::parse_hex(hash_kind, text)?;
            let name = match FullName::new(rel_str) {
                Ok(n) => n,
                Err(_) => continue,
            };
            sink(name, oid);
        }
    }
    let _ = root;
    Ok(())
}

// ---------------------------------------------------------------------------
// Destination HEAD writing
// ---------------------------------------------------------------------------

fn write_dst_head(dst_repo: &Repository, head: &SourceHead) -> Result<(), CloneError> {
    let head_name = FullName::new("HEAD").map_err(RefError::Name)?;
    let mut tx = dst_repo.refs().transaction();
    match head {
        SourceHead::Symbolic { branch, .. } | SourceHead::Unborn { branch } => {
            tx.update(
                &head_name,
                ExpectedOldValue::Any,
                NewValue::Symbolic(branch.clone()),
                ReflogMessage::none(),
            )?;
        }
        SourceHead::Detached(oid) => {
            tx.update(
                &head_name,
                ExpectedOldValue::Any,
                NewValue::Direct(*oid),
                ReflogMessage::from(format!("clone: detached at {oid}")),
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tree peeling — lifted from cli::checkout's helper. We don't import it
// because we don't want a CLI <-> backend dependency loop; this is a small
// duplication and it keeps the module self-contained.
// ---------------------------------------------------------------------------

fn peel_to_tree(repo: &Repository, oid: ObjectId) -> Result<ObjectId, CloneError> {
    use crate::commit::Commit;
    use crate::object::ObjectKind;
    let obj = repo.odb().read(&oid).map_err(|e| CloneError::Io {
        path: repo.gitdir().to_path_buf(),
        source: io::Error::other(format!("odb read {oid}: {e}")),
    })?;
    match obj.kind {
        ObjectKind::Tree => Ok(oid),
        ObjectKind::Commit => {
            let c = Commit::parse(&obj.data, repo.hash_kind()).map_err(|e| CloneError::Io {
                path: repo.gitdir().to_path_buf(),
                source: io::Error::new(io::ErrorKind::InvalidData, format!("{e}")),
            })?;
            Ok(c.tree)
        }
        ObjectKind::Tag => {
            let body = std::str::from_utf8(&obj.data).map_err(|_| CloneError::Io {
                path: repo.gitdir().to_path_buf(),
                source: io::Error::new(io::ErrorKind::InvalidData, "non-utf8 tag"),
            })?;
            for line in body.lines() {
                if let Some(rest) = line.strip_prefix("object ") {
                    let next = ObjectId::parse_hex(repo.hash_kind(), rest.trim())?;
                    return peel_to_tree(repo, next);
                }
                if line.is_empty() {
                    break;
                }
            }
            Err(CloneError::Io {
                path: repo.gitdir().to_path_buf(),
                source: io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("tag {oid} missing object line"),
                ),
            })
        }
        other => Err(CloneError::Io {
            path: repo.gitdir().to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{oid} is a {other}, not commit-ish"),
            ),
        }),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn has_system_git() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Create a non-bare source repo with a couple of commits using system git.
    /// Returns the repo path (workdir) and a snapshot of its HEAD branch name.
    fn make_source_with_commits(dir: &Path) -> (PathBuf, String) {
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        run_git(&src, &["init", "-q", "-b", "main", "."]);
        run_git(&src, &["config", "user.email", "test@example.com"]);
        run_git(&src, &["config", "user.name", "Test User"]);

        std::fs::write(src.join("README.md"), b"hello\n").unwrap();
        run_git(&src, &["add", "README.md"]);
        run_git(&src, &["commit", "-q", "-m", "first"]);

        std::fs::write(src.join("a.txt"), b"alpha\n").unwrap();
        std::fs::create_dir_all(src.join("dir")).unwrap();
        std::fs::write(src.join("dir").join("b.txt"), b"beta\n").unwrap();
        run_git(&src, &["add", "."]);
        run_git(&src, &["commit", "-q", "-m", "second"]);

        (src, "main".to_string())
    }

    fn make_bare_source(dir: &Path) -> PathBuf {
        // Make a non-bare seed first, then clone to bare.
        let (work, _branch) = make_source_with_commits(dir);
        let bare = dir.join("bare.git");
        run_git(
            dir,
            &[
                "clone",
                "-q",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        bare
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed in {}: stderr={}",
            cwd.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Snapshot every regular file under `<gitdir>/objects/` *except* the
    /// fast-changing `info/` directory. The result is a sorted list of
    /// `(rel_path, contents_hash)` so two object stores can be compared.
    fn snapshot_objects(gitdir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        let root = gitdir.join("objects");
        snapshot_walk(&root, &root, &mut out);
        // Filter info/ — gc bookkeeping that's allowed to differ.
        out.retain(|(p, _)| !p.starts_with("info/"));
        // Filter pack/.rev — pack reverse index that's optional.
        out.retain(|(p, _)| !p.ends_with(".rev"));
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn snapshot_walk(root: &Path, cur: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let entries = match std::fs::read_dir(cur) {
            Ok(e) => e,
            Err(_) => return,
        };
        for ent in entries.flatten() {
            let path = ent.path();
            let ft = match ent.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                snapshot_walk(root, &path, out);
            } else if ft.is_file() {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                let bytes = std::fs::read(&path).unwrap_or_default();
                out.push((rel, bytes));
            }
        }
    }

    // --- 1. Round-trip a non-bare source --------------------------------

    #[test]
    fn round_trip_non_bare_source() {
        if !has_system_git() {
            eprintln!("skipping: system git not on PATH");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let (src, branch) = make_source_with_commits(tmp.path());
        let dst = tmp.path().join("dst");

        clone_local(&src, &dst, &CloneOpts::default()).unwrap();

        // 1a. Object stores match.
        let src_objs = snapshot_objects(&src.join(".git"));
        let dst_objs = snapshot_objects(&dst.join(".git"));
        assert_eq!(
            src_objs,
            dst_objs,
            "object store contents diverge between {} and {}",
            src.display(),
            dst.display()
        );

        // 1b. HEAD points at the same branch and the same oid.
        let src_head = std::fs::read_to_string(src.join(".git").join("HEAD")).unwrap();
        let dst_head = std::fs::read_to_string(dst.join(".git").join("HEAD")).unwrap();
        assert_eq!(src_head.trim(), dst_head.trim());

        // 1c. refs/heads/<branch> matches.
        let src_branch =
            std::fs::read_to_string(src.join(".git").join("refs/heads").join(&branch)).unwrap();
        let dst_branch =
            std::fs::read_to_string(dst.join(".git").join("refs/heads").join(&branch)).unwrap();
        assert_eq!(src_branch.trim(), dst_branch.trim());

        // 1d. refs/remotes/origin/<branch> exists with the same oid.
        let dst_remote =
            std::fs::read_to_string(dst.join(".git").join("refs/remotes/origin").join(&branch))
                .unwrap();
        assert_eq!(src_branch.trim(), dst_remote.trim());

        // 1e. Working tree was materialized.
        assert_eq!(std::fs::read(dst.join("README.md")).unwrap(), b"hello\n");
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"alpha\n");
        assert_eq!(
            std::fs::read(dst.join("dir").join("b.txt")).unwrap(),
            b"beta\n"
        );
    }

    // --- 2. Source-from-bare ---------------------------------------------

    #[test]
    fn round_trip_bare_source() {
        if !has_system_git() {
            eprintln!("skipping: system git not on PATH");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let bare = make_bare_source(tmp.path());
        let dst = tmp.path().join("dst");

        clone_local(&bare, &dst, &CloneOpts::default()).unwrap();

        // The bare repo IS the gitdir; objects should still copy.
        let src_objs = snapshot_objects(&bare);
        let dst_objs = snapshot_objects(&dst.join(".git"));
        assert_eq!(src_objs, dst_objs);

        // HEAD comes through.
        let src_head = std::fs::read_to_string(bare.join("HEAD")).unwrap();
        let dst_head = std::fs::read_to_string(dst.join(".git").join("HEAD")).unwrap();
        assert_eq!(src_head.trim(), dst_head.trim());

        // Working tree is materialized.
        assert!(dst.join("README.md").exists());
    }

    // --- 3. Refuses non-empty dst ----------------------------------------

    #[test]
    fn refuses_non_empty_dst() {
        if !has_system_git() {
            eprintln!("skipping: system git not on PATH");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let (src, _branch) = make_source_with_commits(tmp.path());
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join("preexisting.txt"), b"hi").unwrap();

        let err = clone_local(&src, &dst, &CloneOpts::default()).unwrap_err();
        match err {
            CloneError::DestNotEmpty(_) => {}
            other => panic!("expected DestNotEmpty, got {other:?}"),
        }
        // The pre-existing file is untouched.
        assert_eq!(std::fs::read(dst.join("preexisting.txt")).unwrap(), b"hi");
    }

    // --- 4. file:// URL -------------------------------------------------

    #[test]
    fn accepts_file_url() {
        if !has_system_git() {
            eprintln!("skipping: system git not on PATH");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let (src, _branch) = make_source_with_commits(tmp.path());
        let url = format!("file://{}", src.display());
        let dst = tmp.path().join("dst-from-url");

        clone_local(Path::new(&url), &dst, &CloneOpts::default()).unwrap();

        let src_objs = snapshot_objects(&src.join(".git"));
        let dst_objs = snapshot_objects(&dst.join(".git"));
        assert_eq!(src_objs, dst_objs);
    }

    // --- 5. --no-checkout: refs + objects, no working tree ---------------

    #[test]
    fn no_checkout_skips_workdir() {
        if !has_system_git() {
            eprintln!("skipping: system git not on PATH");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let (src, _branch) = make_source_with_commits(tmp.path());
        let dst = tmp.path().join("dst");

        let opts = CloneOpts {
            quiet: true,
            no_checkout: true,
        };
        clone_local(&src, &dst, &opts).unwrap();

        // .git is there.
        assert!(dst.join(".git").is_dir());
        // Refs + objects propagated.
        assert!(dst.join(".git").join("refs/remotes/origin").exists());
        let dst_objs = snapshot_objects(&dst.join(".git"));
        assert!(!dst_objs.is_empty());
        // But the working tree files don't exist.
        assert!(!dst.join("README.md").exists());
        assert!(!dst.join("a.txt").exists());
    }

    // --- 6. fsck passes --------------------------------------------------

    #[test]
    fn cloned_repo_passes_fsck() {
        if !has_system_git() {
            eprintln!("skipping: system git not on PATH");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let (src, _branch) = make_source_with_commits(tmp.path());
        let dst = tmp.path().join("dst");

        clone_local(&src, &dst, &CloneOpts::default()).unwrap();

        let out = Command::new("git")
            .args(["fsck", "--full"])
            .current_dir(&dst)
            .output()
            .expect("spawn git fsck");
        assert!(
            out.status.success(),
            "git fsck --full failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // --- 7. git log matches between src and dst --------------------------

    #[test]
    fn git_log_matches_source() {
        if !has_system_git() {
            eprintln!("skipping: system git not on PATH");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let (src, _branch) = make_source_with_commits(tmp.path());
        let dst = tmp.path().join("dst");

        clone_local(&src, &dst, &CloneOpts::default()).unwrap();

        let src_log = Command::new("git")
            .args(["log", "--oneline", "--all"])
            .current_dir(&src)
            .output()
            .unwrap();
        let dst_log = Command::new("git")
            .args(["log", "--oneline"])
            .current_dir(&dst)
            .output()
            .unwrap();
        assert!(src_log.status.success() && dst_log.status.success());
        // dst log should be a (suffix or equal) of src log — modulo the fact
        // that src may have extra refs only listed under --all in src.
        // For our flat fixture this is just a direct equality check.
        assert_eq!(src_log.stdout, dst_log.stdout);
    }
}
