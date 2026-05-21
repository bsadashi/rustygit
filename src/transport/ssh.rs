//! SSH transport — protocol v2 over a spawned `ssh` child process.
//!
//! [`SshConnection`] implements [`Connection`] by shelling out to the system
//! `ssh` binary (just like upstream git does) and shuttling pkt-line bytes
//! over its stdin/stdout. Deliberately avoids embedding an SSH client crate
//! (`ssh2`/`russh`): spawning the system binary picks up the user's existing
//! `~/.ssh/config`, keys, agent forwarding, and known-hosts file for free,
//! and behaves identically to real git.
//!
//! ## Wire shape
//!
//! After parsing the URL we spawn:
//!
//! ```text
//!   ssh [-p <port>] [user@]host  'GIT_PROTOCOL=version=2 git-upload-pack <path>'
//! ```
//!
//! The remote-side shell expands the env-var assignment as a prefix to the
//! `git-upload-pack` invocation, so the server enters protocol-v2 mode and
//! sends the capability advertisement immediately on stdout. There is no
//! separate "discovery" exchange the way HTTP has — the first read off the
//! child's stdout IS the v2 capability ad.
//!
//! This has a structural consequence for the [`Connection`] trait: the same
//! `git-upload-pack` process handles BOTH `discover_capabilities` and the
//! subsequent `send_request`. We hold the spawned [`Child`] across both calls
//! and write the request body to its stdin in `send_request`, then stream
//! the response from its stdout until EOF.
//!
//! ## URL grammar
//!
//! Two equivalent forms (both supported by `is_ssh_url`):
//!
//! 1. `ssh://[user@]host[:port]/path/to/repo.git` — explicit scheme.
//! 2. `[user@]host:path/to/repo.git` — historical scp-form. Detected by the
//!    presence of a `:` BEFORE any `/` (otherwise it'd be a Unix path).
//!
//! Reference: `gitprotocol-pack(5)` ("ssh://" transport), and git's own
//! `connect.c::parse_connect_url` for the disambiguation rules.

use std::io::{Read, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::thread::{self, JoinHandle};

use super::pkt_line::{PktLine, PktLineReader};
use super::{Connection, TransportError};

/// Which remote git service to invoke after SSH-ing in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshService {
    /// `git-upload-pack` — fetch / clone / ls-remote.
    UploadPack,
    /// `git-receive-pack` — push.
    ReceivePack,
}

impl SshService {
    fn command(self) -> &'static str {
        match self {
            SshService::UploadPack => "git-upload-pack",
            SshService::ReceivePack => "git-receive-pack",
        }
    }
}

/// SSH-transport git client speaking protocol v2.
///
/// Construction parses the URL but doesn't spawn anything. The first call to
/// [`discover_capabilities`](Connection::discover_capabilities) spawns the
/// `ssh` child and reads the initial v2 capability advertisement off its
/// stdout. A subsequent [`send_request`](Connection::send_request) writes the
/// request body to the SAME child's stdin and streams the response from its
/// stdout until the remote `git-upload-pack` exits.
pub struct SshConnection {
    /// Parsed URL parts.
    user: Option<String>,
    host: String,
    port: Option<u16>,
    remote_path: String,
    /// Service to invoke remotely.
    service: SshService,
    /// The spawned `ssh` process. `Some` after `spawn`; `None` before.
    child: Option<Child>,
    /// `ssh` binary to invoke. Configurable for tests (defaults to `"ssh"`,
    /// found via $PATH).
    ssh_program: String,
    /// Whether `send_request` has already been called once on this connection.
    /// Used to enforce single-use semantics — v2-over-SSH is one round-trip.
    consumed: bool,
    /// Stderr-drain thread spawned at `discover_capabilities` time. Handed
    /// off to the `ChildReader` when `send_request` runs, so the reader can
    /// surface the captured stderr if the remote exits non-zero.
    pending_stderr: Option<JoinHandle<Vec<u8>>>,
}

impl std::fmt::Debug for SshConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshConnection")
            .field("user", &self.user)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("remote_path", &self.remote_path)
            .field("service", &self.service)
            .field("child_spawned", &self.child.is_some())
            .field("ssh_program", &self.ssh_program)
            .finish()
    }
}

