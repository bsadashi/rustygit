//! `rustygit symbolic-ref` — read or write a symbolic ref.
//!
//! Forms:
//!   `symbolic-ref <NAME>` — print the target full name (e.g. `refs/heads/main`)
//!   `symbolic-ref <NAME> <REF>` — point <NAME> at <REF>
//!   `symbolic-ref --short <NAME>` — print short form (e.g. `main` for `refs/heads/main`)
//!   `symbolic-ref -d <NAME>` — delete

use std::io;

use clap::Args;

use crate::refs::{ExpectedOldValue, FullName, NewValue, RefTarget, ReflogMessage};
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct SymbolicRefArgs {
    /// Print short form (strip `refs/heads/` etc.).
    #[arg(long = "short")]
    pub short: bool,

    /// Quiet: suppress error if the ref doesn't exist; just exit non-zero.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Delete the symbolic ref.
    #[arg(short = 'd', long = "delete")]
    pub delete: bool,

    /// Reason text for the reflog.
    #[arg(short = 'm', value_name = "MESSAGE")]
    pub message: Option<String>,

    /// The symbolic-ref name (typically `HEAD`).
    #[arg(value_name = "NAME")]
    pub name: String,

    /// New target (omit to read the current one).
    #[arg(value_name = "REF")]
    pub target: Option<String>,
}

pub fn run(args: SymbolicRefArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let name = match FullName::new(&args.name) {
        Ok(n) => n,
        Err(e) => {
            if !args.quiet {
                eprintln!("rustygit: {e}");
            }
            return Ok(128);
        }
    };

    if args.delete {
        let mut tx = repo.refs().transaction();
        tx.delete(&name, ExpectedOldValue::Any).map_err(io_err)?;
        tx.commit().map_err(io_err)?;
        return Ok(0);
    }

    if let Some(target_str) = args.target {
        let target = FullName::new(target_str.trim())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e}")))?;
        let reflog = args
            .message
            .map(ReflogMessage::from)
            .unwrap_or_else(ReflogMessage::none);
        let mut tx = repo.refs().transaction();
        tx.update(
            &name,
            ExpectedOldValue::Any,
            NewValue::Symbolic(target),
            reflog,
        )
        .map_err(io_err)?;
        tx.commit().map_err(io_err)?;
        return Ok(0);
    }

    // Read mode.
    let r = match repo.refs().read(&name).map_err(io_err)? {
        Some(r) => r,
        None => {
            if !args.quiet {
                eprintln!("rustygit: ref {} not found", name);
            }
            return Ok(1);
        }
    };
    let target = match r.target {
        RefTarget::Symbolic(t) => t,
        RefTarget::Direct(_) => {
            if !args.quiet {
                eprintln!("rustygit: ref {} is not a symbolic ref", name);
            }
            return Ok(1);
        }
    };
    if args.short {
        println!("{}", short_form(target.as_str()));
    } else {
        println!("{target}");
    }
    Ok(0)
}

fn short_form(full: &str) -> String {
    for prefix in ["refs/heads/", "refs/tags/", "refs/remotes/"] {
        if let Some(rest) = full.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    full.to_string()
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
