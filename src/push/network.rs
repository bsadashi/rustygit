//! Network push — drive `git-receive-pack` over HTTPS via Track A's
//! `send_pack` primitives.
//!
//! Flow:
//!
//!   1. Open a `ReceivePackConnection` against the URL.
//!   2. Discover the server's current refs + capabilities.
//!   3. For each refspec: compute (old, new) where old comes from the
//!      advertisement and new from the local repo's ref. Enforce a
//!      fast-forward check unless `force` was set. Decide whether this is
//!      a Create, Update, or Delete.
//!   4. Build a pack containing every object reachable from new tips but not
//!      reachable from the server's old tips (M11: we conservatively
//!      include any object we can't prove the server has). The pack lives
//!      in a temp file under `<repo>/.git/objects/pack/` for the duration
//!      of the request.
//!   5. Send the request body (`encode_request`) over the connection.
//!   6. Parse the `ReportStatus` returned by the server. If `unpack_ok` is
//!      false or any per-ref status is `ng`, surface the failure as
//!      `NetworkPushError::Rejected`.
//!   7. On success, update local `refs/remotes/<remote>/<branch>` to track
//!      the pushed tip — so subsequent fetches don't see the push as
//!      remote-side divergence.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::PathBuf;

use crate::hash::ObjectId;
use crate::pack;
use crate::reachable::{ReachableError, ReachableSet};
use crate::refs::{ExpectedOldValue, FullName, NewValue, RefError, ReflogMessage};
use crate::repo::Repository;
use crate::transport::TransportError;
// Track A's send_pack module. If it isn't merged yet this import will fail
// at compile time — that's expected and noted in the task brief.
use crate::transport::send_pack::{
    encode_request, negotiate_capabilities, PushCommand, ReceivePackConnection, RefStatus,
    SendPackError,
};