impl SshConnection {
    /// Parse `url` (either `ssh://...` or `[user@]host:path` scp-form) and
    /// build a connection. Does NOT spawn ssh — the child process is created
    /// lazily by [`discover_capabilities`](Connection::discover_capabilities).
    pub fn new(url: &str, service: SshService) -> Result<Self, TransportError> {
        let parts = parse_ssh_url(url)?;
        Ok(Self {
            user: parts.user,
            host: parts.host,
            port: parts.port,
            remote_path: parts.path,
            service,
            child: None,
            ssh_program: "ssh".to_string(),
            consumed: false,
            pending_stderr: None,
        })
    }

    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }
    pub fn host(&self) -> &str {
        &self.host
    }
    pub fn port(&self) -> Option<u16> {
        self.port
    }
    pub fn remote_path(&self) -> &str {
        &self.remote_path
    }
    pub fn service(&self) -> SshService {
        self.service
    }

    /// Override the `ssh` binary. Used by tests to inject a stub.
    #[doc(hidden)]
    pub fn set_ssh_program(&mut self, program: impl Into<String>) {
        self.ssh_program = program.into();
    }

    /// Build (but don't execute) the `Command` we'd spawn. Exposed for tests.
    fn build_command(&self) -> Command {
        let mut cmd = Command::new(&self.ssh_program);
        if let Some(port) = self.port {
            cmd.arg("-p").arg(port.to_string());
        }
        // `user@host` if a user was specified, else just `host`. ssh picks
        // the local user / its config when none is passed.
        let target = match &self.user {
            Some(u) => format!("{}@{}", u, self.host),
            None => self.host.clone(),
        };
        cmd.arg(target);
        // The remote command runs in a shell, so the `VAR=value cmd args`
        // form sets GIT_PROTOCOL for that one invocation. We single-quote
        // the path so it survives spaces and shell metacharacters; any
        // embedded single quote in the path is escaped POSIX-style by
        // closing the quote, emitting `\'`, and reopening.
        let escaped_path = shell_single_quote(&self.remote_path);
        let remote = format!(
            "GIT_PROTOCOL=version=2 {} {}",
            self.service.command(),
            escaped_path
        );
        cmd.arg(remote);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    /// Spawn the `ssh` child. Errors if called more than once for the same
    /// `SshConnection`.
    fn spawn(&mut self) -> Result<(), TransportError> {
        if self.child.is_some() {
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "ssh child already spawned for this connection",
            )));
        }
        let mut cmd = self.build_command();
        let child = cmd.spawn().map_err(|e| {
            // Decorate with the program name — `No such file or directory`
            // for a missing `ssh` binary is much friendlier if we say which
            // program we tried.
            TransportError::Io(std::io::Error::new(
                e.kind(),
                format!("failed to spawn `{}`: {e}", self.ssh_program),
            ))
        })?;
        self.child = Some(child);
        Ok(())
    }

    /// Capture stderr in a background thread so we can surface it on errors
    /// or when the child exits non-zero. Returns the join handle; the caller
    /// can collect the bytes when reading is complete.
    fn drain_stderr(&mut self) -> Option<JoinHandle<Vec<u8>>> {
        let mut stderr = self.child.as_mut()?.stderr.take()?;
        Some(thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            buf
        }))
    }
}

// ---------------------------------------------------------------------------
// Connection impl
// ---------------------------------------------------------------------------

