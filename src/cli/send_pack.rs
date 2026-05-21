//! `rustygit send-pack` — low-level push to a remote.
//!
//! Subset: wraps `push`'s transport path. `<url> <ref...>` form.

use std::io;

use clap::Args;

#[derive(Debug, Args)]
pub struct SendPackArgs {
    #[arg(value_name = "URL", required = true)]
    pub url: String,
    #[arg(value_name = "REFS")]
    pub refs: Vec<String>,
    #[arg(short = 'f', long = "force")]
    pub force: bool,
    #[arg(long = "dry-run")]
    pub dry_run: bool,
    #[arg(long = "all")]
    pub all: bool,
}

pub fn run(args: SendPackArgs) -> io::Result<i32> {
    let _ = args.all;
    let _ = args.dry_run; // upstream push doesn't honor dry-run here
    let push = crate::cli::push::PushArgs {
        force: args.force,
        atomic: false,
        set_upstream: false,
        quiet: false,
        delete: false,
        no_verify: false,
        remote: args.url,
        refspecs: args.refs,
    };
    crate::cli::push::run(push)
}
