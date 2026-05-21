//! Attribute filters: smudge / clean / textconv.
//!
//! When a path has `filter=<name>` set in `.gitattributes`, the
//! configured external programs are run during checkout (smudge) and
//! during `add`/`commit` (clean). This module exposes the hooks that
//! `cli/add.rs` and `cli/checkout.rs` can call.

use std::io::{self, Read, Write};
use std::process::{Command, Stdio};

use crate::config::Config;

/// Look up the filter driver for an attribute name, if any.
/// Returns `(clean_cmd, smudge_cmd)` strings or `(None, None)`.
pub fn filter_driver(config: &Config, name: &str) -> (Option<String>, Option<String>) {
    let clean = config
        .get_string_sub("filter", name, "clean")
        .map(str::to_string);
    let smudge = config
        .get_string_sub("filter", name, "smudge")
        .map(str::to_string);
    (clean, smudge)
}

/// Run a clean filter program with `payload` on stdin; return the
/// transformed bytes from stdout.
pub fn run_clean(cmd: &str, payload: &[u8]) -> io::Result<Vec<u8>> {
    pipe_through(cmd, payload)
}

/// Run a smudge filter program with `payload` on stdin; return the
/// transformed bytes from stdout.
pub fn run_smudge(cmd: &str, payload: &[u8]) -> io::Result<Vec<u8>> {
    pipe_through(cmd, payload)
}

/// Run a textconv program over `payload` and return its stdout.
/// Used by `diff` to render non-text blobs (PDFs, etc.) as text.
pub fn run_textconv(cmd: &str, payload: &[u8]) -> io::Result<Vec<u8>> {
    pipe_through(cmd, payload)
}

fn pipe_through(cmd: &str, input: &[u8]) -> io::Result<Vec<u8>> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(input)?;
    }
    let mut out = Vec::new();
    if let Some(stdout) = child.stdout.as_mut() {
        stdout.read_to_end(&mut out)?;
    }
    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "filter '{cmd}' exited non-zero ({:?})",
            status.code()
        )));
    }
    Ok(out)
}
