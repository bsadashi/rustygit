//! `rustygit format-patch` — emit commits as mail-formatted patches.
//!
//! Subset:
//!   * `<since>..<head>` produces one file per commit, named
//!     `<idx>-<subject-slug>.patch`.
//!   * `-1` / `--root` / `-N` shorthand also supported.
//!   * `--stdout` streams to stdout instead.

use std::io::{self, Write};

use clap::Args;

use crate::commit::Commit;
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct FormatPatchArgs {
    /// Output directory.
    #[arg(short = 'o', long = "output-directory", default_value = ".")]
    pub output_dir: String,
    /// Emit all to stdout (concatenated).
    #[arg(long = "stdout")]
    pub stdout: bool,
    /// Limit count.
    #[arg(short = 'n', long = "max-count")]
    pub max: Option<usize>,
    /// Cover-letter file for the series.
    #[arg(long = "cover-letter")]
    pub cover_letter: bool,
    /// Range, e.g. `main..feature`.
    #[arg(value_name = "RANGE", required = true)]
    pub range: String,
}

pub fn run(args: FormatPatchArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let commits = match crate::revparse::resolve_range(repo.refs(), repo.odb(), &args.range) {
        Ok(Some(v)) => v,
        Ok(None) => {
            let single =
                crate::revparse::resolve(repo.refs(), repo.odb(), &args.range).map_err(io_err)?;
            vec![single]
        }
        Err(e) => return Err(io::Error::other(format!("{e}"))),
    };
    let mut commits = commits;
    commits.reverse();
    if let Some(max) = args.max {
        commits.truncate(max);
    }

    if !args.stdout {
        std::fs::create_dir_all(&args.output_dir)?;
    }
    if args.cover_letter {
        let cover_path = std::path::Path::new(&args.output_dir).join("0000-cover-letter.patch");
        let body = format!(
            "From: rustygit\n\
             Subject: [PATCH 0/{N}] *** SUBJECT HERE ***\n\
             \n\
             *** BLURB HERE ***\n\
             \n\
             {N} patches.\n",
            N = commits.len()
        );
        std::fs::write(cover_path, body)?;
    }
    for (i, oid) in commits.iter().enumerate() {
        let raw = repo.odb().read(oid).map_err(io_err)?;
        let commit = Commit::parse(&raw.data, repo.hash_kind()).map_err(io_err)?;
        let subject = first_line(&commit.message);
        let slug = slugify(&subject);
        let filename = format!("{:04}-{slug}.patch", i + 1);
        let mut content = Vec::new();
        writeln!(content, "From {oid} Mon Sep 17 00:00:00 2001")?;
        writeln!(
            content,
            "From: {} <{}>",
            commit.author.name, commit.author.email
        )?;
        writeln!(content, "Date: {}", commit.author.when.serialize())?;
        writeln!(
            content,
            "Subject: [PATCH {}/{}] {subject}",
            i + 1,
            commits.len()
        )?;
        writeln!(content)?;
        let body = String::from_utf8_lossy(&commit.message);
        let body_lines: Vec<&str> = body.lines().skip(1).collect();
        let mut started = false;
        for line in body_lines {
            if !started && line.trim().is_empty() {
                continue;
            }
            started = true;
            writeln!(content, "{line}")?;
        }
        writeln!(content, "---")?;
        // Body diff.
        let parent = commit.parents.first().copied();
        let parent_tree = match parent {
            Some(p) => {
                let praw = repo.odb().read(&p).map_err(io_err)?;
                let pc = Commit::parse(&praw.data, repo.hash_kind()).map_err(io_err)?;
                pc.tree
            }
            None => crate::hash::ObjectId::parse_hex(
                crate::hash::HashKind::Sha1,
                "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
            )
            .unwrap(),
        };
        crate::diff::diff_two_trees(&repo, parent_tree, commit.tree, &mut content)
            .map_err(io_err)?;
        writeln!(content, "-- ")?;
        writeln!(content, "rustygit")?;
        if args.stdout {
            let stdout = io::stdout();
            stdout.lock().write_all(&content)?;
        } else {
            let path = std::path::Path::new(&args.output_dir).join(&filename);
            std::fs::write(&path, &content)?;
            println!("{}", path.display());
        }
    }
    Ok(0)
}

fn first_line(msg: &[u8]) -> String {
    let s = String::from_utf8_lossy(msg);
    s.lines().next().unwrap_or("").to_string()
}

fn slugify(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(50)
        .collect()
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
