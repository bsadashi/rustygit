//! `rustygit stripspace` — clean up whitespace in commit-message-style
//! input read from stdin.
//!
//! Rules (matching `git stripspace`):
//!   * Trim trailing whitespace on every line.
//!   * Collapse runs of empty lines into a single empty line.
//!   * Strip leading empty lines.
//!   * Strip trailing empty lines.
//!   * Ensure the output ends with exactly one `\n`.
//!   * With `-c` / `--strip-comments`, drop lines whose first non-space
//!     byte is `#`.
//!   * With `--comment-lines`, prefix every non-empty line with `# `
//!     (used by commit's editor-flow template).

use std::io::{self, Read, Write};

use clap::Args;

#[derive(Debug, Args)]
pub struct StripspaceArgs {
    /// Strip comment lines (lines starting with `#`).
    #[arg(short = 's', long = "strip-comments")]
    pub strip_comments: bool,
    /// Inverse: turn every line into a comment line.
    #[arg(short = 'c', long = "comment-lines")]
    pub comment_lines: bool,
}

pub fn run(args: StripspaceArgs) -> io::Result<i32> {
    if args.strip_comments && args.comment_lines {
        eprintln!("rustygit: stripspace: -s and -c are mutually exclusive");
        return Ok(129);
    }

    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;

    let cleaned = if args.comment_lines {
        comment_lines(&input)
    } else {
        strip(&input, args.strip_comments)
    };

    let stdout = io::stdout();
    let mut out = stdout.lock();
    out.write_all(cleaned.as_bytes())?;
    Ok(0)
}

/// The core stripspace algorithm.
pub fn strip(input: &str, strip_comments: bool) -> String {
    let mut out = String::with_capacity(input.len());
    let mut pending_blank = false;
    let mut seen_content = false;

    for raw_line in input.split('\n') {
        // Drop trailing whitespace.
        let mut line = raw_line.trim_end_matches([' ', '\t', '\r']).to_string();

        // Comment stripping looks at the first non-space character.
        if strip_comments {
            let first_non_space = line.trim_start();
            if first_non_space.starts_with('#') {
                continue;
            }
        }

        if line.is_empty() {
            // Defer; we only emit blank lines once we know they're followed
            // by content (no leading blanks, no double blanks, no trailing
            // blanks).
            if seen_content {
                pending_blank = true;
            }
        } else {
            if pending_blank {
                out.push('\n');
                pending_blank = false;
            }
            seen_content = true;
            line.push('\n');
            out.push_str(&line);
        }
    }

    // The split above produces an extra "" element when input ends with '\n'.
    // pending_blank may be true here only if the final non-blank line is
    // already followed by exactly one '\n' in `out` — so we're done.
    out
}

/// `--comment-lines`: prefix every non-empty line with `# `.
pub fn comment_lines(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + input.lines().count() * 2);
    for line in input.split('\n') {
        if line.is_empty() {
            out.push('\n');
            continue;
        }
        out.push_str("# ");
        out.push_str(line);
        out.push('\n');
    }
    // Trim duplicate trailing newline introduced when input ends with '\n'.
    if input.ends_with('\n') && out.ends_with("\n\n") {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_trailing_whitespace() {
        assert_eq!(strip("hello   \n", false), "hello\n");
    }

    #[test]
    fn collapses_consecutive_blank_lines() {
        assert_eq!(strip("a\n\n\n\nb\n", false), "a\n\nb\n");
    }

    #[test]
    fn strips_leading_blank_lines() {
        assert_eq!(strip("\n\nfirst\n", false), "first\n");
    }

    #[test]
    fn strips_trailing_blank_lines() {
        assert_eq!(strip("text\n\n\n", false), "text\n");
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(strip("", false), "");
        assert_eq!(strip("\n\n\n", false), "");
    }

    #[test]
    fn strip_comments_drops_pound_lines() {
        assert_eq!(strip("subject\n# comment\nbody\n", true), "subject\nbody\n");
    }

    #[test]
    fn comment_lines_prefixes_each() {
        assert_eq!(comment_lines("a\nb\n"), "# a\n# b\n");
    }

    #[test]
    fn comment_lines_preserves_blanks() {
        assert_eq!(comment_lines("a\n\nb\n"), "# a\n\n# b\n");
    }
}