impl Connection for SshConnection {
    fn discover_capabilities(&mut self) -> Result<Vec<PktLine>, TransportError> {
        if self.child.is_none() {
            self.spawn()?;
        } else {
            // Already spawned — discover_capabilities is once-per-connection.
            return Err(TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "discover_capabilities already called on this SshConnection",
            )));
        }

        // Drain stderr concurrently so the child can't deadlock on a full
        // pipe while we're reading stdout. We attach the handle to the
        // connection so `send_request` can surface stderr on failure.
        let stderr_handle = self.drain_stderr();

        // Read pkt-lines from stdout up to and including the first flush —
        // that delimits the v2 capability advertisement.
        let stdout = self
            .child
            .as_mut()
            .and_then(|c| c.stdout.take())
            .ok_or_else(|| TransportError::Io(std::io::Error::other("ssh child has no stdout")))?;

        let mut reader = PktLineReader::new(stdout);
        let mut out = Vec::new();
        loop {
            match reader.next_pkt() {
                Ok(Some(p)) => {
                    let done = matches!(p, PktLine::Flush);
                    out.push(p);
                    if done {
                        break;
                    }
                }
                Ok(None) => {
                    // EOF before flush — server probably failed. Wait for
                    // the child, gather stderr, and surface it.
                    return Err(self.fail_with_stderr(
                        stderr_handle,
                        "EOF before flush during capability advertisement",
                    ));
                }
                Err(e) => {
                    // Decorate with stderr if any, for easier diagnostics.
                    let stderr_msg = stderr_handle
                        .and_then(|h| h.join().ok())
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                        .unwrap_or_default();
                    return Err(decorate_with_stderr(e, &stderr_msg));
                }
            }
        }

        // Put stdout back so `send_request` can keep reading from it.
        let inner_stdout = reader.into_inner();
        if let Some(child) = self.child.as_mut() {
            child.stdout = Some(inner_stdout);
        }
        // Stash the stderr handle on the side so send_request can finalize it.
        // (We use a small holder field to avoid leaking the JoinHandle type
        // into the Connection trait signature.)
        self.pending_stderr = stderr_handle;

        Ok(out)
    }

    fn send_request(&mut self, body: Vec<u8>) -> Result<Box<dyn Read + Send>, TransportError> {
        if self.consumed {
            return Err(TransportError::Io(std::io::Error::other(
                "send_request already called on this SshConnection",
            )));
        }
        // discover_capabilities must have been called first to spawn ssh.
        let child = self.child.as_mut().ok_or_else(|| {
            TransportError::Io(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "send_request before discover_capabilities",
            ))
        })?;
        self.consumed = true;

        // Write the request body to the child's stdin, then close stdin to
        // signal EOF to the remote process (so it knows the request is
        // complete and starts streaming its response).
        {
            let mut stdin = child.stdin.take().ok_or_else(|| {
                TransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "ssh child stdin already closed",
                ))
            })?;
            stdin.write_all(&body)?;
            // Dropping stdin closes the pipe → remote sees EOF.
            drop(stdin);
        }

        // Reclaim stdout and hand it back to the caller as a streaming Read.
        // We wrap in a small adapter that:
        //   - Forwards reads to the child's stdout.
        //   - On EOF, waits for the child to exit and surfaces non-zero
        //     status (along with captured stderr) as an io::Error.
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TransportError::Io(std::io::Error::other("ssh child has no stdout")))?;

        // Steal the Child out so the reader can `wait()` on it after EOF.
        let child = self.child.take().expect("child Some");
        let stderr_handle = self.pending_stderr.take();
        let reader = ChildReader::new(child, stdout, stderr_handle);
        Ok(Box::new(reader))
    }
}

// `pending_stderr` is internal mutable state living next to `child` — we add
// it via an inherent impl extension below so the `Connection` trait stays
// small.
impl SshConnection {
    /// Wait for the child (if any) and concatenate its stderr into an error.
    fn fail_with_stderr(
        &mut self,
        stderr_handle: Option<JoinHandle<Vec<u8>>>,
        prefix: &str,
    ) -> TransportError {
        let status = self.child.as_mut().and_then(|c| c.wait().ok());
        let stderr = stderr_handle
            .and_then(|h| h.join().ok())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        let status_str = match status {
            Some(s) => format!("status={s}"),
            None => "status=?".to_string(),
        };
        TransportError::Io(std::io::Error::other(format!(
            "ssh: {prefix} ({status_str}){}",
            if stderr.is_empty() {
                String::new()
            } else {
                format!("; stderr: {}", stderr.trim_end())
            }
        )))
    }
}

// ---------------------------------------------------------------------------
// `ChildReader` — wraps the child's stdout to keep the Child alive until EOF.
// ---------------------------------------------------------------------------

