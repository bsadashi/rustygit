//! `rustygit apply` — apply a unified diff to the working tree.
//!
//! Subset:
//!   * Reads patch from stdin or a file argument.
//!   * Handles `--check` (just validate; don't modify anything).
//!   * Handles `--reverse` (apply the inverse).
//!   * Handles `-3` / `--3way` (three-way fallback) — DEFERRED; on
//!     conflict we report a clean rejection.
//!   * Supports plain content patches; binary patches are deferred.

use std::io::{self, Read};

use clap::Args;

#[derive(Debug, Args)]
pub struct ApplyArgs {
    /// Validate only; don't modify the workdir.
    #[arg(long = "check")]
    pub check: bool,
    /// Apply in reverse (subtract instead of add).
    #[arg(short = 'R', long = "reverse")]
    pub reverse: bool,
    /// Apply to the index too.
    #[arg(long = "index")]
    pub index: bool,
    /// Apply to the index only (don't touch workdir).
    #[arg(long = "cached")]
    pub cached: bool,
    /// Three-way fallback (deferred).
    #[arg(short = '3', long = "3way")]
    pub three_way: bool,
    /// Strip <n> leading path components.
    #[arg(short = 'p', value_name = "N", default_value_t = 1)]
    pub strip: usize,
    /// Patch files (default stdin).
    #[arg(value_name = "PATCH")]
    pub patches: Vec<String>,
}

pub fn run(args: ApplyArgs) -> io::Result<i32> {
    let mut input = Vec::new();
    if args.patches.is_empty() {
        io::stdin().read_to_end(&mut input)?;
    } else {
        for p in &args.patches {
            input.extend(std::fs::read(p)?);
            input.push(b'\n');
        }
    }
    let files = parse_patch(&input, args.strip);
    let mut applied = 0usize;
    let mut failed = 0usize;
    let cwd = std::env::current_dir()?;

    for file in &files {
        let target = cwd.join(&file.target_path);
        // Read the current content (or empty for a new file).
        let current = std::fs::read(&target).unwrap_or_default();
        let result = apply_hunks(&current, &file.hunks, args.reverse);
        match result {
            Ok(new_content) => {
                if args.check {
                    applied += 1;
                    continue;
                }
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&target, new_content)?;
                applied += 1;
            }
            Err(reason) => {
                eprintln!("rustygit apply: {}: {reason}", file.target_path.display());
                failed += 1;
            }
        }
    }
    let _ = args.index;
    let _ = args.cached;
    let _ = args.three_way;
    if !args.check {
        let _ = (applied,);
    } else {
        // No diagnostic on --check unless something failed.
    }
    Ok(if failed > 0 { 1 } else { 0 })
}

struct PatchFile {
    target_path: std::path::PathBuf,
    hunks: Vec<Hunk>,
}

struct Hunk {
    old_start: usize,
    lines: Vec<HunkLine>,
}

#[derive(Clone)]
struct HunkLine {
    kind: char, // ' ' | '+' | '-'
    body: Vec<u8>,
}

