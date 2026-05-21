//! `rustygit verify-commit` — verify the GPG signature on a commit object.
//!
//! Mirrors `git verify-commit <commit>`: looks up the commit, extracts its
//! `gpgsig` header, builds the *unsigned* body (commit with `gpgsig`
//! stripped, all other headers preserved), and invokes the configured
//! gpg via [`crate::signing::GpgSigner`] for verification.
//!
//! Exit codes (match git):
//! - `0` on a good, trusted signature.
//! - `1` on a bad signature or unknown-key.
//! - `128` when the commit doesn't exist or has no `gpgsig`.

use std::io;

use clap::Args;

use crate::commit::Commit;
use crate::config::Config;
use crate::object::ObjectKind;
use crate::repo::Repository;
use crate::revparse::resolve;
use crate::signing::{GpgSigner, Signer, VerifyOutcome};

#[derive(Debug, Args)]
pub struct VerifyCommitArgs {
    /// One or more commit ids/refs to verify.
    #[arg(value_name = "COMMIT", required = true)]
    pub commits: Vec<String>,

    /// Print gpg's raw verification output. (For now, we always print
    /// our parsed result; this flag is accepted for upstream-flag parity.)
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,
}

pub fn run(args: VerifyCommitArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let config = Config::from_repo_dir(repo.gitdir()).map_err(io_err)?;
    let signer = GpgSigner::from_config(&config);

    let mut worst: i32 = 0;
    for rev in &args.commits {
        let oid = resolve(repo.refs(), repo.odb(), rev).map_err(io_err)?;
        let obj = repo.odb().read(&oid).map_err(io_err)?;
        if obj.kind != ObjectKind::Commit {
            eprintln!("rustygit: verify-commit: {rev} is not a commit");
            worst = worst.max(128);
            continue;
        }
        let commit = Commit::parse(&obj.data, repo.hash_kind()).map_err(io_err)?;
        let Some(sig) = commit.gpgsig.clone() else {
            eprintln!("rustygit: verify-commit: {rev} has no signature");
            worst = worst.max(128);
            continue;
        };

        // Build the unsigned payload: same commit, gpgsig stripped.
        let mut unsigned = commit.clone();
        unsigned.gpgsig = None;
        let payload = unsigned.serialize();

        match signer.verify(&payload, &sig).map_err(io_err)? {
            VerifyOutcome::Good {
                fingerprint,
                signer: who,
            } => {
                eprintln!(
                    "rustygit: verify-commit: {rev}: GOODSIG{}{}",
                    who.as_deref().map(|s| format!(" {s}")).unwrap_or_default(),
                    fingerprint
                        .as_deref()
                        .map(|s| format!(" (fingerprint {s})"))
                        .unwrap_or_default(),
                );
            }
            VerifyOutcome::UnknownKey => {
                eprintln!(
                    "rustygit: verify-commit: {rev}: signature OK but signing key is not in our keyring"
                );
                worst = worst.max(1);
            }
            VerifyOutcome::Bad { reason } => {
                eprintln!("rustygit: verify-commit: {rev}: BADSIG: {reason}");
                worst = worst.max(1);
            }
        }
    }
    Ok(worst)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