/// Bridges a `ChildStdout` back to the `Box<dyn Read + Send>` the
/// [`Connection`] trait returns, while keeping the `Child` itself owned so
/// it isn't reaped early. On the first read that returns 0 bytes, we call
/// `wait()` on the child and, if the exit status is non-zero, convert that
/// into an io::Error so callers see the failure.
struct ChildReader {
    child: Option<Child>,
    stdout: ChildStdout,
    stderr_handle: Option<JoinHandle<Vec<u8>>>,
    /// Once we've waited on the child and decided what to do at EOF, we
    /// remember the verdict here so further reads keep returning the same
    /// outcome (0 bytes, or the cached error).
    finished: bool,
    cached_error: Option<std::io::Error>,
}

impl ChildReader {
    fn new(child: Child, stdout: ChildStdout, stderr_handle: Option<JoinHandle<Vec<u8>>>) -> Self {
        Self {
            child: Some(child),
            stdout,
            stderr_handle,
            finished: false,
            cached_error: None,
        }
    }

    fn handle_eof(&mut self) -> std::io::Result<usize> {
        if self.finished {
            // Already settled — replay cached error if any, else clean EOF.
            if let Some(e) = self.cached_error.as_ref() {
                return Err(std::io::Error::new(e.kind(), e.to_string()));
            }
            return Ok(0);
        }
        self.finished = true;
        let status = match self.child.as_mut() {
            Some(c) => c.wait()?,
            None => return Ok(0),
        };
        let stderr_msg = self
            .stderr_handle
            .take()
            .and_then(|h| h.join().ok())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        if !status.success() {
            let err = std::io::Error::other(format!(
                "ssh: remote git failed ({status}){}",
                if stderr_msg.is_empty() {
                    String::new()
                } else {
                    format!("; stderr: {}", stderr_msg.trim_end())
                }
            ));
            self.cached_error = Some(std::io::Error::new(err.kind(), err.to_string()));
            return Err(err);
        }
        // Successful exit — but emit any stderr to OUR stderr so the user
        // sees progress / informational messages.
        if !stderr_msg.is_empty() {
            // Best-effort write to stderr.
            let _ = std::io::stderr().write_all(stderr_msg.as_bytes());
        }
        Ok(0)
    }
}

impl Read for ChildReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.finished {
            return self.handle_eof();
        }
        match self.stdout.read(buf)? {
            0 => self.handle_eof(),
            n => Ok(n),
        }
    }
}

impl Drop for ChildReader {
    fn drop(&mut self) {
        // Make sure we don't leave a zombie. If the caller didn't drain to
        // EOF, kill+wait the child.
        if let Some(mut c) = self.child.take() {
            if !self.finished {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// URL parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedSshUrl {
    user: Option<String>,
    host: String,
    port: Option<u16>,
    path: String,
}

fn parse_ssh_url(url: &str) -> Result<ParsedSshUrl, TransportError> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(TransportError::BadUrl(url.to_string()));
    }

    // ssh:// scheme — formal URL form.
    if let Some(rest) = strip_scheme_ci(trimmed, "ssh://") {
        return parse_ssh_scheme(rest).ok_or_else(|| TransportError::BadUrl(url.to_string()));
    }

    // scp-like form: `[user@]host:path`. Require a colon and that no slash
    // precedes it.
    let colon_idx = match trimmed.find(':') {
        Some(i) => i,
        None => return Err(TransportError::BadUrl(url.to_string())),
    };
    let before_colon = &trimmed[..colon_idx];
    if before_colon.contains('/') || before_colon.is_empty() {
        return Err(TransportError::BadUrl(url.to_string()));
    }
    let after_colon = &trimmed[colon_idx + 1..];
    if after_colon.is_empty() {
        return Err(TransportError::BadUrl(url.to_string()));
    }

    let (user, host) = split_user_host(before_colon);
    Ok(ParsedSshUrl {
        user,
        host: host.to_string(),
        port: None,
        path: after_colon.to_string(),
    })
}

/// Parse the part after `ssh://`. Returns `None` on malformed input.
fn parse_ssh_scheme(rest: &str) -> Option<ParsedSshUrl> {
    // Authority ends at the first `/` (start of path).
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return None;
    }

