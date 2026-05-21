//! Repository discovery and the `Repository` handle.
//!
//! In M0 this is intentionally minimal: discover the `.git` directory, parse
//! `core.repositoryformatversion` and `extensions.objectFormat` from
//! `.git/config`, and expose path helpers. M1 adds the `ObjectDb`; M2 adds
//! the `RefStore`. Until then porcelain commands open files directly.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;

use crate::hash::{new_hasher, HashError, HashKind, Hasher, ObjectId};
use crate::odb::{LooseStore, ObjectDb, ObjectStore};
use crate::pack::PackStore;
use crate::refs::{CompositeRefStore, LooseRefStore, PackedRefStore, RefStore, ReftableStore};

/// A handle to an on-disk git repository.
///
/// **Single-worktree layout** (the common case): `gitdir` is the `.git`
/// directory, `commondir` == `gitdir`, `workdir` is `.git`'s parent.
///
/// **Linked-worktree layout** (NON_GOALS Batch I): the worktree's `.git` is a
/// file containing `gitdir: <path>` pointing at a per-worktree state
/// directory (e.g. `<main>/.git/worktrees/<name>/`). That directory holds
/// per-worktree state (HEAD, index, logs/HEAD reflog) and a `commondir`
/// file pointing back at the main `.git/`, which holds the SHARED state
/// (objects, refs, config, hooks, packed-refs, info, shallow). The
/// `commondir` field on `Repository` resolves to the shared path; the
/// `gitdir` field to the per-worktree one. For single-worktree repos the
/// two are equal.
pub struct Repository {
    gitdir: PathBuf,
    /// The "common" gitdir holding shared state (objects/refs/config). Same
    /// as `gitdir` for a single-worktree (non-linked) repo.
    commondir: PathBuf,
    workdir: PathBuf,
    hash_kind: HashKind,
    odb: ObjectDb,
    refs: Arc<dyn RefStore>,
    /// Set of commits at the shallow-clone boundary — commits whose `parent`
    /// lines reference oids NOT present in the odb. Read once at open time
    /// from `<commondir>/shallow`. Empty for non-shallow repos. Used by
    /// `log` and `revparse` to gracefully stop walking past the boundary.
    shallow_boundary: HashSet<ObjectId>,
}

impl Repository {
    /// Walk up from `start` looking for a `.git` directory OR a `.git` file
    /// (the linked-worktree pointer form). Returns the first match. Errors
    /// if no parent contains one.
    pub fn discover(start: impl AsRef<Path>) -> Result<Self, RepoError> {
        let start = start.as_ref().canonicalize().map_err(|e| {
            RepoError::Io(format!("canonicalize {}: {e}", start.as_ref().display()))
        })?;
        let mut cur: &Path = &start;
        loop {
            let candidate = cur.join(".git");
            // Directory form — standard layout.
            if candidate.is_dir() {
                return Self::open(candidate);
            }
            // File form — linked worktree. Read `gitdir: <path>\n` and open
            // that as the per-worktree gitdir.
            if candidate.is_file() {
                let pointer = resolve_gitdir_pointer(&candidate)?;
                return Self::open(pointer);
            }
            match cur.parent() {
                Some(p) => cur = p,
                None => return Err(RepoError::NotARepo(start)),
            }
        }
    }

    /// Discover starting at the current working directory.
    ///
    /// Honors three upstream-git environment variables before walking parents:
    /// - `GIT_DIR` — if set, short-circuit to `Repository::open($GIT_DIR)` and
    ///   skip the parent-dir walk entirely. Wrapper scripts and IDEs that set
    ///   this expect the operation to target that exact gitdir regardless of
    ///   the process's `$cwd`.
    /// - `GIT_WORK_TREE` and `GIT_INDEX_FILE` are handled in [`Repository::open`]
    ///   and [`Repository::index_path`] respectively (see those docstrings).
    pub fn discover_from_cwd() -> Result<Self, RepoError> {
        if let Some(gitdir) = std::env::var_os("GIT_DIR") {
            let path = PathBuf::from(gitdir);
            crate::trace!("repo", "GIT_DIR={}", path.display());
            // Don't silently accept a non-existent GIT_DIR — upstream git
            // errors with "fatal: not a git repository: '<path>'" in this
            // case and downstream commands would otherwise return spurious
            // "no commits yet" / empty-repo output.
            if !path.exists() {
                return Err(RepoError::NotARepo(path));
            }
            return Self::open(path);
        }
        let cwd =
            std::env::current_dir().map_err(|e| RepoError::Io(format!("current_dir: {e}")))?;
        crate::trace!("repo", "discover from {}", cwd.display());
        Self::discover(cwd)
    }

