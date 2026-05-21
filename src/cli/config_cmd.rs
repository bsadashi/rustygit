//! `rustygit config` — read and write config values.
//!
//! Subset: --get, --set, --unset, --list (default), --add. Scopes:
//! --local (.git/config, default), --global (~/.gitconfig), --system
//! is currently treated like global (we don't ship a /etc path).
//!
//! Section/key parsing: `section.name` or `section.subsection.name`.

use std::io::{self, Write};
use std::path::PathBuf;

use clap::Args;

use crate::config::Config;
use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// Get the value of the named key (`section.[subsection.]name`).
    #[arg(long = "get")]
    pub get: bool,
    /// Replace the value of the named key.
    #[arg(long = "set", num_args = 2, value_names = ["KEY", "VALUE"])]
    pub set: Option<Vec<String>>,
    /// Remove the named key.
    #[arg(long = "unset")]
    pub unset: bool,
    /// Add a new value (append-like behavior; we only have set today).
    #[arg(long = "add", num_args = 2, value_names = ["KEY", "VALUE"])]
    pub add: Option<Vec<String>>,
    /// List every key/value pair.
    #[arg(short = 'l', long = "list")]
    pub list: bool,
    /// Operate on .git/config (the default).
    #[arg(long = "local")]
    pub local: bool,
    /// Operate on ~/.gitconfig.
    #[arg(long = "global", conflicts_with = "local")]
    pub global: bool,
    /// Positional KEY (for --get / --unset).
    #[arg(value_name = "KEY")]
    pub key: Option<String>,
    /// Positional VALUE (when no flag is passed: shorthand for --set).
    #[arg(value_name = "VALUE")]
    pub value: Option<String>,
}

pub fn run(args: ConfigArgs) -> io::Result<i32> {
    let path = config_path(args.global)?;

    if args.list
        || (!args.get
            && args.set.is_none()
            && !args.unset
            && args.add.is_none()
            && args.key.is_none())
    {
        return list(&path);
    }

    // Shorthand: `config <KEY> <VALUE>` = set; `config <KEY>` = get.
    let (op_set, op_get, key, set_value): (bool, bool, Option<String>, Option<String>) =
        if let Some(kv) = args.set.as_ref() {
            (true, false, Some(kv[0].clone()), Some(kv[1].clone()))
        } else if let Some(kv) = args.add.as_ref() {
            (true, false, Some(kv[0].clone()), Some(kv[1].clone()))
        } else if args.unset {
            (false, false, args.key.clone(), None)
        } else if args.get {
            (false, true, args.key.clone(), None)
        } else if let (Some(k), Some(v)) = (args.key.as_ref(), args.value.as_ref()) {
            (true, false, Some(k.clone()), Some(v.clone()))
        } else if let Some(k) = args.key.as_ref() {
            (false, true, Some(k.clone()), None)
        } else {
            (false, false, None, None)
        };

    let key = match key {
        Some(k) => k,
        None => {
            eprintln!("rustygit: config: missing key");
            return Ok(129);
        }
    };
    let parsed = parse_key(&key)?;

    if op_set {
        let v = set_value.unwrap();
        return set_value_in_file(&path, &parsed, &v);
    }
    if args.unset {
        return unset_in_file(&path, &parsed);
    }
    if op_get {
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let cfg = Config::parse_str(&text).map_err(io_err)?;
        let value = match parsed.subsection.as_deref() {
            Some(sub) => cfg.get_string_sub(&parsed.section, sub, &parsed.name),
            None => cfg.get_string(&parsed.section, &parsed.name),
        };
        match value {
            Some(v) => {
                println!("{v}");
                Ok(0)
            }
            None => Ok(1),
        }
    } else {
        Ok(0)
    }
}

