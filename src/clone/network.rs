//! Network clone (M10) — orchestrates `clone <https-url>`:
//!
//! 1. Validate / create the destination, mirror `clone_local`'s layout init.
//! 2. Open an [`HttpConnection`], discover capabilities, parse a
//!    [`CapabilityAdvertisement`].
//! 3. Issue `ls-refs` for `HEAD` / `refs/heads/` / `refs/tags/`.
//! 4. Build a `want` list from every advertised oid (de-duplicated), call
//!    `fetch`.
//! 5. Write the received pack bytes to a temp file under
//!    `<dst>/.git/objects/pack/`, then RE-PACK them with
//!    [`crate::pack::build::write_pack_from_objects`] so we land a
//!    deterministic `.pack` + companion `.idx` that the destination repo can
//!    use through its `PackStore`. The temp `.pack` is removed on success.
//! 6. Mirror every `refs/heads/<branch>` into
//!    `refs/remotes/origin/<branch>`. Bind `HEAD`'s symref target to the
//!    matching local `refs/heads/<branch>`, with that branch's loose ref
//!    written too.
//! 7. Optionally check the HEAD tree out into the workdir.
//!
//! This module is intentionally a sibling of `clone_local` rather than a
//! refactor of it. The shape is similar, but the ordering of "have refs first,
//! then re-init repo+odb, then materialize" matters for network: we don't know
//! the hash algorithm until the server advertises it.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::clone::local::CloneError;
use crate::hash::{HashError, HashKind, ObjectId};
use crate::object::ObjectKind;
use crate::pack::{self, PackBuildError, PackEntryKind, PackError, PackFile, RawPackEntry};
use crate::refs::{ExpectedOldValue, FullName, NewValue, RefError, ReflogMessage};
use crate::repo::{RepoError, Repository};
use crate::transport::protocol_v2::{self, AdvertisedRef, CapabilityAdvertisement, ProtocolError};
use crate::transport::{Connection, TransportError};
use crate::unpack_trees::{self, UnpackError, UnpackOpts};

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct NetworkCloneOpts {
    pub quiet: bool,
    pub no_checkout: bool,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum NetworkCloneError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Pack(#[from] PackError),
    #[error(transparent)]
    PackBuild(#[from] PackBuildError),
    #[error(transparent)]
    Repo(#[from] RepoError),
    #[error(transparent)]
    Refs(#[from] RefError),
    #[error(transparent)]
    Unpack(#[from] UnpackError),
    #[error(transparent)]
    Hash(#[from] HashError),
    #[error("io on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("destination exists and is not empty: {0}")]
    DestNotEmpty(PathBuf),
    #[error("server returned an empty ref advertisement")]
    NoRefs,
    #[error("server advertised capability v2 but does not support ls-refs")]
    NoLsRefs,
    #[error("server advertised capability v2 but does not support fetch")]
    NoFetch,
}

// `CloneError` is reused below for ref-name validation — drag it onto our
// error tree so we don't have to re-declare every variant.
impl From<CloneError> for NetworkCloneError {
    fn from(e: CloneError) -> Self {
        // `CloneError` is rich; map the few variants we actually generate
        // through reuse onto our error tree.
        match e {
            CloneError::DestNotEmpty(p) => NetworkCloneError::DestNotEmpty(p),
            CloneError::Io { path, source } => NetworkCloneError::Io { path, source },
            CloneError::Refs(r) => NetworkCloneError::Refs(r),
            CloneError::Repo(r) => NetworkCloneError::Repo(r),
            CloneError::Unpack(u) => NetworkCloneError::Unpack(u),
            CloneError::NotARepo(p) => NetworkCloneError::Io {
                path: p,
                source: io::Error::other("not a repo"),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Clone the repository at `url` into `dst`. URL must be HTTPS for M10.
///
/// On any failure after we begin writing, the destination is cleaned up
/// (the partially-written `.git` is removed, and if we created `dst` itself
/// we remove that too).
pub fn clone_network(
    url: &str,
    dst: &Path,
    opts: &NetworkCloneOpts,
) -> Result<(), NetworkCloneError> {
    // 1. Validate dst.
    let dst_existed = dst.exists();
    if dst_existed {
        if !dst.is_dir() {
            return Err(NetworkCloneError::DestNotEmpty(dst.to_path_buf()));
        }
        if dir_is_nonempty(dst)? {
            return Err(NetworkCloneError::DestNotEmpty(dst.to_path_buf()));
        }
    } else {
        fs::create_dir_all(dst).map_err(|e| NetworkCloneError::Io {
            path: dst.to_path_buf(),
            source: e,
        })?;
    }

    if !opts.quiet {
        println!("Cloning into '{}'...", dst.display());
        let _ = io::stdout().flush();
    }

    match clone_inner(url, dst, opts) {
        Ok(()) => {
            if !opts.quiet {
                println!("done.");
            }
            Ok(())
        }
        Err(e) => {
            // Best-effort cleanup; don't surface secondary errors.
            if dst_existed {
                let _ = fs::remove_dir_all(dst.join(".git"));
            } else {
                let _ = fs::remove_dir_all(dst);
            }
            Err(e)
        }
    }
}

fn clone_inner(url: &str, dst: &Path, opts: &NetworkCloneOpts) -> Result<(), NetworkCloneError> {
    let gitdir = dst.join(".git");

    // Honor the user's `[url "<base>"] insteadOf` rewrites by loading the
    // ambient layered config (system + XDG + global). We can't yet read a
    // *local* config for the target repo because the gitdir doesn't exist
    // — and that's fine: nobody configures insteadOf in a repo-local file
    // they don't yet have. Use the parent of `dst` as the gitdir argument
    // so the loader doesn't accidentally try to read `dst/config`.
    let cfg_dir = dst.parent().unwrap_or(dst);
    let cfg = crate::config::Config::from_repo_dir(cfg_dir).unwrap_or_default();

    // 2. Open connection, discover capabilities.
    let mut conn = crate::transport::connect_upload_pack_with_config(url, &cfg)?;
    let cap_pkts = conn.discover_capabilities()?;
    let cap = CapabilityAdvertisement::parse(&cap_pkts)?;
    if !cap.supports("ls-refs") {
        return Err(NetworkCloneError::NoLsRefs);
    }
    if !cap.supports("fetch") {
        return Err(NetworkCloneError::NoFetch);
    }
    let hash_kind = cap.object_format;

    // 3. Init the destination layout — we now know the hash kind.
    create_layout(&gitdir)?;
    write_config(&gitdir, hash_kind)?;
    write_description(&gitdir)?;
    write_info_exclude(&gitdir)?;

    // 4. ls-refs to learn what's available.
    let advertised =
        protocol_v2::ls_refs(&mut conn, &["HEAD", "refs/heads/", "refs/tags/"], hash_kind)?;
    if advertised.is_empty() {
        return Err(NetworkCloneError::NoRefs);
    }

    // 5. Build wants — every tip's oid, de-duplicated.
    let wants: Vec<ObjectId> = {
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for r in &advertised {
            if seen.insert(r.oid) {
                out.push(r.oid);
            }
            // For annotated tags, the peel target is implicitly reachable
            // through the tag object the server already gave us, so we don't
            // need a separate `want`.
        }
        out
    };

    // 6. fetch.
    let fetch_result = protocol_v2::fetch(&mut conn, &wants, hash_kind)?;

    // 7. Land the pack — write to temp, then re-pack deterministically.
    write_pack_into_repo(&gitdir, &fetch_result.pack_bytes, hash_kind)?;

    // 8. Open destination repo now that objects/ has content.
    let dst_repo = Repository::open(gitdir.clone())?;

    // 9. Write refs.
    write_refs(&dst_repo, &advertised, hash_kind)?;

    // 10. Optional checkout.
    if !opts.no_checkout {
        if let Some(head_oid) = head_oid_from_advertised(&advertised) {
            let tree_oid = peel_to_tree(&dst_repo, head_oid)?;
            let unpack_opts = UnpackOpts {
                update_workdir: true,
                update_index: true,
                force: false,
                keep_extra: false,
            };
            unpack_trees::checkout_tree(&dst_repo, tree_oid, &unpack_opts)?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Pack landing
// ---------------------------------------------------------------------------

/// Write `pack_bytes` into a temp `.pack` under `<gitdir>/objects/pack/`, open
/// it with our [`PackFile`] reader to verify, then re-pack every entry through
/// [`write_pack_from_objects`] to produce a deterministic `pack-<sha>.pack` +
/// companion `.idx`. The temp file is unlinked on the way out.
///
/// We use this two-step shape for one practical reason: the server's pack
/// arrives without an idx, and `git index-pack` is a separate concern. The
/// existing `write_pack_from_objects` already produces the (pack, idx) pair
/// from object triples, so explosion-then-repack is the shortest path that
/// passes `git fsck` on the destination.
fn write_pack_into_repo(
    gitdir: &Path,
    pack_bytes: &[u8],
    hash_kind: HashKind,
) -> Result<(), NetworkCloneError> {
    let pack_dir = gitdir.join("objects").join("pack");
    fs::create_dir_all(&pack_dir).map_err(|e| NetworkCloneError::Io {
        path: pack_dir.clone(),
        source: e,
    })?;
    let tmp_pack = pack_dir.join(".tmp-clone.pack");
    {
        let mut f = fs::File::create(&tmp_pack).map_err(|e| NetworkCloneError::Io {
            path: tmp_pack.clone(),
            source: e,
        })?;
        f.write_all(pack_bytes).map_err(|e| NetworkCloneError::Io {
            path: tmp_pack.clone(),
            source: e,
        })?;
        f.sync_all().map_err(|e| NetworkCloneError::Io {
            path: tmp_pack.clone(),
            source: e,
        })?;
    }

    let result = explode_and_repack(&tmp_pack, &pack_dir, hash_kind);
    let _ = fs::remove_file(&tmp_pack); // best-effort cleanup
    result
}

/// Walk the temp pack, resolve every entry (including OFS/REF deltas) into a
/// concrete `(oid, kind, body)` triple, then call `write_pack_from_objects`.
fn explode_and_repack(
    tmp_pack: &Path,
    out_dir: &Path,
    hash_kind: HashKind,
) -> Result<(), NetworkCloneError> {
    let pf = PackFile::open(tmp_pack, hash_kind)?;
    // Cache the resolved (kind, body) of every entry, keyed by pack offset,
    // so OFS/REF deltas can patch against earlier entries we already decoded.
    let mut cache: std::collections::HashMap<u64, (ObjectKind, Vec<u8>)> =
        std::collections::HashMap::new();
    let mut oid_lookup: std::collections::HashMap<ObjectId, u64> = std::collections::HashMap::new();
    let mut objects: Vec<(ObjectId, ObjectKind, Vec<u8>)> = Vec::new();

    for entry in pf.iter_entries() {
        let entry = entry?;
        let (kind, body) = resolve_entry(&pf, &entry, &cache, &oid_lookup)?;
        // Hash via RawObject so we share the framing logic with the writer
        // side (loose store, pack builder).
        let oid = crate::object::RawObject::new(kind, body.clone()).oid(hash_kind);
        cache.insert(entry.offset, (kind, body.clone()));
        oid_lookup.insert(oid, entry.offset);
        objects.push((oid, kind, body));
    }

    // Re-pack deterministically. The output names itself `pack-<sha>.{pack,idx}`,
    // matching the rest of the repo's pack-dir layout.
    pack::build::write_pack_from_objects(&objects, out_dir, hash_kind)?;
    Ok(())
}

/// Resolve one entry to its final `(kind, body)`. Mirrors the resolution loop
/// in `cli::unpack_objects` but operates against a `(offset → resolved)` cache
/// rather than re-walking the pack.
fn resolve_entry(
    pf: &PackFile,
    entry: &RawPackEntry,
    cache: &std::collections::HashMap<u64, (ObjectKind, Vec<u8>)>,
    oid_lookup: &std::collections::HashMap<ObjectId, u64>,
) -> Result<(ObjectKind, Vec<u8>), NetworkCloneError> {
    match &entry.kind {
        PackEntryKind::Direct(kind) => Ok((*kind, entry.data.clone())),
        PackEntryKind::OfsDelta { base_offset } => {
            let (base_kind, base_body) = match cache.get(base_offset) {
                Some(b) => b.clone(),
                None => {
                    // Cache miss is unusual (pack-objects emits bases-first),
                    // but it can happen with thin packs — re-read the base.
                    let base_entry = pf.read_entry_at(*base_offset)?;
                    resolve_entry(pf, &base_entry, cache, oid_lookup)?
                }
            };
            let patched = pack::apply_delta(&base_body, &entry.data).map_err(|e| {
                PackError::Malformed(Box::leak(
                    format!("apply ofs-delta failed: {e}").into_boxed_str(),
                ))
            })?;
            Ok((base_kind, patched))
        }
        PackEntryKind::RefDelta { base_oid } => {
            // The base might live earlier in this very pack (thin-pack base
            // already sent). We look it up by oid in our running cache.
            if let Some(base_offset) = oid_lookup.get(base_oid) {
                if let Some(base) = cache.get(base_offset) {
                    let patched = pack::apply_delta(&base.1, &entry.data).map_err(|e| {
                        PackError::Malformed(Box::leak(
                            format!("apply ref-delta failed: {e}").into_boxed_str(),
                        ))
                    })?;
                    return Ok((base.0, patched));
                }
            }
            // M10: we don't yet support cross-pack base resolution during the
            // initial clone. Real-world fetch responses don't generate this
            // case for a no-haves clone — the server only sends what we
            // already have indirectly, and we have nothing. Treat as
            // malformed.
            Err(PackError::Malformed("ref-delta base not present in clone pack").into())
        }
    }
}

// ---------------------------------------------------------------------------
// Refs
// ---------------------------------------------------------------------------

/// For every advertised `refs/heads/<name>`, write `refs/remotes/origin/<name>`.
/// If HEAD has a symref-target like `refs/heads/<branch>`, also write the
/// local `refs/heads/<branch>` ref and set HEAD = `ref: refs/heads/<branch>`.
/// For annotated tags (`refs/tags/<name>`), mirror them under
/// `refs/tags/<name>` in the destination too.
fn write_refs(
    dst_repo: &Repository,
    advertised: &[AdvertisedRef],
    _hash_kind: HashKind,
) -> Result<(), NetworkCloneError> {
    // We do two passes through a single transaction. Pass A: mirror every
    // ref under refs/heads/ and refs/tags/. Pass B: set HEAD according to
    // the advertised HEAD's symref-target. We need the data from pass A
    // before HEAD so we don't write a HEAD pointing at a branch we don't
    // also keep locally.
    let mut tx = dst_repo.refs().transaction();

    let mut head_target: Option<String> = None;
    let mut head_oid: Option<ObjectId> = None;

    for r in advertised {
        match r.name.as_str() {
            "HEAD" => {
                head_target = r.symref_target.clone();
                head_oid = Some(r.oid);
            }
            name if name.starts_with("refs/heads/") => {
                let suffix = name.strip_prefix("refs/heads/").unwrap();
                let remote_name = format!("refs/remotes/origin/{suffix}");
                let remote_full = FullName::new(remote_name).map_err(RefError::Name)?;
                tx.update(
                    &remote_full,
                    ExpectedOldValue::Any,
                    NewValue::Direct(r.oid),
                    ReflogMessage::none(),
                )?;
            }
            name if name.starts_with("refs/tags/") => {
                let full = FullName::new(name.to_string()).map_err(RefError::Name)?;
                tx.update(
                    &full,
                    ExpectedOldValue::Any,
                    NewValue::Direct(r.oid),
                    ReflogMessage::none(),
                )?;
            }
            _ => {
                // Skip refs we don't understand (e.g. `refs/changes/…` from
                // Gerrit). Mirroring those is an M11+ concern.
            }
        }
    }

    // Pass B: HEAD and its underlying local branch.
    if let (Some(target), Some(_oid)) = (head_target.as_ref(), head_oid) {
        if let Some(branch_suffix) = target.strip_prefix("refs/heads/") {
            // Resolve which advertised entry holds this branch's oid. The
            // server may have given us HEAD's oid directly OR it may match a
            // separate `refs/heads/<branch>` entry. Either way we look up
            // the branch's oid in the advertisement.
            let branch_full =
                FullName::new(format!("refs/heads/{branch_suffix}")).map_err(RefError::Name)?;
            let branch_oid = advertised
                .iter()
                .find(|r| r.name == branch_full.as_str())
                .map(|r| r.oid)
                .or(head_oid)
                .expect("we already verified head_oid is Some");

            tx.update(
                &branch_full,
                ExpectedOldValue::Any,
                NewValue::Direct(branch_oid),
                ReflogMessage::from(format!("clone: from {branch_oid}")),
            )?;
            tx.update(
                &FullName::new("HEAD").map_err(RefError::Name)?,
                ExpectedOldValue::Any,
                NewValue::Symbolic(branch_full),
                ReflogMessage::none(),
            )?;
        } else {
            // Symref-target was something unusual; treat HEAD as detached.
            if let Some(oid) = head_oid {
                tx.update(
                    &FullName::new("HEAD").map_err(RefError::Name)?,
                    ExpectedOldValue::Any,
                    NewValue::Direct(oid),
                    ReflogMessage::from(format!("clone: detached at {oid}")),
                )?;
            }
        }
    } else if let Some(oid) = head_oid {
        // No symref-target — detached HEAD.
        tx.update(
            &FullName::new("HEAD").map_err(RefError::Name)?,
            ExpectedOldValue::Any,
            NewValue::Direct(oid),
            ReflogMessage::from(format!("clone: detached at {oid}")),
        )?;
    }

    tx.commit()?;
    Ok(())
}

/// Extract HEAD's oid from the advertisement, if HEAD was advertised.
fn head_oid_from_advertised(refs: &[AdvertisedRef]) -> Option<ObjectId> {
    refs.iter().find(|r| r.name == "HEAD").map(|r| r.oid)
}

// ---------------------------------------------------------------------------
// Tree peeling for checkout (mirrors clone_local's peel_to_tree).
// ---------------------------------------------------------------------------

fn peel_to_tree(repo: &Repository, oid: ObjectId) -> Result<ObjectId, NetworkCloneError> {
    use crate::commit::Commit;
    let obj = repo.odb().read(&oid).map_err(|e| NetworkCloneError::Io {
        path: repo.gitdir().to_path_buf(),
        source: io::Error::other(format!("odb read {oid}: {e}")),
    })?;
    match obj.kind {
        ObjectKind::Tree => Ok(oid),
        ObjectKind::Commit => {
            let c =
                Commit::parse(&obj.data, repo.hash_kind()).map_err(|e| NetworkCloneError::Io {
                    path: repo.gitdir().to_path_buf(),
                    source: io::Error::new(io::ErrorKind::InvalidData, format!("{e}")),
                })?;
            Ok(c.tree)
        }
        ObjectKind::Tag => {
            let body = std::str::from_utf8(&obj.data).map_err(|_| NetworkCloneError::Io {
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
            Err(NetworkCloneError::Io {
                path: repo.gitdir().to_path_buf(),
                source: io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("tag {oid} missing object line"),
                ),
            })
        }
        other => Err(NetworkCloneError::Io {
            path: repo.gitdir().to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{oid} is a {other}, not commit-ish"),
            ),
        }),
    }
}

// ---------------------------------------------------------------------------
// Layout helpers (mirror cli::init's behaviour but without probing the
// destination filesystem — we don't have it materialized yet at the point we
// write config, and the defaults are good enough for a fresh clone).
// ---------------------------------------------------------------------------

fn dir_is_nonempty(p: &Path) -> Result<bool, NetworkCloneError> {
    let mut entries = fs::read_dir(p).map_err(|e| NetworkCloneError::Io {
        path: p.to_path_buf(),
        source: e,
    })?;
    Ok(entries.next().is_some())
}

fn create_layout(gitdir: &Path) -> Result<(), NetworkCloneError> {
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
        let p = gitdir.join(sub);
        fs::create_dir_all(&p).map_err(|e| NetworkCloneError::Io { path: p, source: e })?;
    }
    Ok(())
}

fn write_config(gitdir: &Path, hash_kind: HashKind) -> Result<(), NetworkCloneError> {
    let format_version = match hash_kind {
        HashKind::Sha1 => 0,
        HashKind::Sha256 => 1,
    };
    let mut s = String::new();
    s.push_str("[core]\n");
    s.push_str(&format!("\trepositoryformatversion = {format_version}\n"));
    s.push_str(&format!("\tfilemode = {}\n", b2s(cfg!(unix))));
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

fn write_description(gitdir: &Path) -> Result<(), NetworkCloneError> {
    let body = "Unnamed repository; edit this file 'description' to name the repository.\n";
    write_atomic(&gitdir.join("description"), body.as_bytes())
}

fn write_info_exclude(gitdir: &Path) -> Result<(), NetworkCloneError> {
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

fn write_atomic(target: &Path, contents: &[u8]) -> Result<(), NetworkCloneError> {
    let parent = target.parent().expect("target has parent");
    fs::create_dir_all(parent).map_err(|e| NetworkCloneError::Io {
        path: parent.to_path_buf(),
        source: e,
    })?;
    let tmp = target.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp).map_err(|e| NetworkCloneError::Io {
            path: tmp.clone(),
            source: e,
        })?;
        f.write_all(contents).map_err(|e| NetworkCloneError::Io {
            path: tmp.clone(),
            source: e,
        })?;
        f.sync_all().map_err(|e| NetworkCloneError::Io {
            path: tmp.clone(),
            source: e,
        })?;
    }
    fs::rename(&tmp, target).map_err(|e| NetworkCloneError::Io {
        path: target.to_path_buf(),
        source: e,
    })
}

fn b2s(b: bool) -> &'static str {
    if b {
        "true"
    } else {
        "false"
    }
}

// ---------------------------------------------------------------------------
// Public re-export for the fetch CLI — same machinery, but skipping repo init
// and updating only `refs/remotes/<remote>/*`.
// ---------------------------------------------------------------------------

/// Like `clone_network` but operating against an existing repo. Used by
/// `rustygit fetch` to update remote-tracking refs without re-initializing
/// the destination or touching `refs/heads/`.
pub fn fetch_into_repo(
    repo: &Repository,
    url: &str,
    remote_name: &str,
    quiet: bool,
) -> Result<Vec<AdvertisedRef>, NetworkCloneError> {
    // Apply `[url "<base>"] insteadOf` rewrites from the repo's layered config.
    let cfg = crate::config::Config::from_repo_dir(repo.commondir()).unwrap_or_default();
    let mut conn = crate::transport::connect_upload_pack_with_config(url, &cfg)?;
    let cap_pkts = conn.discover_capabilities()?;
    let cap = CapabilityAdvertisement::parse(&cap_pkts)?;
    if !cap.supports("ls-refs") {
        return Err(NetworkCloneError::NoLsRefs);
    }
    if !cap.supports("fetch") {
        return Err(NetworkCloneError::NoFetch);
    }
    let hash_kind = cap.object_format;
    if hash_kind != repo.hash_kind() {
        // We don't yet support cross-hash clones / fetches. Fail loudly.
        return Err(NetworkCloneError::Io {
            path: repo.gitdir().to_path_buf(),
            source: io::Error::other(format!(
                "remote uses {} but local repo uses {}",
                hash_kind,
                repo.hash_kind()
            )),
        });
    }

    let advertised =
        protocol_v2::ls_refs(&mut conn, &["HEAD", "refs/heads/", "refs/tags/"], hash_kind)?;
    if advertised.is_empty() {
        return Err(NetworkCloneError::NoRefs);
    }

    // Build wants — only oids we don't already have locally.
    let mut wants: Vec<ObjectId> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for r in &advertised {
        if seen.insert(r.oid) && !repo.odb().contains(&r.oid).unwrap_or(false) {
            wants.push(r.oid);
        }
    }

    if !wants.is_empty() {
        let fetch_result = protocol_v2::fetch(&mut conn, &wants, hash_kind)?;
        write_pack_into_repo(repo.gitdir(), &fetch_result.pack_bytes, hash_kind)?;
    } else if !quiet {
        println!("Already up to date.");
    }

    // Update remote-tracking refs only (NEVER refs/heads/).
    let mut tx = repo.refs().transaction();
    for r in &advertised {
        if let Some(suffix) = r.name.strip_prefix("refs/heads/") {
            let remote_name_full = FullName::new(format!("refs/remotes/{remote_name}/{suffix}"))
                .map_err(RefError::Name)?;
            tx.update(
                &remote_name_full,
                ExpectedOldValue::Any,
                NewValue::Direct(r.oid),
                ReflogMessage::from(format!("fetch: {url}")),
            )?;
        } else if r.name.starts_with("refs/tags/") {
            let full = FullName::new(r.name.clone()).map_err(RefError::Name)?;
            tx.update(
                &full,
                ExpectedOldValue::Any,
                NewValue::Direct(r.oid),
                ReflogMessage::none(),
            )?;
        }
    }
    tx.commit()?;

    Ok(advertised)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dest_not_empty_check() {
        let tmp = tempfile::tempdir().unwrap();
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join("x"), b"y").unwrap();

        // We can't run the full clone without a server, but we CAN exercise
        // the empty-directory guard before any network IO happens.
        // `clone_network` checks dst before opening a connection, so it
        // should fail fast with DestNotEmpty here.
        //
        // Use a syntactically valid HTTPS URL that we'd never actually
        // contact: the guard runs before that.
        let r = clone_network(
            "https://invalid.example.test/repo.git",
            &dst,
            &NetworkCloneOpts {
                quiet: true,
                no_checkout: true,
            },
        );
        match r {
            Err(NetworkCloneError::DestNotEmpty(p)) => assert_eq!(p, dst),
            other => panic!("expected DestNotEmpty, got {other:?}"),
        }
    }
}