    /// Open a `Repository` given an exact path to the `.git` directory (or
    /// per-worktree gitdir, for a linked worktree).
    ///
    /// If the `GIT_WORK_TREE` environment variable is set, it overrides the
    /// workdir we'd otherwise derive from `gitdir`'s parent (or the back-pointer
    /// file for linked worktrees). This matches upstream git's behavior where
    /// wrapper scripts can decouple the gitdir from the working tree.
    pub fn open(gitdir: PathBuf) -> Result<Self, RepoError> {
        // Resolve commondir: if `<gitdir>/commondir` exists (linked-worktree
        // marker), it points at the shared `.git/`. Otherwise commondir ==
        // gitdir for a normal single-worktree repo.
        let commondir = resolve_commondir(&gitdir)?;

        // Workdir: for a linked worktree, the per-worktree gitdir's parent
        // is `<main-git>/worktrees/<name>` — NOT the actual worktree. We
        // need to read `<gitdir>/gitdir` (a back-pointer file written by
        // `worktree add`) to find the real workdir. Falls back to gitdir's
        // parent for the single-worktree case.
        //
        // `GIT_WORK_TREE` overrides whatever we'd compute (per upstream git).
        // Canonicalize the env-supplied path to match the same shape as the
        // discovered workdir path — on macOS `/var/...` resolves to
        // `/private/var/...` via a stable symlink, and `abs.strip_prefix(workdir)`
        // in callers like `cli::add::stage_one` relies on both sides agreeing.
        let workdir = if let Some(p) = std::env::var_os("GIT_WORK_TREE") {
            let raw = PathBuf::from(p);
            raw.canonicalize().unwrap_or(raw)
        } else {
            resolve_workdir(&gitdir)?
        };

        let hash_kind = read_object_format(&commondir)?;
        let ref_format = read_ref_storage_format(&commondir)?;

        // Stack the object stores. The loose+pack stores point at COMMONDIR's
        // objects/ — that's where all the shared object data lives.
        let mut stores: Vec<Arc<dyn ObjectStore>> = Vec::new();
        stores.push(Arc::new(LooseStore::new(
            commondir.join("objects"),
            hash_kind,
        )));
        for pack_path in discover_pack_files(&commondir.join("objects").join("pack")) {
            match PackStore::open_pair(&pack_path, hash_kind) {
                Ok(ps) => stores.push(Arc::new(ps)),
                Err(e) => {
                    eprintln!(
                        "rustygit: skipping unreadable pack {}: {e}",
                        pack_path.display()
                    );
                }
            }
        }
        let odb = ObjectDb::new(stores, 0, hash_kind);

        // RefStore: loose refs live under `<commondir>/refs/` and HEAD-style
        // pseudo-refs live under the per-worktree `<gitdir>`. We hand the
        // loose store the COMMONDIR path so refs/heads/* etc. work; HEAD
        // resolution itself is per-worktree and that's wired through
        // `head_path()` separately. This is good enough for the v1 of
        // linked-worktree support; sophisticated per-worktree refs
        // (`refs/worktree/<wt-name>/`) are deferred.
        let refs: Arc<dyn RefStore> = match ref_format {
            RefStorageFormat::Files => {
                let loose_refs = Arc::new(LooseRefStore::new(commondir.clone(), hash_kind));
                let packed_refs = Arc::new(PackedRefStore::new(
                    commondir.join("packed-refs"),
                    hash_kind,
                ));
                Arc::new(CompositeRefStore::new(loose_refs, packed_refs))
            }
            RefStorageFormat::Reftable => {
                Arc::new(ReftableStore::open(commondir.join("reftable"), hash_kind)?)
            }
        };

        let shallow_boundary = read_shallow_file(&commondir, hash_kind);

        Ok(Self {
            gitdir,
            commondir,
            workdir,
            hash_kind,
            odb,
            refs,
            shallow_boundary,
        })
    }

