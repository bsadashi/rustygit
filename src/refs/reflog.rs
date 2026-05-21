//! Reflog writer.
//!
//! Reflog format (one event per line, append-only):
//!
//! ```text
//! <old-oid> <new-oid> <name> <<email>> <unix-secs> <±HHMM>\t<message>\n
//! ```
//!
//! For ref creation, `old-oid` is the all-zeros oid; for deletion, `new-oid`
//! is the all-zeros oid. M2 writes a minimal but well-formed entry; M14 will
//! refine identity sourcing (config + GIT_COMMITTER_* env override) when we
//! have a real config parser.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::hash::{HashKind, ObjectId};

use super::{FullName, RefError};

#[derive(Debug, Clone)]
pub struct Identity {
    pub name: String,
    pub email: String,
}

impl Identity {
    /// Read GIT_COMMITTER_* env vars first, then fall back to a placeholder.
    /// Real config parsing arrives in M3.
    pub fn from_env_or_placeholder() -> Self {
        let name = std::env::var("GIT_COMMITTER_NAME")
            .or_else(|_| std::env::var("GIT_AUTHOR_NAME"))
            .unwrap_or_else(|_| "rustygit".to_string());
        let email = std::env::var("GIT_COMMITTER_EMAIL")
            .or_else(|_| std::env::var("GIT_AUTHOR_EMAIL"))
            .unwrap_or_else(|_| "noreply@invalid".to_string());
        Self { name, email }
    }
}

#[derive(Debug, Clone)]
pub struct ReflogEntry<'a> {
    pub old: ObjectId,
    pub new: ObjectId,
    pub identity: &'a Identity,
    pub message: &'a str,
}

/// Append a reflog entry. Creates the parent directory and the file if needed.
pub fn append(gitdir: &Path, name: &FullName, entry: ReflogEntry<'_>) -> Result<(), RefError> {
    let path = gitdir.join("logs").join(name.loose_path_relative());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| RefError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| RefError::Io {
            path: path.clone(),
            source: e,
        })?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let offset = local_offset_minutes();
    let line = format!(
        "{old} {new} {name} <{email}> {ts} {offset}\t{msg}\n",
        old = entry.old,
        new = entry.new,
        name = entry.identity.name,
        email = entry.identity.email,
        ts = now,
        offset = format_offset(offset),
        msg = sanitize_one_line(entry.message),
    );
    f.write_all(line.as_bytes()).map_err(|e| RefError::Io {
        path: path.clone(),
        source: e,
    })?;
    Ok(())
}

/// Replace newlines/tabs in a message so a single reflog entry stays on one line.
fn sanitize_one_line(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Convenience: produce an all-zeros oid for the given hash kind.
pub fn null_oid(kind: HashKind) -> ObjectId {
    ObjectId::null(kind)
}

#[cfg(unix)]
fn local_offset_minutes() -> i32 {
    // Best-effort: shell out to `date +%z` if needed. For a self-contained
    // implementation we assume UTC if we can't tell. M14 will land a proper
    // timezone path along with the `Signature` type.
    use std::process::Command;
    if let Ok(out) = Command::new("date").arg("+%z").output() {
        if let Ok(s) = std::str::from_utf8(&out.stdout) {
            let s = s.trim();
            if s.len() == 5 {
                let sign = if s.starts_with('-') { -1 } else { 1 };
                if let (Ok(hh), Ok(mm)) = (s[1..3].parse::<i32>(), s[3..5].parse::<i32>()) {
                    return sign * (hh * 60 + mm);
                }
            }
        }
    }
    0
}

#[cfg(not(unix))]
fn local_offset_minutes() -> i32 {
    0
}

fn format_offset(min: i32) -> String {
    let sign = if min < 0 { '-' } else { '+' };
    let abs = min.unsigned_abs();
    let hh = abs / 60;
    let mm = abs % 60;
    format!("{sign}{hh:02}{mm:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn append_creates_log_file_with_one_line() {
        let dir = tempdir().unwrap();
        let name = FullName::new("refs/heads/main").unwrap();
        let id = Identity {
            name: "Test".into(),
            email: "t@example.com".into(),
        };
        let zero = null_oid(HashKind::Sha1);
        let oid = ObjectId::parse_hex(HashKind::Sha1, "abcdef0123456789abcdef0123456789abcdef01")
            .unwrap();
        append(
            dir.path(),
            &name,
            ReflogEntry {
                old: zero,
                new: oid,
                identity: &id,
                message: "branch: created from HEAD",
            },
        )
        .unwrap();
        let log = std::fs::read_to_string(dir.path().join("logs/refs/heads/main")).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("Test <t@example.com>"));
        assert!(lines[0].ends_with("branch: created from HEAD"));
    }

    #[test]
    fn format_offset_examples() {
        assert_eq!(format_offset(0), "+0000");
        assert_eq!(format_offset(60), "+0100");
        assert_eq!(format_offset(-330), "-0530");
    }
}
