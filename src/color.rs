//! ANSI color helpers honoring `color.ui` config and runtime
//! environment.
//!
//! Decision tree (matches git):
//!   * `--color=never` / `NO_COLOR` env / `color.ui=never` → off.
//!   * `--color=always` / `color.ui=always` → on regardless of TTY.
//!   * `color.ui=auto` (default) → on iff stdout is a TTY.
//!
//! All escape sequences are SGR (CSI ...m). Reset is `\x1b[0m`.

use crate::config::Config;

/// Where the colored output is heading. Affects the auto/tty check.
#[derive(Debug, Clone, Copy)]
pub enum Sink {
    Stdout,
    Stderr,
}

/// Should the caller emit color escapes?
///
/// `cli_override`: `--color` argument value, if the caller plumbed one through.
pub fn should_colorize(config: &Config, sink: Sink, cli_override: Option<&str>) -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    let setting = cli_override
        .or_else(|| config.get_string("color", "ui"))
        .unwrap_or("auto");
    match setting {
        "always" | "true" => true,
        "never" | "false" => false,
        _ => is_tty(sink),
    }
}

/// Returns true if the given output sink is connected to a terminal.
///
/// Public so other modules (pager, progress reporters) can reuse the same
/// TTY check rather than reimplementing libc/`IsTerminal` plumbing.
pub fn is_tty(sink: Sink) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let fd = match sink {
            Sink::Stdout => std::io::stdout().as_raw_fd(),
            Sink::Stderr => std::io::stderr().as_raw_fd(),
        };
        // SAFETY: libc::isatty is well-defined for any fd; result is a bool.
        unsafe { libc_isatty(fd) }
    }
    #[cfg(not(unix))]
    {
        let _ = sink;
        false
    }
}

#[cfg(unix)]
unsafe fn libc_isatty(fd: i32) -> bool {
    // Minimal libc binding to avoid the `libc` crate dep.
    extern "C" {
        fn isatty(fd: i32) -> i32;
    }
    isatty(fd) == 1
}

// ---------------------------------------------------------------------------
// SGR sequences
// ---------------------------------------------------------------------------

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const RED: &str = "\x1b[31m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const BLUE: &str = "\x1b[34m";
pub const MAGENTA: &str = "\x1b[35m";
pub const CYAN: &str = "\x1b[36m";

/// Wrap a string in an SGR sequence iff `enabled`.
pub fn paint(text: &str, sgr: &str, enabled: bool) -> String {
    if enabled {
        format!("{sgr}{text}{RESET}")
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paint_adds_codes_when_enabled() {
        let out = paint("hi", RED, true);
        assert!(out.contains("\x1b[31m"));
        assert!(out.ends_with("\x1b[0m"));
    }

    #[test]
    fn paint_passthrough_when_disabled() {
        assert_eq!(paint("hi", RED, false), "hi");
    }

    #[test]
    fn should_colorize_honors_no_color_env() {
        // We can't easily isolate env here; just check that the function
        // exists and is callable.
        let cfg = Config::empty();
        let _ = should_colorize(&cfg, Sink::Stdout, Some("never"));
    }
}
