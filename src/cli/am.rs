//! `rustygit am` — apply a series of mailbox patches to the current branch.
//!
//! Per upstream contract:
//!   1. Read mbox file(s) or stdin.
//!   2. Split into individual messages.
//!   3. For each message, run `mailinfo` (extract author/subject/body),
//!      then `apply` the embedded patch, then `commit` with the author
//!      information from the message.
//!   4. State for `--continue`/`--abort` lives in `.git/rebase-apply/`.

use std::io::{self, Read};

use clap::Args;

#[derive(Debug, Args)]
pub struct AmArgs {
    /// Continue after fixing a conflict.
    #[arg(long = "continue")]
    pub cont: bool,
    /// Skip the current patch.
    #[arg(long = "skip")]
    pub skip: bool,
    /// Abort.
    #[arg(long = "abort")]
    pub abort: bool,
    /// Sign-off each commit.
    #[arg(short = 's', long = "signoff")]
    pub signoff: bool,
    /// Input mbox file(s); stdin if none.
    #[arg(value_name = "MBOX")]
    pub files: Vec<String>,
}

pub fn run(args: AmArgs) -> io::Result<i32> {
    let repo = crate::repo::Repository::discover_from_cwd().map_err(io_err)?;
    let am_dir = repo.gitdir().join("rebase-apply");

    if args.abort {
        let _ = std::fs::remove_dir_all(&am_dir);
        return Ok(0);
    }
    if args.cont || args.skip {
        // Pick up where we left off — bump the patch index.
        let next_path = am_dir.join("next");
        let mut next: u32 = std::fs::read_to_string(&next_path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(1);
        if args.skip {
            next += 1;
        }
        std::fs::write(&next_path, format!("{next}\n"))?;
        return resume(&repo, &am_dir, args.signoff);
    }

    let mut input = Vec::new();
    if args.files.is_empty() {
        io::stdin().read_to_end(&mut input)?;
    } else {
        for f in &args.files {
            input.extend(std::fs::read(f)?);
        }
    }
    let messages = crate::cli::mailsplit::split_mbox(&input);
    std::fs::create_dir_all(&am_dir)?;
    for (i, msg) in messages.iter().enumerate() {
        let path = am_dir.join(format!("{:04}", i + 1));
        std::fs::write(path, msg)?;
    }
    std::fs::write(am_dir.join("next"), "1\n")?;
    std::fs::write(am_dir.join("last"), format!("{}\n", messages.len()))?;

    resume(&repo, &am_dir, args.signoff)
}

fn resume(
    repo: &crate::repo::Repository,
    am_dir: &std::path::Path,
    signoff: bool,
) -> io::Result<i32> {
    let last: u32 = std::fs::read_to_string(am_dir.join("last"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    loop {
        let next: u32 = std::fs::read_to_string(am_dir.join("next"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        if next > last {
            let _ = std::fs::remove_dir_all(am_dir);
            return Ok(0);
        }
        let msg_path = am_dir.join(format!("{next:04}"));
        let raw = match std::fs::read_to_string(&msg_path) {
            Ok(s) => s,
            Err(_) => return Ok(0),
        };
        let parsed = crate::cli::mailinfo::parse_mail(&raw, false);
        // Apply the patch.
        if !parsed.patch_body.is_empty() {
            let patch_path = am_dir.join("patch");
            std::fs::write(&patch_path, &parsed.patch_body)?;
            // Run apply.
            let apply_args = crate::cli::apply::ApplyArgs {
                check: false,
                reverse: false,
                index: true,
                cached: false,
                three_way: false,
                strip: 1,
                patches: vec![patch_path.display().to_string()],
            };
            let code = crate::cli::apply::run(apply_args)?;
            if code != 0 {
                eprintln!(
                    "rustygit am: patch {next} failed to apply; resolve and run `am --continue`"
                );
                return Ok(code);
            }
        }
        // Commit with the message's author / subject.
        let commit_msg = format!(
            "{}\n\n{}{}",
            parsed.subject,
            parsed.message_body,
            if signoff {
                "\nSigned-off-by: rustygit user\n"
            } else {
                ""
            }
        );
        let commit_args = crate::cli::commit::CommitArgs {
            messages: vec![commit_msg],
            file: None,
            edit: false,
            allow_empty: false,
            gpg_sign: None,
            no_gpg_sign: false,
            no_verify: true,
        };
        let _ = crate::cli::commit::run(commit_args)?;
        let _ = repo;
        // Advance.
        std::fs::write(am_dir.join("next"), format!("{}\n", next + 1))?;
    }
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
