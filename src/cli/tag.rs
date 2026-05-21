//! `rustygit tag` — create, list, or delete tags.
//!
//! Supported forms:
//! * `tag`                              — list (alias: `tag -l`)
//! * `tag <name>`                       — lightweight tag at HEAD
//! * `tag <name> <commit-ish>`          — lightweight tag at <commit-ish>
//! * `tag -a -m <msg> <name> [<commit-ish>]` — annotated tag (creates a tag object)
//! * `tag -m <msg> <name> [<commit-ish>]`    — `-m` implies `-a`
//! * `tag -d <name>...`                 — delete one or more tags
//! * `tag -l [<pattern>]`               — list, optional shell glob
//! * `tag -f <name> [<commit-ish>]`     — force overwrite an existing tag
//!
//! NOT yet implemented (documented in COMPAT.md as deferred):
//! * Editor flow when `-a` is given without `-m`
//! * `-s` (sign), `-u <keyid>` (sign with specific key)
//! * `--contains`, `--no-contains`, `--merged`, `--no-merged`, `--points-at`
//! * `--sort=<key>`

use std::io::{self, Write};

use clap::Args;

use crate::config::Config;
use crate::hash::ObjectId;
use crate::identity::{Signature, Time};
use crate::object::ObjectKind;
use crate::refs::{ExpectedOldValue, FullName, NewValue, ReflogMessage};
use crate::repo::Repository;
use crate::revparse;
use crate::signing::{GpgSigner, Signer};
use crate::tag::Tag;
use crate::wildmatch::wildmatch;

#[derive(Debug, Args)]
pub struct TagArgs {
    /// Create an annotated tag.
    #[arg(short = 'a')]
    pub annotated: bool,
    /// Create a PGP-signed annotated tag (implies `-a`). Uses `gpg.program`
    /// and `user.signingkey` from config.
    #[arg(short = 's', long = "sign")]
    pub sign: bool,
    /// Force creation even if the tag already exists.
    #[arg(short = 'f', long = "force")]
    pub force: bool,
    /// Delete the given tag(s).
    #[arg(short = 'd', long = "delete")]
    pub delete: bool,
    /// List tags, optionally filtered by glob. The default action when
    /// no positional arguments are given is `--list`.
    #[arg(short = 'l', long = "list")]
    pub list: bool,
    /// Annotated-tag message. Implies `-a`. May be repeated; messages
    /// are joined with a blank line.
    #[arg(short = 'm', long = "message")]
    pub message: Vec<String>,
    /// Positional arguments. Meaning depends on the mode:
    /// * list  → optional glob patterns
    /// * create → `<name>` then optional `<commit-ish>`
    /// * delete → one or more `<name>`s
    #[arg(value_name = "ARG")]
    pub args: Vec<String>,
}

