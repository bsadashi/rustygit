//! `rustygit mailsplit` — split an mbox file into one message per file.
//!
//! Output: writes files named `0001`, `0002`, ... into `-o <DIR>`.
//! Prints the count of messages to stdout.

use std::io::{self, Read, Write};

use clap::Args;

#[derive(Debug, Args)]
pub struct MailsplitArgs {
    /// Output directory.
    #[arg(short = 'o', value_name = "DIR", required = true)]
    pub output: String,
    /// Treat input as MH-style (one message per file already).
    #[arg(short = 'f', value_name = "N", default_value_t = 0)]
    pub start_at: u32,
    /// Bracket-style — the input is a single message, not an mbox.
    #[arg(short = 'b')]
    pub single: bool,
    /// Input files (stdin if none).
    #[arg(value_name = "FILE")]
    pub files: Vec<String>,
}

pub fn run(args: MailsplitArgs) -> io::Result<i32> {
    std::fs::create_dir_all(&args.output)?;
    let mut input = Vec::new();
    if args.files.is_empty() {
        io::stdin().read_to_end(&mut input)?;
    } else {
        for f in &args.files {
            input.extend(std::fs::read(f)?);
        }
    }

    let messages = if args.single {
        vec![input]
    } else {
        split_mbox(&input)
    };
    for (i, msg) in messages.iter().enumerate() {
        let counter = args.start_at + 1 + i as u32;
        let filename = format!("{counter:04}");
        let path = std::path::Path::new(&args.output).join(&filename);
        let mut f = std::fs::File::create(&path)?;
        f.write_all(msg)?;
    }
    println!("{}", messages.len());
    Ok(0)
}

/// Split an mbox by `^From ` lines.
pub(crate) fn split_mbox(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut last = 0;
    let needle = b"\nFrom ";
    let mut splits: Vec<usize> = Vec::new();
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            splits.push(i + 1);
        }
        i += 1;
    }
    for &start in &splits {
        if start > last {
            out.push(bytes[last..start].to_vec());
        }
        last = start;
    }
    out.push(bytes[last..].to_vec());
    // Trim empty leading messages.
    out.retain(|m| !m.iter().all(|&b| b == b'\n'));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_two_messages() {
        let mbox = b"From a@x  Mon Jan 1\nSubject: A\n\nbody A\n\
                     From b@y  Tue Jan 2\nSubject: B\n\nbody B\n";
        let parts = split_mbox(mbox);
        assert_eq!(parts.len(), 2);
        assert!(parts[0].starts_with(b"From a@x"));
        assert!(parts[1].starts_with(b"From b@y"));
    }
}
