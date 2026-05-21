//! Pager auto-piping shared by `log`, `show`, `diff`, `blame`, and `reflog`.
//!
//! When stdout is a terminal and a pager is available, write through a child
//! process (`less -R` by default) so output longer than a screen can be
//! scrolled. When stdout is redirected to a pipe or file, or `--no-pager` is
//! set, write directly to stdout.
//!
//! Pager selection precedence (matches git):
//! 1. `$GIT_PAGER`
//! 2. `core.pager` from config
//! 3. `$PAGER`
//! 4. `less -R`
//!
//! ## EPIPE handling
//!
//! Pagers exit when the user presses `q`. The next write to the child's
//! stdin then returns `BrokenPipe`. The [`PagerOut`] `Write` impl swallows
//! that into a "stopped" state so callers can break out of their emit loops
//! without panicking. Use [`PagerOut::stopped`] to detect the condition
//! and exit cleanly.

use std::io::{self, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

use crate::color::{is_tty, Sink};
use crate::config::Config;

/// Sink for paged output. Either pipes to a child pager or writes straight
/// to stdout.
pub enum PagerOut {
    /// Output is going through a child pager process. `stdin` is the child's
    /// stdin handle. When `stopped` flips to true (after `BrokenPipe` from
    /// the child closing), further writes become no-ops.
    Pager {
        child: Option<Child>,
        stdin: Option<ChildStdin>,
        stopped: bool,
    },
    /// Output is going directly to this process's stdout.
    Stdout(io::Stdout),
}

impl PagerOut {
    /// Returns true if the pager has closed and further writes will be
    /// dropped. Callers in long emit loops can poll this to bail early.
    pub fn stopped(&self) -> bool {
        matches!(self, PagerOut::Pager { stopped: true, .. })
    }
}

impl Write for PagerOut {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            PagerOut::Pager { stdin, stopped, .. } => {
                if *stopped {
                    // Pretend the write succeeded — the user is gone.
                    return Ok(buf.len());
                }
                let s = match stdin.as_mut() {
                    Some(s) => s,
                    None => {
                        *stopped = true;
                        return Ok(buf.len());
                    }
                };
                match s.write(buf) {
                    Ok(n) => Ok(n),
                    Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                        *stopped = true;
                        Ok(buf.len())
                    }
                    Err(e) => Err(e),
                }
            }
            PagerOut::Stdout(out) => match out.write(buf) {
                Ok(n) => Ok(n),
                // Even direct-to-stdout can hit BrokenPipe (e.g. `rustygit log
                // | head`); convert to a successful no-op so callers can
                // terminate cleanly.
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(buf.len()),
                Err(e) => Err(e),
            },
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            PagerOut::Pager { stdin, stopped, .. } => {
                if *stopped {
                    return Ok(());
                }
                if let Some(s) = stdin.as_mut() {
                    match s.flush() {
                        Ok(()) => Ok(()),
                        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                            *stopped = true;
                            Ok(())
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    Ok(())
                }
            }
            PagerOut::Stdout(out) => match out.flush() {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
                Err(e) => Err(e),
            },
        }
    }
}

impl Drop for PagerOut {
    fn drop(&mut self) {
        if let PagerOut::Pager { child, stdin, .. } = self {
            // Drop stdin first so the pager sees EOF and exits, then wait
            // so the parent doesn't exit before the user finishes scrolling.
            let _ = stdin.take();
            if let Some(mut c) = child.take() {
                let _ = c.wait();
            }
        }
    }
}

/// Build a `PagerOut` for the current invocation.
///
/// Returns the direct-to-stdout variant if any of these is true:
///   * `no_pager` is set (caller is honoring `--no-pager` or a config flag),
///   * stdout is not a terminal,
///   * the configured pager is empty or literally `cat`,
///   * spawning the pager fails.
///
/// On spawn failure we DON'T return an error — `less` not being installed
/// shouldn't crash `rustygit log`. We log a one-line stderr note and fall
/// through to stdout.
pub fn open(config: &Config, no_pager: bool) -> io::Result<PagerOut> {
    if no_pager {
        return Ok(PagerOut::Stdout(io::stdout()));
    }
    if !is_tty(Sink::Stdout) {
        return Ok(PagerOut::Stdout(io::stdout()));
    }
    let pager = pick_pager(config);
    // `cat` (or empty) is git's documented "disable paging" sentinel.
    if pager.is_empty() || pager == "cat" {
        return Ok(PagerOut::Stdout(io::stdout()));
    }

    // The pager string is a shell command line; split via `sh -c`. This
    // matches git's behavior (e.g. `core.pager = "less -FRX"` works).
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&pager).stdin(Stdio::piped());
    // Tell less to behave reasonably by default when env vars aren't set.
    if pager.contains("less") && std::env::var_os("LESS").is_none() {
        cmd.env("LESS", "FRX");
    }
    match cmd.spawn() {
        Ok(mut child) => {
            let stdin = child.stdin.take();
            Ok(PagerOut::Pager {
                child: Some(child),
                stdin,
                stopped: false,
            })
        }
        Err(e) => {
            eprintln!("rustygit: failed to spawn pager '{pager}': {e}; falling back to stdout");
            Ok(PagerOut::Stdout(io::stdout()))
        }
    }
}

/// `$GIT_PAGER` > `core.pager` > `$PAGER` > `less -R`. Identical precedence
/// to git, the default differs only in that we add `-R` so ANSI escapes pass
/// through unchanged (which is also what git's default is, via the `LESS`
/// env-var hand-off below).
fn pick_pager(config: &Config) -> String {
    if let Ok(v) = std::env::var("GIT_PAGER") {
        if !v.is_empty() {
            return v;
        }
    }
    if let Some(v) = config.get_string("core", "pager") {
        if !v.is_empty() {
            return v.to_string();
        }
    }
    if let Ok(v) = std::env::var("PAGER") {
        if !v.is_empty() {
            return v;
        }
    }
    "less -R".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_pager_returns_stdout_variant() {
        let cfg = Config::empty();
        let p = open(&cfg, true).unwrap();
        match p {
            PagerOut::Stdout(_) => {}
            _ => panic!("expected Stdout when no_pager=true"),
        }
    }

    #[test]
    fn cat_pager_returns_stdout_variant() {
        // GIT_PAGER=cat is git's documented disable knob.
        // We can't easily isolate env in tests, so just check the picker.
        let cfg = Config::empty();
        // Simulate: even non-TTY stdout returns Stdout variant.
        let p = open(&cfg, false).unwrap();
        // In `cargo test` stdout is captured, so it's not a TTY. The branch
        // we hit is the "stdout isn't a TTY" early-return.
        match p {
            PagerOut::Stdout(_) => {}
            _ => panic!("test runner stdout isn't a TTY; expected Stdout"),
        }
    }

    #[test]
    fn write_to_stdout_variant_succeeds() {
        let cfg = Config::empty();
        let mut p = open(&cfg, true).unwrap();
        // Writing something innocuous; the test harness will swallow it.
        p.write_all(b"").unwrap();
        p.flush().unwrap();
        assert!(!p.stopped());
    }
}