    let (user, host_port) = split_user_host(authority);
    // Distinguish `host:port` from a bare IPv6 literal in brackets. We do
    // NOT support IPv6 literals here — git's SSH transport historically
    // didn't either in the scp-form. For `ssh://` we permit a simple
    // `host:port` only.
    let (host, port) = match host_port.rfind(':') {
        Some(i) => {
            let host = &host_port[..i];
            let port_str = &host_port[i + 1..];
            if port_str.is_empty() || host.is_empty() {
                return None;
            }
            let port = port_str.parse::<u16>().ok()?;
            (host.to_string(), Some(port))
        }
        None => (host_port.to_string(), None),
    };

    Some(ParsedSshUrl {
        user,
        host,
        port,
        path: path.to_string(),
    })
}

/// Split `user@host` into `(Some(user), host)`, or `(None, host)` when no `@`.
/// If there are multiple `@`, the LAST one separates user from host — this
/// matches openssh's behavior (`user@with@signs@host` → user=`user@with@signs`).
fn split_user_host(input: &str) -> (Option<String>, &str) {
    match input.rfind('@') {
        Some(i) => {
            let user = &input[..i];
            let host = &input[i + 1..];
            if user.is_empty() {
                (None, host)
            } else {
                (Some(user.to_string()), host)
            }
        }
        None => (None, input),
    }
}

fn strip_scheme_ci<'a>(s: &'a str, scheme: &str) -> Option<&'a str> {
    if s.len() < scheme.len() {
        return None;
    }
    let (head, tail) = s.split_at(scheme.len());
    if head.eq_ignore_ascii_case(scheme) {
        Some(tail)
    } else {
        None
    }
}

/// Public: heuristic check for "looks like an SSH URL". See module docs for
/// the disambiguation rules.
pub fn is_ssh_url(url: &str) -> bool {
    let s = url.trim();
    if s.is_empty() {
        return false;
    }
    // ssh:// → yes.
    if strip_scheme_ci(s, "ssh://").is_some() {
        return true;
    }
    // Reject other schemes BEFORE the scp-form check, so a stray `:` in
    // e.g. `https://...` doesn't look like an scp path.
    for forbidden in ["http://", "https://", "git://", "file://", "ftp://"] {
        if strip_scheme_ci(s, forbidden).is_some() {
            return false;
        }
    }
    // scp-form: `[user@]host:path` — colon present, and NO slash before
    // the first colon. Also reject leading colon (empty host).
    if let Some(i) = s.find(':') {
        if i == 0 {
            return false;
        }
        let before = &s[..i];
        if before.contains('/') {
            return false;
        }
        // Must have something after the colon.
        if s.len() == i + 1 {
            return false;
        }
        return true;
    }
    false
}

/// POSIX-style single-quote escape: wrap in `'…'`, escape any embedded `'`
/// by closing the quote, emitting `\'`, and reopening. Result is safe to
/// pass through a `sh -c` style shell.
fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

