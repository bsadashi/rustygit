//! `rustygit gc` — garbage-collect the object store.
//!
//! M9 scope is a thin wrapper over `repack -a -d`: consolidate everything
//! reachable into a single fresh pack, then drop the now-redundant loose
//! objects and old packs.
//!
//! TODO(M14+): honor `gc.reflogExpire`, `gc.reflogExpireUnreachable`,
//! `gc.pruneExpire`, and the broader `git gc --aggressive` knobs. For M9
//! we deliberately only prune objects that are *now redundant* (i.e. live in
//! the new pack); unreachable loose objects are left alone so we don't
//! discard a stash or in-progress work that hasn't yet been referenced.

use std::io;

use clap::Args;

use crate::cli::repack::{self, RepackArgs};
use crate::hooks::{self, HookRunner};
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct GcArgs {
    /// Be quiet.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Recompress objects more aggressively. Accepted for compatibility;
    /// currently a no-op in M9 (we don't yet produce delta-encoded packs at
    /// any quality level, so there's nothing to be aggressive about).
    #[arg(long = "aggressive")]
    pub aggressive: bool,

    /// Skip the consistency check `git gc` runs before pruning. We don't
    /// run a check anyway in M9, so this is a no-op accepted for parity.
    #[arg(long = "auto")]
    pub auto: bool,
}

pub fn run(args: GcArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    // pre-auto-gc hook: per githooks(5), invoked by `git gc --auto` and
    // exiting non-zero aborts the gc. We fire it for both `--auto` and
    // explicit invocations because rustygit's gc is a thin shim — there
    // is no separate code path to differentiate.
    let runner = HookRunner::from_repo(&repo);
    let outcome = runner.run("pre-auto-gc", &[], None)?;
    if outcome.aborts_parent() {
        let code = outcome.exit_code().unwrap_or(1);
        hooks::print_abort("gc", "pre-auto-gc", code);
        return Ok(1);
    }

    let repack_args = RepackArgs {
        delete: true,
        all: true,
        quiet: args.quiet,
    };
    match repack::repack(&repo, &repack_args) {
        Ok(()) => Ok(0),
        Err(e) => {
            eprintln!("rustygit: gc: {e}");
            Ok(128)
        }
    }
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct Wrap {
        #[command(flatten)]
        args: GcArgs,
    }

    #[test]
    fn parses_defaults() {
        let w = Wrap::try_parse_from(["x"]).unwrap();
        assert!(!w.args.quiet);
        assert!(!w.args.aggressive);
        assert!(!w.args.auto);
    }

    #[test]
    fn parses_quiet() {
        let w = Wrap::try_parse_from(["x", "-q"]).unwrap();
        assert!(w.args.quiet);
    }

    #[test]
    fn parses_aggressive_and_auto() {
        let w = Wrap::try_parse_from(["x", "--aggressive", "--auto"]).unwrap();
        assert!(w.args.aggressive);
        assert!(w.args.auto);
    }

    #[test]
    fn rejects_unknown_flag() {
        let r = Wrap::try_parse_from(["x", "--no-such-flag"]);
        assert!(r.is_err());
    }
}