use super::{PushError, PushOpts, Refspec};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum NetworkPushError {
    #[error(transparent)]
    Push(#[from] PushError),
    #[error(transparent)]
    Refs(#[from] RefError),
    #[error(transparent)]
    Reachable(#[from] ReachableError),
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    SendPack(#[from] SendPackError),
    #[error(transparent)]
    PackBuild(#[from] pack::PackBuildError),
    #[error(transparent)]
    Odb(#[from] crate::odb::OdbError),
    #[error("server rejected push: unpack-ok={unpack_ok}, {failures} ref(s) failed")]
    Rejected {
        unpack_ok: bool,
        failures: usize,
        details: Vec<RefStatus>,
        unpack_message: Option<String>,
    },
    #[error("hash algorithm mismatch: local is {local}, server is {server}")]
    HashMismatch {
        local: crate::hash::HashKind,
        server: crate::hash::HashKind,
    },
    #[error("io on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

// ---------------------------------------------------------------------------
// Per-ref outcome
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum NetworkRefOutcome {
    Created {
        dst: String,
        new: ObjectId,
    },
    Updated {
        dst: String,
        old: ObjectId,
        new: ObjectId,
    },
    Forced {
        dst: String,
        old: ObjectId,
        new: ObjectId,
    },
    Deleted {
        dst: String,
        old: ObjectId,
    },
    UpToDate {
        dst: String,
        oid: ObjectId,
    },
}

#[derive(Debug, Default)]
pub struct NetworkPushReport {
    pub outcomes: Vec<NetworkRefOutcome>,
    /// Mirror of the URL passed in, suitable for the "To <url>" header.
    pub url: String,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn push_network(
    src_repo: &Repository,
    url: &str,
    refspecs: &[Refspec],
    opts: &PushOpts,
) -> Result<NetworkPushReport, NetworkPushError> {
    // 1. Open connection, discover server state. Apply the user's URL
    // rewrites — both `pushInsteadOf` (push-specific) and `insteadOf`
    // (universal) — so `git@github.com:owner/repo` routes to
    // `https://github.com/owner/repo` when configured.
    let cfg = crate::config::Config::from_repo_dir(src_repo.commondir()).unwrap_or_default();
    let conn = ReceivePackConnection::new_with_config(url, &cfg)?;
    let adv = conn.discover()?;

    if adv.object_format != src_repo.hash_kind() {
        return Err(NetworkPushError::HashMismatch {
            local: src_repo.hash_kind(),
            server: adv.object_format,
        });
    }

    // Build a quick lookup: server's ref name → oid.
    let mut server_refs: std::collections::BTreeMap<String, ObjectId> =
        std::collections::BTreeMap::new();
    for r in &adv.refs {
        server_refs.insert(r.name.clone(), r.oid);
    }

    // 2. Plan each refspec.
    let mut plans: Vec<RefPlan> = Vec::with_capacity(refspecs.len());
    for rs in refspecs {
        let plan = plan_refspec(src_repo, &server_refs, rs, opts)?;
        plans.push(plan);
    }

    // 3. Collect commands and the set of object ids to send.
    let mut commands: Vec<PushCommand> = Vec::new();
    let mut new_tips: Vec<ObjectId> = Vec::new();
    let mut old_tips: Vec<ObjectId> = Vec::new();
    for plan in &plans {
        match &plan.action {
            PlanAction::Create { new } => {
                commands.push(PushCommand::Create {
                    name: plan.dst_name.as_str().to_string(),
                    new: *new,
                });
                new_tips.push(*new);
            }
            PlanAction::Update { old, new } | PlanAction::Force { old, new } => {
                commands.push(PushCommand::Update {
                    name: plan.dst_name.as_str().to_string(),
                    old: *old,
                    new: *new,
                });
                new_tips.push(*new);
                old_tips.push(*old);
            }
            PlanAction::Delete { old } => {
                commands.push(PushCommand::Delete {
                    name: plan.dst_name.as_str().to_string(),
                    old: *old,
                });
            }
            PlanAction::UpToDate => continue,
        }
    }

    // If every plan is a no-op, short-circuit.
    if commands.is_empty() {
        return Ok(NetworkPushReport {
            outcomes: plans.into_iter().map(|p| p.outcome).collect(),
            url: url.to_string(),
        });
    }

    // 4. Build the pack of new objects to send.
    let pack_bytes = build_pack_bytes(src_repo, &new_tips, &old_tips, &server_refs)?;

    // 5. Negotiate capabilities and encode the request.
    let cap_request = negotiate_capabilities(&adv.capabilities);
    let body = encode_request(&commands, &pack_bytes, &cap_request, adv.object_format);

    // 6. Send and decode the report-status.
    let report = conn.send(body)?;

    // 7. Check for failure.
    let failures: Vec<RefStatus> = report
        .command_results
        .iter()
        .filter(|s| !s.ok)
        .cloned()
        .collect();
    if !report.unpack_ok || !failures.is_empty() {
        return Err(NetworkPushError::Rejected {
            unpack_ok: report.unpack_ok,
            failures: failures.len(),
            details: failures,
            unpack_message: report.unpack_message,
        });
    }

    // 8. On success, mirror pushed tips to refs/remotes/origin/<suffix>. The
    //    remote name is hard-coded to `origin` for M11 — config-driven
    //    remote naming is M12+.
    update_remote_tracking(src_repo, &plans, url)?;

    let report = NetworkPushReport {
        outcomes: plans.into_iter().map(|p| p.outcome).collect(),
        url: url.to_string(),
    };
    Ok(report)
}

// ---------------------------------------------------------------------------
// Planning (mirrors local but consults the server's advertisement)
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum PlanAction {
    Create { new: ObjectId },
    Update { old: ObjectId, new: ObjectId },
    Force { old: ObjectId, new: ObjectId },
    Delete { old: ObjectId },
    UpToDate,
}

#[derive(Debug)]
struct RefPlan {
    dst_name: FullName,
    action: PlanAction,
    outcome: NetworkRefOutcome,
}

fn plan_refspec(
    src_repo: &Repository,
    server_refs: &std::collections::BTreeMap<String, ObjectId>,
    rs: &Refspec,
    opts: &PushOpts,
) -> Result<RefPlan, NetworkPushError> {
    let dst_full = FullName::new(rs.dst.clone()).map_err(PushError::from)?;

    if rs.is_delete() {
        let old = match server_refs.get(rs.dst.as_str()) {
            Some(o) => *o,
            None => {
                return Err(NetworkPushError::Push(PushError::DeleteMissing {
                    dst: rs.dst.clone(),
                }))
            }
        };
        return Ok(RefPlan {
            dst_name: dst_full,
            action: PlanAction::Delete { old },
            outcome: NetworkRefOutcome::Deleted {
                dst: rs.dst.clone(),
                old,
            },
        });
    }

    // Non-delete: resolve src in local repo.
    let src_full = FullName::new(rs.src.clone()).map_err(PushError::from)?;
    let new = match crate::refs::RefTarget::resolve(src_repo.refs(), &src_full)? {
        Some((_, oid)) => oid,
        None => {
            return Err(NetworkPushError::Push(PushError::SourceMissing(
                rs.src.clone(),
            )))
        }
    };
    let old = server_refs.get(rs.dst.as_str()).copied();

    match old {
        None => Ok(RefPlan {
            dst_name: dst_full,
            action: PlanAction::Create { new },
            outcome: NetworkRefOutcome::Created {
                dst: rs.dst.clone(),
                new,
            },
        }),
        Some(old) if old == new => Ok(RefPlan {
            dst_name: dst_full,
            action: PlanAction::UpToDate,
            outcome: NetworkRefOutcome::UpToDate {
                dst: rs.dst.clone(),
                oid: new,
            },
        }),
        Some(old) => {
            let force = rs.force || opts.force;
            let is_ff = is_ancestor(src_repo, old, new)?;
            if !force && !is_ff {
                return Err(NetworkPushError::Push(PushError::NonFastForward {
                    dst: rs.dst.clone(),
                    old: old.to_string(),
                    new: new.to_string(),
                }));
            }
            let (action, outcome) = if is_ff {
                (
                    PlanAction::Update { old, new },
                    NetworkRefOutcome::Updated {
                        dst: rs.dst.clone(),
                        old,
                        new,
                    },
                )
            } else {
                (
                    PlanAction::Force { old, new },
                    NetworkRefOutcome::Forced {
                        dst: rs.dst.clone(),
                        old,
                        new,
                    },
                )
            };
            Ok(RefPlan {
                dst_name: dst_full,
                action,
                outcome,
            })
        }
    }
}

fn is_ancestor(
    repo: &Repository,
    ancestor: ObjectId,
    descendant: ObjectId,
) -> Result<bool, NetworkPushError> {
    if ancestor == descendant {
        return Ok(true);
    }
    let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
    let mut queue: Vec<ObjectId> = vec![descendant];
    let hash_kind = repo.hash_kind();
    while let Some(oid) = queue.pop() {
        if !seen.insert(oid) {
            continue;
        }
        if oid == ancestor {
            return Ok(true);
        }
        let obj = match repo.odb().read(&oid) {
            Ok(o) => o,
            Err(crate::odb::OdbError::NotFound(_)) => continue,
            Err(e) => return Err(NetworkPushError::Odb(e)),
        };
        if obj.kind != crate::object::ObjectKind::Commit {
            continue;
        }
        let commit = crate::commit::Commit::parse(&obj.data, hash_kind).map_err(|e| {
            NetworkPushError::Io {
                path: repo.gitdir().to_path_buf(),
                source: io::Error::new(io::ErrorKind::InvalidData, format!("{e}")),
            }
        })?;
        for p in &commit.parents {
            queue.push(*p);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Pack building
// ---------------------------------------------------------------------------

/// Build a pack of objects reachable from `new_tips` but not from `old_tips`
/// (or from any other server-advertised ref oid we already have locally),
/// return the raw pack bytes.
///
/// Strategy: walk new tips → set N. Walk every server oid that we ALSO have
/// locally → set S. Send N \ S.
fn build_pack_bytes(
    src_repo: &Repository,
    new_tips: &[ObjectId],
    old_tips: &[ObjectId],
    server_refs: &std::collections::BTreeMap<String, ObjectId>,
) -> Result<Vec<u8>, NetworkPushError> {
    if new_tips.is_empty() {
        return Ok(Vec::new());
    }
    let news = ReachableSet::mark_from(src_repo, new_tips)?;

    // Server starts: every server-side oid we have locally is fair game as a
    // "stop" point. We dedup with old_tips.
    let mut stops_vec: Vec<ObjectId> = old_tips.to_vec();
    for oid in server_refs.values() {
        if !stops_vec.contains(oid) && src_repo.odb().contains(oid).unwrap_or(false) {
            stops_vec.push(*oid);
        }
    }
    let stops = if stops_vec.is_empty() {
        BTreeSet::new()
    } else {
        ReachableSet::mark_from(src_repo, &stops_vec)?.oids
    };

    let mut out: Vec<ObjectId> = Vec::new();
    for oid in &news.oids {
        if stops.contains(oid) {
            continue;
        }
        out.push(*oid);
    }
    if out.is_empty() {
        return Ok(Vec::new());
    }

    // Write to a temp dir under `objects/pack`, slurp, then delete. Reusing
    // the existing `write_pack` writer is simpler than refactoring it to
    // produce a Vec directly.
    let tmp_dir = src_repo.gitdir().join("objects").join("pack");
    fs::create_dir_all(&tmp_dir).map_err(|e| NetworkPushError::Io {
        path: tmp_dir.clone(),
        source: e,
    })?;
    let staging = tmp_dir.join(".push-staging");
    fs::create_dir_all(&staging).map_err(|e| NetworkPushError::Io {
        path: staging.clone(),
        source: e,
    })?;
    let result = pack::build::write_pack(&out, src_repo.odb(), &staging, src_repo.hash_kind())?;
    let bytes = fs::read(&result.pack_path).map_err(|e| NetworkPushError::Io {
        path: result.pack_path.clone(),
        source: e,
    })?;
    // Best-effort cleanup of the temp pack files.
    let _ = fs::remove_file(&result.pack_path);
    let _ = fs::remove_file(&result.idx_path);
    let _ = fs::remove_dir(&staging);
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Local remote-tracking update
// ---------------------------------------------------------------------------

fn update_remote_tracking(
    src_repo: &Repository,
    plans: &[RefPlan],
    url: &str,
) -> Result<(), NetworkPushError> {
    let mut tx = src_repo.refs().transaction();
    for plan in plans {
        let suffix = match plan.dst_name.as_str().strip_prefix("refs/heads/") {
            Some(s) => s,
            // Non-branch refs (tags etc.) aren't mirrored under
            // refs/remotes/origin/. Skip them silently.
            None => continue,
        };
        match &plan.action {
            PlanAction::Create { new }
            | PlanAction::Update { new, .. }
            | PlanAction::Force { new, .. } => {
                let tracking = FullName::new(format!("refs/remotes/origin/{suffix}"))
                    .map_err(|e| NetworkPushError::Refs(RefError::Name(e)))?;
                tx.update(
                    &tracking,
                    ExpectedOldValue::Any,
                    NewValue::Direct(*new),
                    ReflogMessage::from(format!("update by push to {url}")),
                )?;
            }
            PlanAction::Delete { .. } => {
                // Drop the matching remote-tracking ref.
                let tracking = FullName::new(format!("refs/remotes/origin/{suffix}"))
                    .map_err(|e| NetworkPushError::Refs(RefError::Name(e)))?;
                tx.delete(&tracking, ExpectedOldValue::Any)?;
            }
            PlanAction::UpToDate => continue,
        }
    }
    tx.commit()?;
    Ok(())
}
