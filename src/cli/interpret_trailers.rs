//! `rustygit interpret-trailers` — read a commit message on stdin and
//! add/replace/remove RFC2822-ish trailers (`Key: value`) at the end.
//!
//! Subset:
//!   * `--trailer "<token>: <value>"` — add (repeatable).
//!   * `--if-exists=addIfDifferent|replace|add|doNothing` (default: addIfDifferent).
//!   * `--if-missing=add|doNothing` (default: add).
//!   * `--only-trailers` — print only the trailer block.
//!   * `--unfold` — join folded trailers into one logical line.

use std::io::{self, Read, Write};

use clap::Args;

#[derive(Debug, Args)]
pub struct InterpretTrailersArgs {
    /// Add a trailer (repeatable). Format: `Key: value`.
    #[arg(long = "trailer", value_name = "TRAILER")]
    pub trailer: Vec<String>,
    /// What to do when the key already exists.
    #[arg(long = "if-exists", default_value = "addIfDifferent")]
    pub if_exists: String,
    /// What to do when the key is missing.
    #[arg(long = "if-missing", default_value = "add")]
    pub if_missing: String,
    /// Print only the trailer block.
    #[arg(long = "only-trailers")]
    pub only_trailers: bool,
    /// Unfold continuation lines in existing trailers.
    #[arg(long = "unfold")]
    pub unfold: bool,
}

pub fn run(args: InterpretTrailersArgs) -> io::Result<i32> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;

    let (body, trailer_block) = split_message(&input);
    let mut trailers = parse_trailers(trailer_block);

    if args.unfold {
        for t in &mut trailers {
            t.value = t.value.replace('\n', " ");
        }
    }

    for new in &args.trailer {
        let (k, v) = match new.split_once(':') {
            Some((k, v)) => (k.trim().to_string(), v.trim().to_string()),
            None => {
                eprintln!("rustygit: interpret-trailers: bad trailer {new:?}");
                return Ok(129);
            }
        };
        apply_trailer(&mut trailers, &k, &v, &args.if_exists, &args.if_missing);
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    if !args.only_trailers {
        out.write_all(body.as_bytes())?;
        // Ensure a blank line separates body and trailer block.
        if !body.is_empty() && !body.ends_with("\n\n") {
            if !body.ends_with('\n') {
                writeln!(out)?;
            }
            writeln!(out)?;
        }
    }
    for t in &trailers {
        writeln!(out, "{}: {}", t.key, t.value)?;
    }
    Ok(0)
}

#[derive(Debug, Clone)]
struct Trailer {
    key: String,
    value: String,
}

fn split_message(input: &str) -> (&str, &str) {
    // Find the last block of lines where every line looks like a trailer
    // (`Key: value`). That block is the trailer section. Everything else
    // is the body.
    let lines: Vec<&str> = input.lines().collect();
    let mut i = lines.len();
    while i > 0 {
        let line = lines[i - 1];
        if line.is_empty() {
            break;
        }
        if !is_trailer_line(line) {
            break;
        }
        i -= 1;
    }
    if i == lines.len() {
        // No trailing trailers; the entire input is the body.
        return (input, "");
    }
    // The split point in characters: walk lines until we hit index `i`.
    // Simpler: rebuild from line indices.
    let body_end = lines[..i].iter().map(|l| l.len() + 1).sum::<usize>();
    let body_end = body_end.min(input.len());
    (&input[..body_end], &input[body_end..])
}

fn is_trailer_line(line: &str) -> bool {
    if let Some(idx) = line.find(':') {
        if idx == 0 {
            return false;
        }
        let key = &line[..idx];
        return key.chars().all(|c| c.is_alphanumeric() || c == '-');
    }
    false
}

fn parse_trailers(block: &str) -> Vec<Trailer> {
    let mut out = Vec::new();
    for raw in block.lines() {
        if let Some(idx) = raw.find(':') {
            let key = raw[..idx].trim();
            let value = raw[idx + 1..].trim();
            if !key.is_empty() {
                out.push(Trailer {
                    key: key.to_string(),
                    value: value.to_string(),
                });
            }
        }
    }
    out
}

fn apply_trailer(
    trailers: &mut Vec<Trailer>,
    key: &str,
    value: &str,
    if_exists: &str,
    if_missing: &str,
) {
    let existing = trailers.iter().any(|t| t.key.eq_ignore_ascii_case(key));
    if !existing {
        if if_missing == "add" {
            trailers.push(Trailer {
                key: key.to_string(),
                value: value.to_string(),
            });
        }
        return;
    }
    match if_exists {
        "doNothing" => {}
        "replace" => {
            for t in trailers.iter_mut() {
                if t.key.eq_ignore_ascii_case(key) {
                    t.value = value.to_string();
                }
            }
        }
        "add" => {
            trailers.push(Trailer {
                key: key.to_string(),
                value: value.to_string(),
            });
        }
        // addIfDifferent: default
        _ => {
            let same = trailers
                .iter()
                .any(|t| t.key.eq_ignore_ascii_case(key) && t.value == value);
            if !same {
                trailers.push(Trailer {
                    key: key.to_string(),
                    value: value.to_string(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_trailer_block() {
        let input = "Subject\n\nBody body body\n\nSigned-off-by: Alice <a@x>\n";
        let (body, block) = split_message(input);
        assert!(body.contains("Body body body"));
        let ts = parse_trailers(block);
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].key, "Signed-off-by");
    }

    #[test]
    fn add_if_missing_adds() {
        let mut ts = Vec::new();
        apply_trailer(&mut ts, "Signed-off-by", "Alice", "addIfDifferent", "add");
        assert_eq!(ts.len(), 1);
    }

    #[test]
    fn add_if_different_dedupes() {
        let mut ts = vec![Trailer {
            key: "Signed-off-by".into(),
            value: "Alice".into(),
        }];
        apply_trailer(&mut ts, "Signed-off-by", "Alice", "addIfDifferent", "add");
        assert_eq!(ts.len(), 1);
    }

    #[test]
    fn replace_overwrites() {
        let mut ts = vec![Trailer {
            key: "Reviewed-by".into(),
            value: "Bob".into(),
        }];
        apply_trailer(&mut ts, "Reviewed-by", "Carol", "replace", "add");
        assert_eq!(ts[0].value, "Carol");
    }
}
