//! `rustygit check-attr` — print the gitattributes value for each
//! (attribute, path) combination.
//!
//! Subset: we ship the parser for `.gitattributes` at repo root and
//! handle the common shape `<pattern> <attr1> <attr2>...` where each
//! attribute is `name`, `-name`, `name=value`, or `!name`.
//!
//! Output (per path × attribute):
//!   `<path>: <attr>: <set|unset|unspecified|value>`
//!
//! With `--all`, prints every attribute set on a path (not just those
//! named in argv).

use std::io::{self, Write};

use clap::Args;

use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct CheckAttrArgs {
    /// Print every attribute set on the path.
    #[arg(long = "all")]
    pub all: bool,
    /// NUL-terminate output records.
    #[arg(short = 'z')]
    pub nul_terminate: bool,
    /// `<attr>... -- <path>...` or `<attr> <path>` — by convention the
    /// last non-flag argument is the path. We accept any mix and treat
    /// the trailing argv as paths once we see `--`. Otherwise the first
    /// arg is the attribute name and the rest are paths.
    #[arg(value_name = "ARG", trailing_var_arg = true)]
    pub args: Vec<String>,
}

pub fn run(args: CheckAttrArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(|e| io::Error::other(format!("{e}")))?;
    let attributes = AttrFile::read_root(&repo)?;
    let term = if args.nul_terminate { 0u8 } else { b'\n' };

    let (attrs, paths) = split_args(&args.args)?;
    if paths.is_empty() {
        eprintln!("rustygit: check-attr: missing <path> argument");
        return Ok(129);
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    for path in &paths {
        let resolved = attributes.classify(path);
        if args.all {
            for (name, value) in &resolved {
                write!(out, "{path}: {name}: {}", display_value(value))?;
                out.write_all(std::slice::from_ref(&term))?;
            }
        } else {
            for name in &attrs {
                let v = resolved
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v.clone())
                    .unwrap_or(AttrValue::Unspecified);
                write!(out, "{path}: {name}: {}", display_value(&v))?;
                out.write_all(std::slice::from_ref(&term))?;
            }
        }
    }
    Ok(0)
}

fn split_args(argv: &[String]) -> io::Result<(Vec<String>, Vec<String>)> {
    if let Some(idx) = argv.iter().position(|s| s == "--") {
        let attrs = argv[..idx].to_vec();
        let paths = argv[idx + 1..].to_vec();
        return Ok((attrs, paths));
    }
    // Without `--`: first arg = attribute, rest = paths.
    if argv.is_empty() {
        return Ok((vec![], vec![]));
    }
    Ok((vec![argv[0].clone()], argv[1..].to_vec()))
}

#[derive(Debug, Clone)]
enum AttrValue {
    Set,
    Unset,
    Unspecified,
    Value(String),
}

fn display_value(v: &AttrValue) -> &str {
    match v {
        AttrValue::Set => "set",
        AttrValue::Unset => "unset",
        AttrValue::Unspecified => "unspecified",
        AttrValue::Value(s) => s,
    }
}

#[derive(Debug, Default)]
struct AttrFile {
    rules: Vec<(String, Vec<(String, AttrValue)>)>,
}

impl AttrFile {
    fn read_root(repo: &Repository) -> io::Result<Self> {
        let path = repo.workdir().join(".gitattributes");
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        let text = String::from_utf8_lossy(&bytes);
        let mut rules = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let pat = match parts.next() {
                Some(p) => p.to_string(),
                None => continue,
            };
            let mut attrs = Vec::new();
            for token in parts {
                if let Some(eq) = token.find('=') {
                    let name = &token[..eq];
                    let value = &token[eq + 1..];
                    attrs.push((name.to_string(), AttrValue::Value(value.to_string())));
                } else if let Some(rest) = token.strip_prefix('-') {
                    attrs.push((rest.to_string(), AttrValue::Unset));
                } else if let Some(rest) = token.strip_prefix('!') {
                    attrs.push((rest.to_string(), AttrValue::Unspecified));
                } else {
                    attrs.push((token.to_string(), AttrValue::Set));
                }
            }
            rules.push((pat, attrs));
        }
        Ok(Self { rules })
    }

    /// Return the effective attribute map for `path` (last-match-wins
    /// per attribute name).
    fn classify(&self, path: &str) -> Vec<(String, AttrValue)> {
        let mut effective: Vec<(String, AttrValue)> = Vec::new();
        for (pat, attrs) in &self.rules {
            if !pattern_matches(pat, path) {
                continue;
            }
            for (name, value) in attrs {
                if let Some(existing) = effective.iter_mut().find(|(n, _)| n == name) {
                    existing.1 = value.clone();
                } else {
                    effective.push((name.clone(), value.clone()));
                }
            }
        }
        effective
    }
}

/// Tiny pattern matcher — supports `*` and literal segments.
fn pattern_matches(pat: &str, path: &str) -> bool {
    // No glob → literal full match OR exact basename.
    if !pat.contains('*') {
        return pat == path || path.ends_with(&format!("/{pat}"));
    }
    // Use existing wildmatch.
    crate::wildmatch::wildmatch(pat.as_bytes(), path.as_bytes(), 0)
}
