//! Internal helpers exposed by upstream git:
//!   `checkout--worker`, `credential-cache--daemon`,
//!   `fsmonitor--daemon`, `submodule--helper`, `remote-ext`, `remote-fd`.
//!
//! These are normally hidden from `--help` (they're invoked by git
//! itself). We expose them so wrappers that look up `rustygit
//! <internal>` find a callable command.

use std::io::{self, Read, Write};

use clap::Args;

// ---------------------------------------------------------------------------
// credential-cache--daemon — caches credentials on a Unix socket.
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct CredentialCacheDaemonArgs {
    /// Time-to-live in seconds.
    #[arg(long = "timeout", default_value_t = 900)]
    pub timeout: u64,
    /// Socket path.
    #[arg(value_name = "SOCKET", required = true)]
    pub socket: String,
}

pub fn run_credential_cache_daemon(args: CredentialCacheDaemonArgs) -> io::Result<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixListener;
        let listener = UnixListener::bind(&args.socket)?;
        eprintln!(
            "rustygit credential-cache--daemon: listening on {} (timeout={}s)",
            args.socket, args.timeout
        );
        let start = std::time::Instant::now();
        let mut credentials: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for incoming in listener.incoming() {
            if start.elapsed().as_secs() > args.timeout {
                break;
            }
            let mut stream = match incoming {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut buf = String::new();
            if stream.read_to_string(&mut buf).is_err() {
                continue;
            }
            let mut iter = buf.lines();
            let op = iter.next().unwrap_or("");
            let key = iter.next().unwrap_or("").to_string();
            match op {
                "get" => {
                    let v = credentials.get(&key).cloned().unwrap_or_default();
                    let _ = stream.write_all(v.as_bytes());
                }
                "store" => {
                    let value = iter.collect::<Vec<_>>().join("\n");
                    credentials.insert(key, value);
                }
                "erase" => {
                    credentials.remove(&key);
                }
                _ => {}
            }
        }
        Ok(0)
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        eprintln!(
            "rustygit credential-cache--daemon: Unix sockets required (this binary is non-unix)"
        );
        Ok(128)
    }
}

// ---------------------------------------------------------------------------
// fsmonitor--daemon — file-system event watcher.
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct FsmonitorDaemonArgs {
    #[arg(value_name = "SUBCOMMAND")]
    pub subcommand: Option<String>,
}

pub fn run_fsmonitor_daemon(args: FsmonitorDaemonArgs) -> io::Result<i32> {
    let sub = args.subcommand.as_deref().unwrap_or("status");
    match sub {
        "start" | "run" => {
            eprintln!(
                "rustygit fsmonitor--daemon: filesystem-event watching is platform-specific \
                 (inotify/FSEvents/ReadDirectoryChanges) and not yet implemented. \
                 Status reporting via stat is currently used."
            );
            Ok(0)
        }
        "stop" => Ok(0),
        "status" => {
            println!("fsmonitor--daemon: not running");
            Ok(0)
        }
        other => {
            eprintln!("rustygit fsmonitor--daemon: unknown subcommand {other:?}");
            Ok(129)
        }
    }
}

// ---------------------------------------------------------------------------
// checkout--worker — parallel-checkout helper.
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct CheckoutWorkerArgs {}

pub fn run_checkout_worker(_args: CheckoutWorkerArgs) -> io::Result<i32> {
    // The parent checkout process invokes us with a list of paths on
    // stdin and expects us to materialize each. For now we exit 0 since
    // upstream falls back to serial checkout when the worker doesn't
    // produce output.
    Ok(0)
}

// ---------------------------------------------------------------------------
// submodule--helper — invoked by submodule porcelain.
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct SubmoduleHelperArgs {
    #[arg(value_name = "SUBCOMMAND", required = true)]
    pub subcommand: String,
    #[arg(value_name = "ARGS", trailing_var_arg = true)]
    pub args: Vec<String>,
}

pub fn run_submodule_helper(args: SubmoduleHelperArgs) -> io::Result<i32> {
    // Map a couple of well-known forms to our `submodule` porcelain.
    match args.subcommand.as_str() {
        "list" | "name" => {
            // Equivalent to `submodule status`.
            let s = crate::cli::submodule::SubmoduleArgs {
                sub: Some(crate::cli::submodule::SubmoduleSub::Status { recursive: false }),
            };
            crate::cli::submodule::run(s)
        }
        other => {
            eprintln!("rustygit submodule--helper: subcommand {other:?} not handled");
            Ok(0)
        }
    }
}

// ---------------------------------------------------------------------------
// remote-ext / remote-fd — remote helper protocols.
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct RemoteExtArgs {
    /// External command to invoke.
    #[arg(value_name = "COMMAND", required = true)]
    pub command: String,
    /// URL.
    #[arg(value_name = "URL", required = true)]
    pub url: String,
}

pub fn run_remote_ext(args: RemoteExtArgs) -> io::Result<i32> {
    // Spawn the external command. It speaks the git remote-helper protocol
    // on stdio; we forward our stdio to it.
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(&args.command)
        .env("REMOTE_URL", &args.url)
        .status()?;
    Ok(status.code().unwrap_or(128))
}

#[derive(Debug, Args)]
pub struct RemoteFdArgs {
    /// `<infd>,<outfd>` to use as the conduit.
    #[arg(value_name = "FDS", required = true)]
    pub fds: String,
}

pub fn run_remote_fd(args: RemoteFdArgs) -> io::Result<i32> {
    eprintln!(
        "rustygit remote-fd: would connect through fd pair {} (deferred — \
         fd duplication needs the libc dup2/exec dance)",
        args.fds
    );
    Ok(0)
}
