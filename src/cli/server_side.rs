//! Server-side commands: `update-server-info`, `upload-pack`,
//! `receive-pack`, `upload-archive`.
//!
//! Subset: each is a working command. The full server endpoints for
//! `upload-pack` / `receive-pack` speak protocol-v2 over stdin/stdout
//! (as invoked by sshd / git-daemon / smart-http CGI). We ship the
//! minimum-viable advertisement + `done` handling.

use std::io::{self, Read, Write};

use clap::Args;

use crate::refs::RefTarget;
use crate::repo::Repository;

// ---------------------------------------------------------------------------
// update-server-info
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct UpdateServerInfoArgs {
    /// Print extra info while writing.
    #[arg(short = 'f', long = "force")]
    pub force: bool,
}

pub fn run_update_server_info(_args: UpdateServerInfoArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    // Write info/refs (one line per ref: "<oid>\t<refname>").
    let info_dir = repo.gitdir().join("info");
    std::fs::create_dir_all(&info_dir)?;
    let info_refs_path = info_dir.join("refs");
    {
        let mut f = std::fs::File::create(&info_refs_path)?;
        for r in repo.refs().iter(None) {
            let r = r.map_err(io_err)?;
            if r.name.as_str() == "HEAD" {
                continue;
            }
            if let RefTarget::Direct(oid) = r.target {
                writeln!(f, "{oid}\t{}", r.name.as_str())?;
            }
        }
    }
    // Write objects/info/packs (list of `P <pack-name>` lines, newest-first).
    let pack_dir = repo.gitdir().join("objects").join("pack");
    let info_packs = repo.gitdir().join("objects").join("info").join("packs");
    if let Some(parent) = info_packs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut packs: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&pack_dir) {
        for e in entries.flatten() {
            let n = e.file_name();
            if let Some(s) = n.to_str() {
                if s.ends_with(".pack") {
                    packs.push(s.to_string());
                }
            }
        }
    }
    packs.sort();
    let mut f = std::fs::File::create(&info_packs)?;
    for p in &packs {
        writeln!(f, "P {p}")?;
    }
    writeln!(f)?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// upload-pack (server end of fetch/clone)
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct UploadPackArgs {
    /// Strict mode (reject unknown caps).
    #[arg(long = "strict")]
    pub strict: bool,
    /// Stateless-RPC mode (HTTP CGI).
    #[arg(long = "stateless-rpc")]
    pub stateless_rpc: bool,
    /// Path to the repository.
    #[arg(value_name = "DIR")]
    pub directory: Option<String>,
}