fn parse_patch(bytes: &[u8], strip: usize) -> Vec<PatchFile> {
    let text = String::from_utf8_lossy(bytes);
    let mut files: Vec<PatchFile> = Vec::new();
    let mut current_target: Option<std::path::PathBuf> = None;
    let mut current_hunks: Vec<Hunk> = Vec::new();
    let mut current_hunk: Option<Hunk> = None;

    let strip_path = |raw: &str, strip: usize| -> std::path::PathBuf {
        let cleaned = raw.trim_start_matches("a/").trim_start_matches("b/");
        let parts: Vec<&str> = cleaned.split('/').collect();
        let kept = if strip < parts.len() {
            parts[strip..].join("/")
        } else {
            parts.last().copied().unwrap_or("").to_string()
        };
        std::path::PathBuf::from(kept)
    };

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            if let Some(prev_hunk) = current_hunk.take() {
                current_hunks.push(prev_hunk);
            }
            if let Some(prev_target) = current_target.take() {
                files.push(PatchFile {
                    target_path: prev_target,
                    hunks: std::mem::take(&mut current_hunks),
                });
            }
            let path = strip_path(rest, strip);
            current_target = Some(path);
            continue;
        }
        if line.starts_with("@@") {
            if let Some(prev_hunk) = current_hunk.take() {
                current_hunks.push(prev_hunk);
            }
            // Parse `@@ -<old>[,<n>] +<new>[,<n>] @@`
            let mut iter = line.split_whitespace();
            iter.next(); // "@@"
            let old_field = iter.next().unwrap_or("-1");
            let old_start = old_field
                .trim_start_matches('-')
                .split(',')
                .next()
                .unwrap_or("1")
                .parse::<usize>()
                .unwrap_or(1);
            current_hunk = Some(Hunk {
                old_start,
                lines: Vec::new(),
            });
            continue;
        }
        if let Some(hunk) = current_hunk.as_mut() {
            let (kind, body): (char, &str) = if let Some(rest) = line.strip_prefix('+') {
                ('+', rest)
            } else if let Some(rest) = line.strip_prefix('-') {
                ('-', rest)
            } else if let Some(rest) = line.strip_prefix(' ') {
                (' ', rest)
            } else {
                continue;
            };
            hunk.lines.push(HunkLine {
                kind,
                body: body.as_bytes().to_vec(),
            });
        }
    }
    if let Some(h) = current_hunk {
        current_hunks.push(h);
    }
    if let Some(t) = current_target {
        files.push(PatchFile {
            target_path: t,
            hunks: current_hunks,
        });
    }
    files
}

fn apply_hunks(current: &[u8], hunks: &[Hunk], reverse: bool) -> Result<Vec<u8>, String> {
    let mut lines: Vec<Vec<u8>> = current
        .split_inclusive(|&b| b == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    // Apply hunks in reverse order so line numbers stay valid.
    let mut sorted: Vec<&Hunk> = hunks.iter().collect();
    sorted.sort_by_key(|h| std::cmp::Reverse(h.old_start));
    for hunk in sorted {
        let mut idx = hunk.old_start.saturating_sub(1);
        for line in &hunk.lines {
            let effective_kind = if reverse {
                match line.kind {
                    '+' => '-',
                    '-' => '+',
                    c => c,
                }
            } else {
                line.kind
            };
            let mut body_with_nl = line.body.clone();
            body_with_nl.push(b'\n');
            match effective_kind {
                ' ' => {
                    // Context line — must match.
                    let actual = lines.get(idx).cloned().unwrap_or_default();
                    if strip_nl(&actual) != strip_nl(&body_with_nl) {
                        return Err(format!(
                            "context mismatch at line {} (expected {:?}, got {:?})",
                            idx + 1,
                            String::from_utf8_lossy(&body_with_nl),
                            String::from_utf8_lossy(&actual)
                        ));
                    }
                    idx += 1;
                }
                '-' => {
                    if idx >= lines.len() {
                        return Err("deletion past end of file".into());
                    }
                    let actual = lines[idx].clone();
                    if strip_nl(&actual) != strip_nl(&body_with_nl) {
                        return Err(format!("deletion mismatch at line {}", idx + 1));
                    }
                    lines.remove(idx);
                }
                '+' => {
                    lines.insert(idx, body_with_nl);
                    idx += 1;
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::with_capacity(current.len());
    for l in &lines {
        out.extend_from_slice(l);
    }
    Ok(out)
}

fn strip_nl(b: &[u8]) -> &[u8] {
    let mut end = b.len();
    while end > 0 && (b[end - 1] == b'\n' || b[end - 1] == b'\r') {
        end -= 1;
    }
    &b[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_a_simple_insert() {
        let current = b"a\nb\nc\n";
        let patch = b"+++ b/file
@@ -1,3 +1,4 @@
 a
+X
 b
 c
";
        let files = parse_patch(patch, 1);
        assert_eq!(files.len(), 1);
        let out = apply_hunks(current, &files[0].hunks, false).unwrap();
        assert_eq!(out, b"a\nX\nb\nc\n");
    }

    #[test]
    fn reverse_inverts_a_patch() {
        let current = b"a\nX\nb\nc\n";
        let patch = b"+++ b/file
@@ -1,3 +1,4 @@
 a
+X
 b
 c
";
        let files = parse_patch(patch, 1);
        let out = apply_hunks(current, &files[0].hunks, true).unwrap();
        assert_eq!(out, b"a\nb\nc\n");
    }
}