    /// True if `oid` is a commit at the shallow-clone boundary — its parents
    /// (per its commit object) are NOT present in the odb. Callers walking
    /// the commit DAG should stop here instead of erroring on the missing
    /// parent.
    pub fn is_shallow_boundary(&self, oid: &ObjectId) -> bool {
        self.shallow_boundary.contains(oid)
    }

    pub fn odb(&self) -> &ObjectDb {
        &self.odb
    }

    pub fn refs(&self) -> &dyn RefStore {
        self.refs.as_ref()
    }

    pub fn gitdir(&self) -> &Path {
        &self.gitdir
    }

    /// The "common" gitdir holding shared state — same as [`gitdir`] for
    /// non-linked-worktree repos.
    pub fn commondir(&self) -> &Path {
        &self.commondir
    }

    /// True iff this repository is opened as a linked (secondary) worktree.
    pub fn is_linked_worktree(&self) -> bool {
        self.gitdir != self.commondir
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    pub fn hash_kind(&self) -> HashKind {
        self.hash_kind
    }

    /// Construct a fresh hasher matching the repository's algorithm.
    pub fn new_hasher(&self) -> Box<dyn Hasher> {
        new_hasher(self.hash_kind)
    }

    /// Shared object database. Lives under [`commondir`], not [`gitdir`].
    pub fn objects_dir(&self) -> PathBuf {
        self.commondir.join("objects")
    }

    /// Shared refs root. Lives under [`commondir`].
    pub fn refs_dir(&self) -> PathBuf {
        self.commondir.join("refs")
    }

    /// HEAD is per-worktree — lives in [`gitdir`].
    pub fn head_path(&self) -> PathBuf {
        self.gitdir.join("HEAD")
    }

    /// Index is per-worktree — lives in [`gitdir`].
    ///
    /// If the `GIT_INDEX_FILE` environment variable is set, that path is used
    /// instead. This matches upstream git's behavior where porcelain commands
    /// can be redirected to an alternate index (e.g. `git read-tree --index-output`
    /// and the `GIT_INDEX_FILE` wrapper convention).
    pub fn index_path(&self) -> PathBuf {
        if let Some(p) = std::env::var_os("GIT_INDEX_FILE") {
            PathBuf::from(p)
        } else {
            self.gitdir.join("index")
        }
    }

    /// Shared config. Lives under [`commondir`].
    pub fn config_path(&self) -> PathBuf {
        self.commondir.join("config")
    }

    /// `core.symlinks`: when false, symlink-mode index entries are checked
    /// out as regular files whose content is the link target. Defaults to
    /// `true` on Unix and `false` on Windows, matching upstream git.
    ///
    /// Reads the layered config on every call (rare path — only invoked
    /// during checkout). Returns the default on parse/IO error.
    pub fn core_symlinks(&self) -> bool {
        let default = cfg!(unix);
        match crate::config::Config::from_repo_dir(&self.commondir) {
            Ok(cfg) => cfg.get_bool("core", "symlinks").unwrap_or(default),
            Err(_) => default,
        }
    }

    /// `core.autocrlf`: returns `Some(AutoCrlf)` if the key is set,
    /// `None` if unset. Recognized spellings: `true`, `false`, `input`.
    ///
    /// Unset (None) means "no conversion" — matches upstream git's behavior
    /// when neither `core.autocrlf` nor a `.gitattributes` driver is in
    /// effect. Note: `.gitattributes`-based `text=auto` is NOT yet honored.
    pub fn core_autocrlf(&self) -> Option<crate::config::AutoCrlf> {
        let cfg = crate::config::Config::from_repo_dir(&self.commondir).ok()?;
        let raw = cfg.get_string("core", "autocrlf")?;
        crate::config::AutoCrlf::parse(raw)
    }
}

/// Read a `.git` file (the linked-worktree pointer form) and return the
/// absolute path to the per-worktree gitdir it names. Format:
///   `gitdir: <path>\n`  (path may be relative to the `.git` file's dir).
fn resolve_gitdir_pointer(dot_git_file: &Path) -> Result<PathBuf, RepoError> {
    let bytes = fs::read(dot_git_file)
        .map_err(|e| RepoError::Io(format!("read {}: {e}", dot_git_file.display())))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| RepoError::CorruptConfig(".git pointer file is not valid UTF-8".into()))?;
    let line = text.lines().next().unwrap_or("");
    let rest = line.strip_prefix("gitdir:").ok_or_else(|| {
        RepoError::CorruptConfig(format!(
            ".git file at {} missing 'gitdir:' prefix",
            dot_git_file.display()
        ))
    })?;
    let raw_path = rest.trim();
    let pointed = Path::new(raw_path);
    let absolute = if pointed.is_absolute() {
        pointed.to_path_buf()
    } else {
        dot_git_file
            .parent()
            .ok_or_else(|| RepoError::NotARepo(dot_git_file.to_path_buf()))?
            .join(pointed)
    };
    // Canonicalize to collapse `..` segments; many `.git` files use
    // relative paths with `..` to point back into the main `.git/worktrees`.
    absolute
        .canonicalize()
        .map_err(|e| RepoError::Io(format!("canonicalize gitdir pointer {raw_path:?}: {e}")))
}