pub fn run(args: TagArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    if args.delete {
        return delete(&repo, &args.args);
    }

    let make_annotated = args.annotated || args.sign || !args.message.is_empty();
    let positional_create = !args.args.is_empty() && !args.list;

    if !positional_create {
        // Bare `tag` or `tag -l [pattern...]`.
        return list(&repo, &args.args);
    }

    create(&repo, &args, make_annotated)
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn list(repo: &Repository, patterns: &[String]) -> io::Result<i32> {
    let mut names: Vec<String> = Vec::new();
    for r in repo.refs().iter(Some("refs/tags/")) {
        let r = r.map_err(io_err)?;
        if let Some(name) = r.name.as_str().strip_prefix("refs/tags/") {
            names.push(name.to_owned());
        }
    }
    names.sort();

    let stdout = io::stdout();
    let mut out = stdout.lock();
    for n in &names {
        let keep = patterns.is_empty()
            || patterns
                .iter()
                .any(|p| wildmatch(p.as_bytes(), n.as_bytes(), 0));
        if keep {
            writeln!(out, "{n}")?;
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// delete
// ---------------------------------------------------------------------------

fn delete(repo: &Repository, names: &[String]) -> io::Result<i32> {
    if names.is_empty() {
        eprintln!("rustygit: tag: -d requires a tag name");
        return Ok(129);
    }
    let mut had_error = false;
    for name in names {
        let full = match FullName::new(format!("refs/tags/{name}")) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("rustygit: tag: {e}");
                had_error = true;
                continue;
            }
        };
        let existing = repo.refs().read(&full).map_err(io_err)?;
        let oid = match existing {
            Some(r) => match r.target {
                crate::refs::RefTarget::Direct(o) => o,
                crate::refs::RefTarget::Symbolic(_) => {
                    eprintln!("rustygit: tag '{name}' is symbolic; cannot delete");
                    had_error = true;
                    continue;
                }
            },
            None => {
                eprintln!("rustygit: tag '{name}' not found.");
                had_error = true;
                continue;
            }
        };
        let mut tx = repo.refs().transaction();
        tx.delete(&full, ExpectedOldValue::Direct(oid))
            .map_err(io_err)?;
        tx.commit().map_err(io_err)?;
        println!("Deleted tag '{name}' (was {}).", oid.short_hex(7));
    }
    Ok(if had_error { 1 } else { 0 })
}

// ---------------------------------------------------------------------------
// create
// ---------------------------------------------------------------------------

fn create(repo: &Repository, args: &TagArgs, annotated: bool) -> io::Result<i32> {
    let name = &args.args[0];
    let start_rev = args.args.get(1).map(String::as_str).unwrap_or("HEAD");

    if args.args.len() > 2 {
        eprintln!("rustygit: tag: too many arguments");
        return Ok(129);
    }

    let target_oid = match revparse::resolve(repo.refs(), repo.odb(), start_rev) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("rustygit: tag: {e}");
            return Ok(128);
        }
    };
    let target_kind = read_object_kind(repo, target_oid)?;

    let full = match FullName::new(format!("refs/tags/{name}")) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("rustygit: tag: {e}");
            return Ok(128);
        }
    };

    let ref_target_oid = if annotated {
        if args.message.is_empty() {
            eprintln!(
                "rustygit: tag: -a/-m required for annotated tags; editor flow not yet supported"
            );
            return Ok(128);
        }
        let message = build_message(&args.message);
        let config = Config::from_repo_dir(repo.gitdir()).map_err(io_err)?;
        let tagger =
            Signature::committer_from_env_or_config(&config, Time::now_local()).map_err(io_err)?;
        let mut tag = Tag::new(
            target_oid,
            target_kind,
            name.as_bytes().to_vec(),
            tagger,
            message,
        );

        if args.sign {
            // Sign the unsigned tag body, then append the armored PGP
            // block to the message. The final stored bytes embed the
            // signature in the message body (NOT as a header — matches
            // upstream git's signed-tag format).
            let unsigned_payload = tag.serialize();
            let signer = GpgSigner::from_config(&config);
            let sig_bytes = signer.sign(&unsigned_payload).map_err(io_err)?;
            // Ensure the message ends with a newline before appending the
            // PGP block, so the BEGIN line starts on its own line.
            if !tag.message.ends_with(b"\n") {
                tag.message.push(b'\n');
            }
            tag.message.extend_from_slice(&sig_bytes);
            // gpg --detach-sign --armor emits a trailing newline after
            // -----END PGP SIGNATURE-----, but be defensive in case
            // some gpg builds don't.
            if !tag.message.ends_with(b"\n") {
                tag.message.push(b'\n');
            }
        }

        let raw = tag.to_object();
        repo.odb().write(&raw).map_err(io_err)?
    } else {
        target_oid
    };

    let expected = if args.force {
        ExpectedOldValue::Any
    } else {
        ExpectedOldValue::Missing
    };
    let reflog_msg = if annotated {
        format!("tag: annotated {name}")
    } else {
        format!("tag: lightweight {name}")
    };
    let mut tx = repo.refs().transaction();
    tx.update(
        &full,
        expected,
        NewValue::Direct(ref_target_oid),
        ReflogMessage::from(reflog_msg),
    )
    .map_err(io_err)?;
    match tx.commit() {
        Ok(()) => Ok(0),
        Err(crate::refs::RefError::Update(crate::refs::RefUpdateError::ExpectedMissing(_))) => {
            eprintln!("rustygit: tag '{name}' already exists");
            Ok(128)
        }
        Err(e) => Err(io_err(e)),
    }
}

fn read_object_kind(repo: &Repository, oid: ObjectId) -> io::Result<ObjectKind> {
    let (kind, _size) = repo.odb().read_header(&oid).map_err(io_err)?;
    Ok(kind)
}

fn build_message(parts: &[String]) -> Vec<u8> {
    let mut joined = String::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            joined.push_str("\n\n");
        }
        joined.push_str(p);
    }
    let mut bytes = joined.into_bytes();
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    bytes
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct Wrap {
        #[command(flatten)]
        args: TagArgs,
    }

    #[test]
    fn parses_bare() {
        let w = Wrap::try_parse_from(["test"]).unwrap();
        assert!(!w.args.annotated);
        assert!(!w.args.delete);
        assert!(!w.args.list);
        assert!(w.args.args.is_empty());
    }

    #[test]
    fn parses_lightweight_create() {
        let w = Wrap::try_parse_from(["test", "v1.0"]).unwrap();
        assert_eq!(w.args.args, vec!["v1.0".to_string()]);
        assert!(!w.args.annotated);
    }

    #[test]
    fn parses_annotated_with_message() {
        let w = Wrap::try_parse_from(["test", "-a", "-m", "the message", "v1.0"]).unwrap();
        assert!(w.args.annotated);
        assert_eq!(w.args.message, vec!["the message".to_string()]);
        assert_eq!(w.args.args, vec!["v1.0".to_string()]);
    }

    #[test]
    fn parses_delete() {
        let w = Wrap::try_parse_from(["test", "-d", "v1.0"]).unwrap();
        assert!(w.args.delete);
        assert_eq!(w.args.args, vec!["v1.0".to_string()]);
    }

    #[test]
    fn parses_force_overwrite() {
        let w = Wrap::try_parse_from(["test", "-f", "v1.0", "deadbeef"]).unwrap();
        assert!(w.args.force);
        assert_eq!(
            w.args.args,
            vec!["v1.0".to_string(), "deadbeef".to_string()]
        );
    }

    #[test]
    fn parses_list_with_pattern() {
        let w = Wrap::try_parse_from(["test", "-l", "v1.*"]).unwrap();
        assert!(w.args.list);
        assert_eq!(w.args.args, vec!["v1.*".to_string()]);
    }

    #[test]
    fn build_message_single() {
        assert_eq!(build_message(&["hi".into()]), b"hi\n");
    }

    #[test]
    fn build_message_multiple_joined_with_blank() {
        assert_eq!(
            build_message(&["one".into(), "two".into()]),
            b"one\n\ntwo\n"
        );
    }

    #[test]
    fn build_message_already_has_trailing_nl() {
        assert_eq!(build_message(&["hi\n".into()]), b"hi\n");
    }
}
