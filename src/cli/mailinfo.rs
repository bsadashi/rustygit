//! `rustygit mailinfo` — extract author / subject / body from an RFC2822
//! email message read from stdin.
//!
//! Output format (matches `git mailinfo`):
//!   * stdout receives parsed lines `Author:`, `Email:`, `Subject:`,
//!     `Date:` plus a blank line and the body.
//!   * argv passes two filenames: <msg-file> <patch-file>. We write the
//!     parsed message into <msg-file> and the in-body patch fragment
//!     into <patch-file>.

use std::io::{self, Read, Write};

use clap::Args;

#[derive(Debug, Args)]
pub struct MailinfoArgs {
    /// Keep [PATCH] prefixes in the subject.
    #[arg(short = 'k')]
    pub keep: bool,
    /// Don't strip leading whitespace from body.
    #[arg(short = 'b')]
    pub keep_blank: bool,
    /// Treat input as UTF-8 (always true today).
    #[arg(short = 'u')]
    pub utf8: bool,
    /// Message body output file.
    #[arg(value_name = "MSG", required = true)]
    pub msg_file: String,
    /// Patch body output file.
    #[arg(value_name = "PATCH", required = true)]
    pub patch_file: String,
}

pub fn run(args: MailinfoArgs) -> io::Result<i32> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let parsed = parse_mail(&input, args.keep);

    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "Author: {}", parsed.author_name)?;
    writeln!(out, "Email: {}", parsed.author_email)?;
    writeln!(out, "Subject: {}", parsed.subject)?;
    writeln!(out, "Date: {}", parsed.date)?;
    writeln!(out)?;

    // Write message body and any embedded `--- ... +++ ...` patch fragment.
    std::fs::write(&args.msg_file, &parsed.message_body)?;
    std::fs::write(&args.patch_file, &parsed.patch_body)?;
    Ok(0)
}

pub(crate) struct ParsedMail {
    pub author_name: String,
    pub author_email: String,
    pub subject: String,
    pub date: String,
    pub message_body: String,
    pub patch_body: String,
}

pub(crate) fn parse_mail(input: &str, keep_prefix: bool) -> ParsedMail {
    let mut lines = input.lines();
    let mut author_name = String::new();
    let mut author_email = String::new();
    let mut subject = String::new();
    let mut date = String::new();

    // Header section: read until the first blank line.
    let mut current_header: Option<(String, String)> = None;
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines.by_ref() {
        if line.is_empty() {
            if let Some(h) = current_header.take() {
                headers.push(h);
            }
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            // Continuation of previous header.
            if let Some(h) = current_header.as_mut() {
                h.1.push(' ');
                h.1.push_str(line.trim_start());
            }
            continue;
        }
        if let Some(h) = current_header.take() {
            headers.push(h);
        }
        if let Some((k, v)) = line.split_once(':') {
            current_header = Some((k.trim().to_lowercase(), v.trim().to_string()));
        }
    }
    if let Some(h) = current_header.take() {
        headers.push(h);
    }

    for (k, v) in &headers {
        match k.as_str() {
            "from" => {
                let (name, email) = split_ident(v);
                author_name = name;
                author_email = email;
            }
            "subject" => {
                subject = if keep_prefix {
                    v.to_string()
                } else {
                    strip_patch_prefix(v)
                };
            }
            "date" => date = v.to_string(),
            _ => {}
        }
    }

    // Body: everything after the blank line, split on the first
    // diff header into "message" and "patch" halves.
    let body: String = lines.collect::<Vec<_>>().join("\n");
    let (msg, patch) = split_body(&body);
    ParsedMail {
        author_name,
        author_email,
        subject,
        date,
        message_body: msg,
        patch_body: patch,
    }
}

fn split_ident(v: &str) -> (String, String) {
    if let Some(open) = v.find('<') {
        if let Some(close) = v.find('>') {
            if close > open + 1 {
                let name = v[..open].trim().trim_matches('"').to_string();
                let email = v[open + 1..close].to_string();
                return (name, email);
            }
        }
    }
    (v.trim().to_string(), String::new())
}

fn strip_patch_prefix(s: &str) -> String {
    let s = s.trim_start();
    if s.starts_with('[') {
        if let Some(close) = s.find(']') {
            return s[close + 1..].trim_start().to_string();
        }
    }
    s.to_string()
}

fn split_body(body: &str) -> (String, String) {
    if let Some(off) = body.find("\n--- ") {
        (body[..off + 1].to_string(), body[off + 1..].to_string())
    } else if let Some(off) = body.find("diff --git ") {
        (body[..off].to_string(), body[off..].to_string())
    } else {
        (body.to_string(), String::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_simple_mail() {
        let mail = "From: Alice <a@x>\n\
                    Subject: [PATCH] hello\n\
                    Date: 2026-01-01\n\
                    \n\
                    Body of message\n\
                    \n\
                    --- a/x\n\
                    +++ b/x\n";
        let p = parse_mail(mail, false);
        assert_eq!(p.author_name, "Alice");
        assert_eq!(p.author_email, "a@x");
        assert_eq!(p.subject, "hello");
        assert!(p.message_body.contains("Body of message"));
        assert!(p.patch_body.starts_with("--- a/x"));
    }

    #[test]
    fn keep_prefix_preserves_brackets() {
        let mail = "From: a <a@x>\nSubject: [PATCH v3] thing\n\n";
        let p = parse_mail(mail, true);
        assert_eq!(p.subject, "[PATCH v3] thing");
    }
}