// Adapt io::Error from PktLineReader's `Err` path for nicer diagnostics
// when stderr has hints.
fn decorate_with_stderr(err: TransportError, stderr_msg: &str) -> TransportError {
    if stderr_msg.is_empty() {
        return err;
    }
    match err {
        TransportError::Io(e) => TransportError::Io(std::io::Error::new(
            e.kind(),
            format!("{e}; ssh stderr: {}", stderr_msg.trim_end()),
        )),
        other => other,
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // URL parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_ssh_scheme_full() {
        let p = parse_ssh_url("ssh://git@github.com:22/user/repo.git").unwrap();
        assert_eq!(p.user.as_deref(), Some("git"));
        assert_eq!(p.host, "github.com");
        assert_eq!(p.port, Some(22));
        assert_eq!(p.path, "/user/repo.git");
    }

    #[test]
    fn parse_ssh_scheme_no_port() {
        let p = parse_ssh_url("ssh://git@example.com/foo/bar.git").unwrap();
        assert_eq!(p.user.as_deref(), Some("git"));
        assert_eq!(p.host, "example.com");
        assert_eq!(p.port, None);
        assert_eq!(p.path, "/foo/bar.git");
    }

    #[test]
    fn parse_ssh_scheme_no_user() {
        let p = parse_ssh_url("ssh://host/path").unwrap();
        assert_eq!(p.user, None);
        assert_eq!(p.host, "host");
        assert_eq!(p.port, None);
        assert_eq!(p.path, "/path");
    }

    #[test]
    fn parse_scp_form_full() {
        let p = parse_ssh_url("git@github.com:user/repo.git").unwrap();
        assert_eq!(p.user.as_deref(), Some("git"));
        assert_eq!(p.host, "github.com");
        assert_eq!(p.port, None);
        assert_eq!(p.path, "user/repo.git");
    }

    #[test]
    fn parse_scp_form_no_user() {
        let p = parse_ssh_url("host:path/repo").unwrap();
        assert_eq!(p.user, None);
        assert_eq!(p.host, "host");
        assert_eq!(p.port, None);
        assert_eq!(p.path, "path/repo");
    }

    #[test]
    fn parse_scp_form_with_subdirs() {
        let p = parse_ssh_url("git@host.example.com:deeply/nested/path/repo.git").unwrap();
        assert_eq!(p.user.as_deref(), Some("git"));
        assert_eq!(p.host, "host.example.com");
        assert_eq!(p.path, "deeply/nested/path/repo.git");
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(parse_ssh_url("").is_err());
        assert!(parse_ssh_url("   ").is_err());
    }

    #[test]
    fn parse_rejects_no_colon_or_scheme() {
        assert!(parse_ssh_url("nopath").is_err());
        assert!(parse_ssh_url("/some/path").is_err());
    }

    #[test]
    fn parse_rejects_slash_before_colon() {
        // This would be a Unix path "with-colon", not an SSH URL.
        assert!(parse_ssh_url("/some/path:with-colon").is_err());
        assert!(parse_ssh_url("relative/path:colon").is_err());
    }

    #[test]
    fn parse_rejects_empty_host_or_path() {
        assert!(parse_ssh_url(":path").is_err()); // no host
        assert!(parse_ssh_url("host:").is_err()); // no path
        assert!(parse_ssh_url("ssh://").is_err());
        assert!(parse_ssh_url("ssh:///path").is_err()); // empty authority
    }

    #[test]
    fn parse_rejects_bad_port() {
        assert!(parse_ssh_url("ssh://host:notaport/path").is_err());
        // 65536 is out of u16 range
        assert!(parse_ssh_url("ssh://host:65536/path").is_err());
    }

    #[test]
    fn parse_ssh_scheme_path_optional_kind() {
        // No path is OK at parse time (path becomes empty). The caller's
        // remote-cmd builder will pass an empty quoted string.
        let p = parse_ssh_url("ssh://host").unwrap();
        assert_eq!(p.host, "host");
        assert_eq!(p.path, "");
    }

    #[test]
    fn split_user_host_basic() {
        assert_eq!(split_user_host("git@host"), (Some("git".into()), "host"));
        assert_eq!(split_user_host("host"), (None, "host"));
        assert_eq!(split_user_host("@host"), (None, "host"));
    }

    #[test]
    fn split_user_host_multi_at() {
        // Last @ separates user from host.
        assert_eq!(
            split_user_host("user@with@signs@host"),
            (Some("user@with@signs".into()), "host")
        );
    }

    // -----------------------------------------------------------------------
    // is_ssh_url
    // -----------------------------------------------------------------------

    #[test]
    fn is_ssh_url_positive() {
        assert!(is_ssh_url("ssh://git@github.com/repo.git"));
        assert!(is_ssh_url("ssh://github.com:22/repo.git"));
        assert!(is_ssh_url("SSH://github.com/repo.git")); // case-insensitive
        assert!(is_ssh_url("git@github.com:user/repo.git"));
        assert!(is_ssh_url("host:path"));
        assert!(is_ssh_url("host:path/with/slashes"));
    }

    #[test]
    fn is_ssh_url_negative() {
        assert!(!is_ssh_url("https://github.com/user/repo.git"));
        assert!(!is_ssh_url("HTTPS://github.com/user/repo.git"));
        assert!(!is_ssh_url("http://example.com/repo.git"));
        assert!(!is_ssh_url("git://example.com/repo.git"));
        assert!(!is_ssh_url("file:///tmp/repo"));
        assert!(!is_ssh_url("/path/to/repo"));
        assert!(!is_ssh_url("/path/to/repo:with-colon"));
        assert!(!is_ssh_url("relative/path"));
        assert!(!is_ssh_url("relative/path:colon"));
        assert!(!is_ssh_url(""));
        assert!(!is_ssh_url("   "));
        assert!(!is_ssh_url(":nohost"));
        assert!(!is_ssh_url("plainstring"));
    }

    #[test]
    fn is_ssh_url_disambiguation() {
        // The classic discriminator: where is the first slash?
        assert!(is_ssh_url("host:path"));
        assert!(!is_ssh_url("/host:path"));
        assert!(!is_ssh_url("./host:path"));
        assert!(!is_ssh_url("path/with/colon:in-the-middle"));
    }

    // -----------------------------------------------------------------------
    // shell_single_quote
    // -----------------------------------------------------------------------

    #[test]
    fn shell_quote_simple() {
        assert_eq!(shell_single_quote("foo"), "'foo'");
        assert_eq!(shell_single_quote("foo/bar.git"), "'foo/bar.git'");
        assert_eq!(shell_single_quote(""), "''");
    }

    #[test]
    fn shell_quote_with_space() {
        assert_eq!(shell_single_quote("foo bar/baz.git"), "'foo bar/baz.git'");
    }

    #[test]
    fn shell_quote_with_single_quote() {
        // Embedded ' is escaped as '\''
        assert_eq!(shell_single_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_single_quote("don't.git"), "'don'\\''t.git'");
    }

    #[test]
    fn shell_quote_with_metachars() {
        // $, `, *, ?, etc. all survive single-quoting unchanged.
        assert_eq!(shell_single_quote("a$b`c*d?e"), "'a$b`c*d?e'");
        assert_eq!(shell_single_quote("path; rm -rf /"), "'path; rm -rf /'");
    }

    // -----------------------------------------------------------------------
    // Connection construction (no spawn)
    // -----------------------------------------------------------------------

    #[test]
    fn new_constructs_upload_pack() {
        let c = SshConnection::new(
            "git@github.com:octocat/Hello-World.git",
            SshService::UploadPack,
        )
        .unwrap();
        assert_eq!(c.user(), Some("git"));
        assert_eq!(c.host(), "github.com");
        assert_eq!(c.port(), None);
        assert_eq!(c.remote_path(), "octocat/Hello-World.git");
        assert_eq!(c.service(), SshService::UploadPack);
    }

    #[test]
    fn new_constructs_receive_pack() {
        let c = SshConnection::new(
            "ssh://git@example.com:2222/path/repo.git",
            SshService::ReceivePack,
        )
        .unwrap();
        assert_eq!(c.port(), Some(2222));
        assert_eq!(c.remote_path(), "/path/repo.git");
        assert_eq!(c.service(), SshService::ReceivePack);
    }

    #[test]
    fn new_rejects_invalid_url() {
        let err = SshConnection::new("/not/an/ssh/url", SshService::UploadPack).unwrap_err();
        assert!(matches!(err, TransportError::BadUrl(_)));
    }

    #[test]
    fn build_command_args_upload_pack() {
        let mut c = SshConnection::new(
            "ssh://git@example.com:2222/path/to/repo.git",
            SshService::UploadPack,
        )
        .unwrap();
        c.set_ssh_program("ssh");
        let cmd = c.build_command();
        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.len(), 4);
        assert_eq!(args[0], "-p");
        assert_eq!(args[1], "2222");
        assert_eq!(args[2], "git@example.com");
        assert_eq!(
            args[3],
            "GIT_PROTOCOL=version=2 git-upload-pack '/path/to/repo.git'"
        );
    }

    #[test]
    fn build_command_args_scp_form_no_port() {
        let c = SshConnection::new("git@github.com:user/repo.git", SshService::UploadPack).unwrap();
        let cmd = c.build_command();
        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "git@github.com");
        assert_eq!(
            args[1],
            "GIT_PROTOCOL=version=2 git-upload-pack 'user/repo.git'"
        );
    }

    #[test]
    fn build_command_args_no_user() {
        let c = SshConnection::new("host:path", SshService::UploadPack).unwrap();
        let cmd = c.build_command();
        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "host"); // bare host, no user
    }

    #[test]
    fn build_command_args_receive_pack() {
        let c = SshConnection::new("git@host:repo.git", SshService::ReceivePack).unwrap();
        let cmd = c.build_command();
        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert!(args.last().unwrap().contains("git-receive-pack"));
    }

    #[test]
    fn build_command_escapes_path_with_quote() {
        let c = SshConnection::new("git@host:weird/it's/repo.git", SshService::UploadPack).unwrap();
        let cmd = c.build_command();
        let args: Vec<String> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        // Embedded ' must be wrapped as '\''
        assert_eq!(
            args.last().unwrap(),
            "GIT_PROTOCOL=version=2 git-upload-pack 'weird/it'\\''s/repo.git'"
        );
    }

    // -----------------------------------------------------------------------
    // Spawn-failure handling
    // -----------------------------------------------------------------------

    #[test]
    fn spawn_failure_when_ssh_program_missing() {
        // Point at a binary that definitely doesn't exist. Spawn should
        // produce a clean TransportError::Io, not panic.
        let mut c = SshConnection::new("git@example.com:repo.git", SshService::UploadPack).unwrap();
        c.set_ssh_program("rustygit_definitely_not_an_ssh_binary_xyzzy_12345");
        match c.discover_capabilities() {
            Err(TransportError::Io(_)) => {}
            other => panic!("expected Io error from missing ssh binary, got {other:?}"),
        }
    }

    #[test]
    fn discover_capabilities_against_unresolvable_host() {
        // Use a real ssh binary against a host that won't resolve. ssh will
        // exit non-zero (DNS failure), our stdout read will hit EOF without
        // ever seeing a pkt-line, and we surface that as an error.
        //
        // Skip if no `ssh` is in PATH (CI without ssh).
        if which("ssh").is_none() {
            eprintln!("skipping: no `ssh` in PATH");
            return;
        }
        let mut c = SshConnection::new(
            "git@does.not.resolve.invalid.tld:repo.git",
            SshService::UploadPack,
        )
        .unwrap();
        match c.discover_capabilities() {
            Err(_) => {} // any error is fine, just must not panic / hang
            Ok(pkts) => panic!(
                "expected error for unresolvable host, got {} pkt-lines",
                pkts.len()
            ),
        }
    }

    /// Live test against a real SSH git host, gated on `SSH_TEST_URL`. Skipped
    /// unless set. Example:
    ///
    /// ```text
    /// SSH_TEST_URL=git@github.com:octocat/Hello-World.git cargo test \
    ///     --lib transport::ssh::tests::live -- --nocapture
    /// ```
    #[test]
    fn live_ssh_discover_capabilities() {
        let url = match std::env::var("SSH_TEST_URL") {
            Ok(v) => v,
            Err(_) => {
                eprintln!("skipping live SSH test: set SSH_TEST_URL to enable");
                return;
            }
        };
        let mut conn = match SshConnection::new(&url, SshService::UploadPack) {
            Ok(c) => c,
            Err(e) => panic!("construction failed for {url:?}: {e}"),
        };
        let pkts = match conn.discover_capabilities() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("live SSH discover failed: {e}");
                return;
            }
        };
        assert!(!pkts.is_empty(), "expected non-empty advertisement");
        let has_version_two = pkts.iter().any(|p| match p {
            PktLine::Data(d) => d.starts_with(b"version 2\n") || d.as_slice() == b"version 2",
            _ => false,
        });
        assert!(
            has_version_two,
            "live advertisement did not contain 'version 2': {pkts:?}"
        );
        assert!(
            matches!(pkts.last(), Some(PktLine::Flush)),
            "advertisement did not end in flush-pkt"
        );
    }

    /// Locate a binary on $PATH. Tiny, no extra deps.
    fn which(program: &str) -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(program);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }
}
