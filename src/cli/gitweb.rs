//! `rustygit gitweb` — emit a static HTML snapshot of the current repo.
//!
//! Implementation: render a single-page repo browser. Sections:
//!   * Header (repo path, HEAD oid)
//!   * Refs (branches + tags)
//!   * Log (latest N commits)
//!   * Worktree-root listing
//!
//! Output goes to `<output>/index.html` (default `gitweb.html`).
//!
//! `git-instaweb` extends this by also starting a local HTTP server.

use std::io::{self, Write};

use clap::Args;

use crate::commit::Commit;
use crate::refs::RefTarget;
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct GitwebArgs {
    /// Output HTML file.
    #[arg(short = 'o', long = "output", default_value = "gitweb.html")]
    pub output: String,
    /// How many commits to render in the log section.
    #[arg(short = 'n', long = "count", default_value_t = 20)]
    pub count: usize,
}

pub fn run(args: GitwebArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let mut html = String::new();
    html.push_str("<!doctype html><html><head><meta charset=\"utf-8\"><title>gitweb</title>");
    html.push_str("<style>body{font-family:monospace;margin:2em}h2{border-bottom:1px solid #ccc}.oid{color:#888}</style>");
    html.push_str("</head><body>");
    html.push_str(&format!(
        "<h1>{} <span class=oid>(rustygit gitweb)</span></h1>",
        html_escape(repo.workdir().display().to_string().as_str())
    ));

    html.push_str("<h2>Refs</h2><ul>");
    for r in repo.refs().iter(None) {
        let r = r.map_err(io_err)?;
        let name = r.name.as_str();
        if name == "HEAD" {
            continue;
        }
        let target = match r.target {
            RefTarget::Direct(o) => o.to_string(),
            RefTarget::Symbolic(s) => format!("→ {}", s.as_str()),
        };
        html.push_str(&format!(
            "<li>{} <span class=oid>{}</span></li>",
            html_escape(name),
            html_escape(&target)
        ));
    }
    html.push_str("</ul>");

    html.push_str(&format!("<h2>Log (latest {})</h2><ol>", args.count));
    if let Ok(head) = crate::revparse::resolve(repo.refs(), repo.odb(), "HEAD") {
        let mut cur = head;
        for _ in 0..args.count {
            let raw = match repo.odb().read(&cur) {
                Ok(r) => r,
                Err(_) => break,
            };
            let commit = match Commit::parse(&raw.data, repo.hash_kind()) {
                Ok(c) => c,
                Err(_) => break,
            };
            let subject = String::from_utf8_lossy(&commit.message)
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
            html.push_str(&format!(
                "<li><code>{}</code> {} <span class=oid>by {}</span></li>",
                cur.short_hex(7),
                html_escape(&subject),
                html_escape(&commit.author.name)
            ));
            cur = match commit.parents.first() {
                Some(p) => *p,
                None => break,
            };
        }
    }
    html.push_str("</ol>");

    html.push_str("<h2>Worktree</h2><ul>");
    if let Ok(entries) = std::fs::read_dir(repo.workdir()) {
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| n != ".git")
            .collect();
        names.sort();
        for n in names {
            html.push_str(&format!("<li>{}</li>", html_escape(&n)));
        }
    }
    html.push_str("</ul></body></html>");

    std::fs::write(&args.output, &html)?;
    let stdout = io::stdout();
    writeln!(
        stdout.lock(),
        "rustygit gitweb: wrote {} bytes to {}",
        html.len(),
        args.output
    )?;
    Ok(0)
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}

// ---------------------------------------------------------------------------
// gitk / git-gui — text-mode equivalents
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct GitkArgs {
    /// Max commits to show.
    #[arg(short = 'n', default_value_t = 50)]
    pub count: usize,
}

pub fn run_gitk(args: GitkArgs) -> io::Result<i32> {
    // TUI replacement: emit a one-line-per-commit chronological listing.
    let log = crate::cli::log::LogArgs {
        max: Some(args.count),
        oneline: true,
        abbrev: None,
        abbrev_commit: false,
        patch: false,
        grep: None,
        author: None,
        committer: None,
        start: "HEAD".to_string(),
    };
    crate::cli::log::run(log)
}

#[derive(Debug, Args)]
pub struct GitGuiArgs {}

pub fn run_git_gui(_args: GitGuiArgs) -> io::Result<i32> {
    // TUI commit pane equivalent: print status + run interactive commit.
    let status = crate::cli::status::StatusArgs {
        porcelain: false,
        short: false,
    };
    let _ = crate::cli::status::run(status)?;
    println!(
        "\nrustygit git-gui: this is the text-mode equivalent of the Tk commit GUI.\n\
         Use `rustygit commit` to commit the staged changes, or `rustygit add -p` to stage interactively."
    );
    Ok(0)
}

#[derive(Debug, Args)]
pub struct InstawebArgs {
    /// HTTP port to listen on.
    #[arg(long = "port", default_value_t = 1234)]
    pub port: u16,
    /// Run as `--start` (default), `--stop`, or `--restart`.
    #[arg(long = "start", default_value_t = true)]
    pub start: bool,
}

pub fn run_instaweb(args: InstawebArgs) -> io::Result<i32> {
    let out_path = std::env::temp_dir().join(format!("rustygit-instaweb-{}.html", args.port));
    let _ = run(GitwebArgs {
        output: out_path.display().to_string(),
        count: 50,
    })?;
    println!(
        "rustygit instaweb: wrote {} ({} bytes)\n\
         Open the file directly in your browser. Built-in HTTP serving is deferred.",
        out_path.display(),
        out_path.metadata().map(|m| m.len()).unwrap_or(0)
    );
    Ok(0)
}