/// If `<gitdir>/commondir` exists, read it (relative or absolute path) and
/// return the resolved common gitdir. Otherwise return `gitdir` itself.
fn resolve_commondir(gitdir: &Path) -> Result<PathBuf, RepoError> {
    let marker = gitdir.join("commondir");
    let bytes = match fs::read(&marker) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(gitdir.to_path_buf());
        }
        Err(e) => return Err(RepoError::Io(format!("read {}: {e}", marker.display()))),
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| RepoError::CorruptConfig("commondir is not valid UTF-8".into()))?;
    let raw = text.lines().next().unwrap_or("").trim();
    if raw.is_empty() {
        return Err(RepoError::CorruptConfig("commondir is empty".into()));
    }
    let pointed = Path::new(raw);
    let absolute = if pointed.is_absolute() {
        pointed.to_path_buf()
    } else {
        gitdir.join(pointed)
    };
    absolute
        .canonicalize()
        .map_err(|e| RepoError::Io(format!("canonicalize commondir {raw:?}: {e}")))
}

/// Find the workdir associated with `gitdir`. For the main worktree it's
/// just `gitdir`'s parent. For a linked worktree, the per-worktree gitdir
/// contains a `gitdir` file (NOT a `.git` file — confusingly named) that
/// names the working-tree's `.git` pointer file; the workdir is its parent.
fn resolve_workdir(gitdir: &Path) -> Result<PathBuf, RepoError> {
    let backptr = gitdir.join("gitdir");
    if let Ok(bytes) = fs::read(&backptr) {
        if let Ok(text) = std::str::from_utf8(&bytes) {
            let line = text.lines().next().unwrap_or("").trim();
            if !line.is_empty() {
                let p = Path::new(line);
                if let Some(parent) = p.parent() {
                    if parent.exists() {
                        return parent
                            .canonicalize()
                            .map_err(|e| RepoError::Io(format!("canonicalize workdir: {e}")));
                    }
                }
            }
        }
    }
    // No back-pointer file (main worktree) — fall back to gitdir's parent.
    gitdir
        .parent()
        .ok_or_else(|| RepoError::NotARepo(gitdir.to_path_buf()))
        .map(Path::to_path_buf)
}