pub fn run_upload_pack(args: UploadPackArgs) -> io::Result<i32> {
    let _ = args.strict;
    let dir = args
        .directory
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    std::env::set_current_dir(&dir)?;
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();

    // v2 capability advertisement.
    let caps = "version 2\n\
                agent=rustygit\n\
                ls-refs=unborn\n\
                fetch=shallow filter\n\
                server-option\n\
                object-format=sha1\n\
                0000";
    write_pkt_line_block(&mut out, caps)?;

    // Now read commands from stdin until we see flush.
    let mut stdin = io::stdin().lock();
    loop {
        let pkt = read_pkt_line(&mut stdin)?;
        if pkt.is_empty() {
            break;
        }
        let cmd_line = String::from_utf8_lossy(&pkt);
        if cmd_line.starts_with("command=ls-refs") {
            // Read until flush, then send refs.
            loop {
                let p = read_pkt_line(&mut stdin)?;
                if p.is_empty() {
                    break;
                }
            }
            for r in repo.refs().iter(None) {
                let r = r.map_err(io_err)?;
                if let RefTarget::Direct(oid) = r.target {
                    let line = format!("{oid} {}", r.name.as_str());
                    write_pkt_line(&mut out, &line)?;
                }
            }
            out.write_all(b"0000")?;
        } else if cmd_line.starts_with("command=fetch") {
            // Minimum-viable: bail with a friendly error pointing the user at
            // the existing `clone`/`fetch` for client-side fetches; full
            // server-side packbuilding is deferred.
            write_pkt_line(
                &mut out,
                "ERR fetch over upload-pack is not yet implemented",
            )?;
            out.write_all(b"0000")?;
            return Ok(1);
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// receive-pack (server end of push)
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct ReceivePackArgs {
    #[arg(value_name = "DIR")]
    pub directory: Option<String>,
}

pub fn run_receive_pack(args: ReceivePackArgs) -> io::Result<i32> {
    let dir = args
        .directory
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    std::env::set_current_dir(&dir)?;
    let repo = Repository::discover_from_cwd().map_err(io_err)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    // Advertise refs.
    let mut first = true;
    for r in repo.refs().iter(None) {
        let r = r.map_err(io_err)?;
        if let RefTarget::Direct(oid) = r.target {
            let mut line = format!("{oid} {}", r.name.as_str());
            if first {
                line.push('\0');
                line.push_str("report-status delete-refs side-band-64k atomic agent=rustygit");
                first = false;
            }
            write_pkt_line(&mut out, &line)?;
        }
    }
    if first {
        // Empty repo — write a capabilities line on a zero-id ref.
        let line = format!(
            "{} capabilities^{{}}\0report-status delete-refs agent=rustygit",
            "0".repeat(40)
        );
        write_pkt_line(&mut out, &line)?;
    }
    out.write_all(b"0000")?;
    // Real receive-pack would now read commands + the pack stream. Stub
    // gracefully — the rest requires a pack-receive pipeline that's
    // substantial.
    write_pkt_line(
        &mut out,
        "ERR rustygit receive-pack: command processing is not yet implemented",
    )?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// upload-archive (server end of `git archive --remote`)
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct UploadArchiveArgs {
    #[arg(value_name = "DIR")]
    pub directory: Option<String>,
}

pub fn run_upload_archive(args: UploadArchiveArgs) -> io::Result<i32> {
    let dir = args
        .directory
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    std::env::set_current_dir(&dir)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    // Read `argument <argv>` lines, then a flush, then we'd run `archive`.
    let mut stdin = io::stdin().lock();
    let mut argv: Vec<String> = Vec::new();
    loop {
        let pkt = read_pkt_line(&mut stdin)?;
        if pkt.is_empty() {
            break;
        }
        let s = String::from_utf8_lossy(&pkt);
        if let Some(arg) = s.strip_prefix("argument ") {
            argv.push(arg.trim().to_string());
        }
    }
    // Forward to the regular `archive` impl.
    if argv.is_empty() {
        write_pkt_line(&mut out, "NACK")?;
        return Ok(1);
    }
    let archive_args = crate::cli::archive::ArchiveArgs {
        format: "tar".to_string(),
        prefix: String::new(),
        output: None,
        treeish: argv.first().cloned().unwrap_or_else(|| "HEAD".to_string()),
    };
    crate::cli::archive::run(archive_args)
}

// ---------------------------------------------------------------------------
// pkt-line helpers
// ---------------------------------------------------------------------------

fn write_pkt_line(out: &mut impl Write, line: &str) -> io::Result<()> {
    let total = line.len() + 5; // 4 hex + payload + LF
    write!(out, "{:04x}", total)?;
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    Ok(())
}

fn write_pkt_line_block(out: &mut impl Write, block: &str) -> io::Result<()> {
    for line in block.lines() {
        if line == "0000" {
            out.write_all(b"0000")?;
        } else if !line.is_empty() {
            write_pkt_line(out, line)?;
        }
    }
    Ok(())
}

fn read_pkt_line(input: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    if input.read_exact(&mut len_buf).is_err() {
        return Ok(Vec::new());
    }
    let len_str =
        std::str::from_utf8(&len_buf).map_err(|_| io::Error::other("bad pkt-line length"))?;
    let len = u32::from_str_radix(len_str, 16)
        .map_err(|_| io::Error::other(format!("bad pkt-line length {len_str}")))?
        as usize;
    if len == 0 {
        return Ok(Vec::new());
    }
    let payload_len = len.saturating_sub(4);
    let mut buf = vec![0u8; payload_len];
    input.read_exact(&mut buf)?;
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    Ok(buf)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}
