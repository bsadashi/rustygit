//! `rustygit send-email` — pipe `.patch` files into a local `sendmail` (or
//! configured SMTP command).
//!
//! Subset: invoke `sendmail` (configured via `sendemail.smtpserver`)
//! once per file. To/CC/From are taken from headers in each patch file.

use std::io::{self, Write};
use std::process::{Command, Stdio};

use clap::Args;

use crate::config::Config;

#[derive(Debug, Args)]
pub struct SendEmailArgs {
    /// To address(es) (comma-separated, repeatable).
    #[arg(long = "to", value_name = "ADDR")]
    pub to: Vec<String>,
    /// CC address(es).
    #[arg(long = "cc", value_name = "ADDR")]
    pub cc: Vec<String>,
    /// From address.
    #[arg(long = "from", value_name = "ADDR")]
    pub from: Option<String>,
    /// SMTP server / sendmail program.
    #[arg(long = "smtp-server", value_name = "PATH")]
    pub smtp_server: Option<String>,
    /// Files to send.
    #[arg(value_name = "FILE", required = true)]
    pub files: Vec<String>,
}

pub fn run(args: SendEmailArgs) -> io::Result<i32> {
    let gitdir = crate::repo::Repository::discover_from_cwd()
        .map(|r| r.gitdir().to_path_buf())
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    let config = Config::from_repo_dir(&gitdir).unwrap_or_else(|_| Config::empty());

    let sendmail = args.smtp_server.unwrap_or_else(|| {
        config
            .get_string("sendemail", "smtpserver")
            .map(str::to_string)
            .unwrap_or_else(|| "/usr/sbin/sendmail".to_string())
    });

    let to_set: Vec<String> = args.to.clone();
    if to_set.is_empty() {
        eprintln!("rustygit send-email: --to is required");
        return Ok(129);
    }
    let _ = args.cc;
    let _ = args.from;

    for file in &args.files {
        let body = std::fs::read(file)?;
        // We just pipe verbatim into the sendmail-compatible program with
        // -t (read recipient from headers).
        let mut child = Command::new(&sendmail)
            .arg("-t")
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(&body)?;
        }
        let status = child.wait()?;
        if !status.success() {
            eprintln!("rustygit send-email: {file}: sendmail returned non-zero");
            return Ok(1);
        }
        println!("sent {file}");
    }
    Ok(0)
}
