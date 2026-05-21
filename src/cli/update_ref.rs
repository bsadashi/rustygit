//! `rustygit update-ref` — atomic ref creation, update, or deletion.
//!
//! Form: `update-ref [-m <reason>] <ref> <newvalue> [<oldvalue>]` (create/update)
//!       `update-ref -d <ref> [<oldvalue>]`                       (delete)
//!
//! `<oldvalue>` may be empty (`""`) to assert the ref does not currently exist.

use std::io;

use clap::Args;

use crate::hash::ObjectId;
use crate::refs::{ExpectedOldValue, FullName, NewValue, RefError, ReflogMessage};
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct UpdateRefArgs {
    /// Reason text recorded in the reflog.
    #[arg(short = 'm', value_name = "MESSAGE")]
    pub message: Option<String>,

    /// Delete the ref instead of updating it.
    #[arg(short = 'd', long = "delete")]
    pub delete: bool,

    /// Don't dereference symbolic refs (M2: not yet honored — refs are always
    /// dereffed; this is a no-op stub for CLI compatibility).
    #[arg(long = "no-deref")]
    pub no_deref: bool,

    /// The ref name (e.g. `refs/heads/main`).
    #[arg(value_name = "REF")]
    pub refname: String,

    /// New value. For `--delete`, optional <oldvalue> goes here.
    #[arg(value_name = "NEWVALUE")]
    pub newvalue: Option<String>,

    /// Old value (assertion). Empty string means "must not exist".
    #[arg(value_name = "OLDVALUE")]
    pub oldvalue: Option<String>,
}

pub fn run(args: UpdateRefArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let hash_kind = repo.hash_kind();
    let name = match FullName::new(&args.refname) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("rustygit: {e}");
            return Ok(128);
        }
    };

    if args.delete {
        let expected = match args.newvalue.as_deref() {
            None | Some("") => ExpectedOldValue::Any,
            Some(hex) => ExpectedOldValue::Direct(parse_oid(hex, hash_kind)?),
        };
        let mut tx = repo.refs().transaction();
        tx.delete(&name, expected).map_err(io_err)?;
        return commit_or_report(tx);
    }

    let new_str = args.newvalue.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "update-ref: <newvalue> required",
        )
    })?;

    let new = if let Some(target) = new_str.strip_prefix("ref:") {
        let target = FullName::new(target.trim())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e}")))?;
        NewValue::Symbolic(target)
    } else {
        NewValue::Direct(parse_oid(&new_str, hash_kind)?)
    };

    let expected = match args.oldvalue.as_deref() {
        None => ExpectedOldValue::Any,
        Some("") => ExpectedOldValue::Missing,
        Some(hex) => ExpectedOldValue::Direct(parse_oid(hex, hash_kind)?),
    };

    // logallrefupdates defaults to true; treat absent -m as empty-message log.
    let reflog = ReflogMessage::from(args.message.unwrap_or_default());

    let mut tx = repo.refs().transaction();
    tx.update(&name, expected, new, reflog).map_err(io_err)?;
    commit_or_report(tx)
}

fn commit_or_report(tx: Box<dyn crate::refs::RefTransactionTrait + '_>) -> io::Result<i32> {
    match tx.commit() {
        Ok(()) => Ok(0),
        Err(RefError::Update(u)) => {
            eprintln!("rustygit: update-ref: {u}");
            Ok(1)
        }
        Err(e) => Err(io_err(e)),
    }
}

fn parse_oid(s: &str, kind: crate::hash::HashKind) -> io::Result<ObjectId> {
    ObjectId::parse_hex(kind, s.trim())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e}")))
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
