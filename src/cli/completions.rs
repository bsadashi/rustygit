//! `rustygit completions <shell>` and `rustygit manpage` — generate shell
//! completions and the man page on demand by introspecting the live clap
//! `Cli` struct.
//!
//! Both subcommands are HIDDEN from `--help`: they exist for the release
//! workflow (and curious users), not for everyday porcelain. The output
//! goes to stdout so the caller can redirect to whatever path the host
//! distribution expects:
//!
//! ```sh
//! rustygit completions bash > /usr/share/bash-completion/completions/rustygit
//! rustygit completions zsh  > /usr/share/zsh/site-functions/_rustygit
//! rustygit completions fish > /usr/share/fish/vendor_completions.d/rustygit.fish
//! rustygit manpage          > /usr/share/man/man1/rustygit.1
//! ```
//!
//! Implementation note: we deliberately do NOT use a `build.rs`. The clap
//! `Cli` struct lives in the crate itself and `build.rs` cannot import it
//! (build scripts run before the library is compiled). Doing this at
//! runtime via `clap::CommandFactory` is the standard idiom and keeps the
//! single source of truth in `src/cli/mod.rs`. `release.yml` invokes the
//! freshly-built binary to produce the artifacts. See NON_GOALS B4.

use std::io::{self, ErrorKind, Write};

use clap::{Args, CommandFactory};
use clap_complete::{generate, Shell};
use clap_mangen::Man;

#[derive(Debug, Args)]
pub struct CompletionsArgs {
    /// Target shell. One of: bash, zsh, fish, powershell, elvish.
    #[arg(value_enum)]
    pub shell: Shell,
}

#[derive(Debug, Args)]
pub struct ManpageArgs {
    // No flags. Reserved for future `--section`, `--name` overrides if we
    // ever need to render multiple pages (one per subcommand).
}

/// Write a buffer to stdout, swallowing `EPIPE` (`BrokenPipe`).
///
/// `clap_complete::generate` and `clap_mangen::Man::render` are large
/// writes. If the user pipes our output into `head` (or any consumer
/// that closes early), the OS reports `EPIPE` and `Write::write_all`
/// returns `BrokenPipe`. That's not an error for us — the consumer is
/// happy with what it has — so we exit 0 silently rather than wrapping
/// it in the panic-preamble bug-report dance.
fn write_stdout_ignoring_epipe(buf: &[u8]) -> io::Result<()> {
    match io::stdout().write_all(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e),
    }
}

/// `rustygit completions <shell>` — write a completion script to stdout.
///
/// We render into an in-memory `Vec<u8>` first so the call site can deal
/// with `EPIPE` itself; `clap_complete::generate` panics on stdout-write
/// failure when handed a `&mut io::Stdout` directly.
pub fn run_completions(args: CompletionsArgs) -> io::Result<i32> {
    let mut cmd = super::Cli::command();
    let name = cmd.get_name().to_string();
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    generate(args.shell, &mut cmd, name, &mut buf);
    write_stdout_ignoring_epipe(&buf)?;
    Ok(super::EXIT_OK)
}

/// `rustygit manpage` — write a troff(1)-formatted man page to stdout.
///
/// The output is a single `rustygit.1` page covering the top-level command
/// plus subcommand summaries. Each subcommand's full `--help` is NOT
/// expanded into its own page (would be ~120 separate `man` files);
/// callers who want that can run `clap_mangen` themselves, or just use
/// `rustygit <subcmd> --help`.
pub fn run_manpage(_args: ManpageArgs) -> io::Result<i32> {
    let cmd = super::Cli::command();
    let man = Man::new(cmd);
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    man.render(&mut buf)?;
    write_stdout_ignoring_epipe(&buf)?;
    Ok(super::EXIT_OK)
}