fn list(path: &PathBuf) -> io::Result<i32> {
    let bytes = match std::fs::read_to_string(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    // We don't have a built-in iteration API on Config that preserves
    // ordering, so re-parse the file lines for the listing.
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut section: Option<(String, Option<String>)> = None;
    for raw in bytes.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let inner = &line[1..line.len() - 1].trim();
            // section "subsection" form
            if let Some(q) = inner.find('"') {
                let head = inner[..q].trim().to_string();
                let sub_end = inner.rfind('"').unwrap_or(inner.len());
                let sub = inner[q + 1..sub_end].to_string();
                section = Some((head, Some(sub)));
            } else {
                section = Some((inner.to_string(), None));
            }
            continue;
        }
        if let Some(eq) = line.find('=') {
            let k = line[..eq].trim();
            let v = line[eq + 1..].trim();
            if let Some((s, sub)) = &section {
                let key = match sub {
                    Some(sub) => format!("{s}.{sub}.{k}"),
                    None => format!("{s}.{k}"),
                };
                writeln!(out, "{key}={v}")?;
            }
        }
    }
    Ok(0)
}

struct ParsedKey {
    section: String,
    subsection: Option<String>,
    name: String,
}

fn parse_key(s: &str) -> io::Result<ParsedKey> {
    let parts: Vec<&str> = s.splitn(3, '.').collect();
    match parts.as_slice() {
        [s, n] => Ok(ParsedKey {
            section: s.to_string(),
            subsection: None,
            name: n.to_string(),
        }),
        [s, sub, n] => Ok(ParsedKey {
            section: s.to_string(),
            subsection: Some(sub.to_string()),
            name: n.to_string(),
        }),
        _ => Err(io::Error::other(format!(
            "config: invalid key {s:?}; expected section.name or section.subsection.name"
        ))),
    }
}

fn config_path(global: bool) -> io::Result<PathBuf> {
    if global {
        if let Some(home) = dirs_home() {
            return Ok(home.join(".gitconfig"));
        }
        return Err(io::Error::other("config: no $HOME for --global"));
    }
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    Ok(repo.gitdir().join("config"))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn set_value_in_file(path: &PathBuf, key: &ParsedKey, value: &str) -> io::Result<i32> {
    let mut text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let header = match &key.subsection {
        Some(sub) => format!("[{section} \"{sub}\"]\n", section = key.section, sub = sub),
        None => format!("[{section}]\n", section = key.section),
    };

    // Try to find an existing matching key inside the right section.
    // We do a line-oriented rewrite for simplicity.
    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    let mut in_section = false;
    let mut replaced = false;
    for line in lines.iter_mut() {
        let trim = line.trim();
        if trim.starts_with('[') && trim.ends_with(']') {
            in_section = trim == header.trim();
            continue;
        }
        if in_section {
            if let Some(eq) = trim.find('=') {
                let k = trim[..eq].trim();
                if k == key.name {
                    *line = format!("\t{name} = {value}", name = key.name);
                    replaced = true;
                    break;
                }
            }
        }
    }
    if replaced {
        text = lines.join("\n");
        if !text.ends_with('\n') {
            text.push('\n');
        }
    } else {
        // Append a new section if not already present, then the key.
        if !text.contains(&header) {
            if !text.ends_with('\n') && !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&header);
        }
        // Find the section header and insert immediately after.
        let mut new_lines: Vec<String> = Vec::with_capacity(lines.len() + 1);
        let mut inserted = false;
        for line in text.lines() {
            new_lines.push(line.to_string());
            if !inserted && line.trim() == header.trim() {
                new_lines.push(format!("\t{name} = {value}", name = key.name));
                inserted = true;
            }
        }
        text = new_lines.join("\n");
        if !text.ends_with('\n') {
            text.push('\n');
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)?;
    Ok(0)
}

fn unset_in_file(path: &PathBuf, key: &ParsedKey) -> io::Result<i32> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let header = match &key.subsection {
        Some(sub) => format!("[{section} \"{sub}\"]", section = key.section, sub = sub),
        None => format!("[{section}]", section = key.section),
    };
    let mut out_lines: Vec<String> = Vec::new();
    let mut in_section = false;
    for raw in text.lines() {
        let trim = raw.trim();
        if trim.starts_with('[') && trim.ends_with(']') {
            in_section = trim == header;
            out_lines.push(raw.to_string());
            continue;
        }
        if in_section {
            if let Some(eq) = trim.find('=') {
                let k = trim[..eq].trim();
                if k == key.name {
                    continue; // drop this line
                }
            }
        }
        out_lines.push(raw.to_string());
    }
    let mut new_text = out_lines.join("\n");
    if !new_text.ends_with('\n') {
        new_text.push('\n');
    }
    std::fs::write(path, new_text)?;
    Ok(0)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
