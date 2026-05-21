//! `rustygit daemon` — minimal `git://` daemon (TCP on port 9418).
//!
//! Speaks the simple, unencrypted git wire protocol. On connect:
//!   1. Read a single pkt-line of the form
//!      `git-upload-pack <path>\0host=<hostname>\0`.
//!   2. Look up `<path>` under `--base-path`.
//!   3. Delegate to `upload-pack`'s implementation (advertise refs + accept fetch).
//!
//! Real git's `git daemon` also handles `git-receive-pack` and access
//! control via `git-daemon-export-ok`. We ship upload-pack (read-only)
//! plus the export-ok gate.

use std::io;
use std::net::TcpListener;
use std::path::PathBuf;

use clap::Args;

#[derive(Debug, Args)]
pub struct DaemonArgs {
    /// Base directory containing exported repos.
    #[arg(long = "base-path", value_name = "DIR", default_value = ".")]
    pub base_path: String,
    /// Bind port (default 9418).
    #[arg(long = "port", default_value_t = 9418)]
    pub port: u16,
    /// Bind address (default 0.0.0.0).
    #[arg(long = "listen", default_value = "0.0.0.0")]
    pub listen: String,
    /// Allow `git-receive-pack` (writes).
    #[arg(long = "enable-receive-pack")]
    pub enable_receive_pack: bool,
}

pub fn run(args: DaemonArgs) -> io::Result<i32> {
    let addr = format!("{}:{}", args.listen, args.port);
    let listener = TcpListener::bind(&addr)?;
    eprintln!(
        "rustygit daemon: listening on {} (base={}), receive-pack {}",
        addr,
        args.base_path,
        if args.enable_receive_pack {
            "ON"
        } else {
            "OFF"
        }
    );
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        // Read the request packet.
        let mut header = [0u8; 4];
        use std::io::{Read, Write};
        if stream.read_exact(&mut header).is_err() {
            continue;
        }
        let len_hex = std::str::from_utf8(&header).unwrap_or("0000");
        let len = u32::from_str_radix(len_hex, 16).unwrap_or(0) as usize;
        if len < 4 {
            continue;
        }
        let mut payload = vec![0u8; len - 4];
        if stream.read_exact(&mut payload).is_err() {
            continue;
        }
        let request = String::from_utf8_lossy(&payload);
        // Expected form: "git-upload-pack /path\0host=foo\0"
        let mut iter = request.split('\0');
        let cmd_and_path = iter.next().unwrap_or("");
        let mut cp_iter = cmd_and_path.splitn(2, ' ');
        let cmd = cp_iter.next().unwrap_or("");
        let path = cp_iter.next().unwrap_or("/");
        let repo_path = PathBuf::from(&args.base_path).join(path.trim_start_matches('/'));
        if !repo_path.join("git-daemon-export-ok").is_file() {
            let _ = stream.write_all(b"0040ERR access denied: missing git-daemon-export-ok\n");
            continue;
        }
        match cmd {
            "git-upload-pack" => {
                let _ = std::env::set_current_dir(&repo_path);
                let _ = crate::cli::server_side::run_upload_pack(
                    crate::cli::server_side::UploadPackArgs {
                        strict: false,
                        stateless_rpc: false,
                        directory: Some(repo_path.display().to_string()),
                    },
                );
            }
            "git-receive-pack" if args.enable_receive_pack => {
                let _ = std::env::set_current_dir(&repo_path);
                let _ = crate::cli::server_side::run_receive_pack(
                    crate::cli::server_side::ReceivePackArgs {
                        directory: Some(repo_path.display().to_string()),
                    },
                );
            }
            other => {
                let line = format!("ERR unknown command '{other}'\n");
                let header = format!("{:04x}", line.len() + 4);
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(line.as_bytes());
            }
        }
    }
    Ok(0)
}