/// Which ref-storage backend the repo uses. Selected by
/// `extensions.refStorage` in `.git/config`. Files is the implicit default and
/// what every git repo ships with unless `git init --ref-format=reftable` was
/// passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefStorageFormat {
    Files,
    Reftable,
}

/// Read `extensions.refStorage` from `.git/config`. Returns `Files` if absent
/// or unrecognized. Mirrors the structure of `read_object_format` so the two
/// INI scans stay symmetric.
fn read_ref_storage_format(gitdir: &Path) -> Result<RefStorageFormat, RepoError> {
    let cfg_path = gitdir.join("config");
    let bytes = match fs::read(&cfg_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(RefStorageFormat::Files),
        Err(e) => return Err(RepoError::Io(format!("read {}: {e}", cfg_path.display()))),
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| RepoError::CorruptConfig("config is not valid UTF-8".into()))?;
    let mut in_extensions = false;
    for raw_line in text.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(section) = strip_section_header(line) {
            in_extensions = section.eq_ignore_ascii_case("extensions");
            continue;
        }
        if !in_extensions {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            let v = v.trim().trim_matches('"');
            if k.eq_ignore_ascii_case("refstorage") {
                return match v {
                    "files" => Ok(RefStorageFormat::Files),
                    "reftable" => Ok(RefStorageFormat::Reftable),
                    other => Err(RepoError::CorruptConfig(format!(
                        "unknown extensions.refStorage = {other}"
                    ))),
                };
            }
        }
    }
    Ok(RefStorageFormat::Files)
}

/// Read `extensions.objectFormat` from `.git/config`. Returns `Sha1` if the key
/// is absent. We don't yet have a real config parser (M0 is too early to land
/// it); parse just enough INI to find this single key. Anything we don't
/// understand is treated as "no extension set" → SHA-1.
fn read_object_format(gitdir: &Path) -> Result<HashKind, RepoError> {
    let cfg_path = gitdir.join("config");
    let bytes = match fs::read(&cfg_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HashKind::Sha1);
        }
        Err(e) => return Err(RepoError::Io(format!("read {}: {e}", cfg_path.display()))),
    };
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| RepoError::CorruptConfig("config is not valid UTF-8".into()))?;
    let mut in_extensions = false;
    for raw_line in text.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(section) = strip_section_header(line) {
            in_extensions = section.eq_ignore_ascii_case("extensions");
            continue;
        }
        if !in_extensions {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            let v = v.trim().trim_matches('"');
            if k.eq_ignore_ascii_case("objectformat") {
                return HashKind::parse(v).map_err(RepoError::Hash);
            }
        }
    }
    Ok(HashKind::Sha1)
}

/// Parse `.git/shallow` if present. Missing or malformed file → empty set
/// (we never reject a repo just because the shallow file is unreadable).
fn read_shallow_file(gitdir: &Path, hash_kind: HashKind) -> HashSet<ObjectId> {
    let path = gitdir.join("shallow");
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(_) => return HashSet::new(),
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(t) => t,
        Err(_) => return HashSet::new(),
    };
    let mut out = HashSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Ok(oid) = ObjectId::parse_hex(hash_kind, line) {
            out.insert(oid);
        }
    }
    out
}

fn strip_section_header(line: &str) -> Option<&str> {
    let line = line.strip_prefix('[')?.strip_suffix(']')?;
    Some(line.trim())
}

/// List `.pack` files under `objects/pack/`. Returns paths sorted by name so
/// the read-cascade order is deterministic across runs (matches git's
/// alphabetical mtime fallback for stable reproducibility in tests).
fn discover_pack_files(pack_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(pack_dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("pack") {
            out.push(path);
        }
    }
    out.sort();
    out
}

#[derive(Error, Debug)]
pub enum RepoError {
    #[error("not a rustygit repository (or any parent directory): {0}")]
    NotARepo(PathBuf),
    #[error("io: {0}")]
    Io(String),
    #[error("corrupt config: {0}")]
    CorruptConfig(String),
    #[error(transparent)]
    Hash(#[from] HashError),
    #[error("refs: {0}")]
    Refs(#[from] crate::refs::RefError),
}
