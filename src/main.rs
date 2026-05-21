use std::process::ExitCode;

use clap::Parser;
use rustygit::cli::{dispatch, explain_unsupported_subcommand, Cli};

fn main() -> ExitCode {
    install_panic_handler();
    install_sigint_cleanup();

    // Intercept the explicit-non-goal subcommand names BEFORE clap rejects them
    // with a generic "unrecognized subcommand" error. We want a concrete
    // explanation of why rustygit doesn't ship the command and what to use
    // instead. Skip ahead to clap parsing for everything else.
    let mut argv: Vec<String> = std::env::args().collect();

    // Subcommand aliases — translate before any parsing so the
    // user-facing UX is identical to upstream git.
    if let Some(name) = argv.get_mut(1) {
        match name.as_str() {
            "annotate" => *name = "blame".to_string(),
            "init-db" => *name = "init".to_string(),
            "gui" | "git-gui" => *name = "git-gui".to_string(),
            "git-svn" => *name = "svn".to_string(),
            "git-p4" => *name = "p4".to_string(),
            "git-instaweb" => *name = "instaweb".to_string(),
            _ => {}
        }
    }
    // Run i18n catalog load once.
    rustygit::cli::i18n_load::init();

    // Strip `--i-know-this-is-beta` from argv and (on `-beta` builds, when
    // no acknowledgement is set in config) print a one-line stderr banner
    // pointing at BETA.md. GA tags (versions without `-beta`) no-op this
    // call — the banner drops automatically. See `cli/beta.rs`.
    rustygit::cli::beta::emit_beta_banner_if_unacknowledged(&mut argv);

    if let Some(name) = argv.get(1) {
        if let Some(message) = explain_unsupported_subcommand(name) {
            eprintln!("{message}");
            return ExitCode::from(128);
        }
    }

    // Bare `--exec-path` (no subcommand) is a print-and-exit, like real git.
    // Clap requires a subcommand on Cli so we intercept here before parsing.
    // Note: this handles ONLY the no-subcommand form. `--exec-path=<dir>` and
    // `--exec-path <dir> <subcmd>` route through normal clap parsing.
    if argv.len() == 2 && argv[1] == "--exec-path" {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("rustygit: --exec-path: current_exe: {e}");
                return ExitCode::from(128);
            }
        };
        let dir = exe.parent().unwrap_or_else(|| std::path::Path::new("."));
        println!("{}", dir.display());
        return ExitCode::from(0);
    }

    // `[alias]` config expansion — before clap parses, so `rustygit st` (with
    // `alias.st = status` in `~/.gitconfig`) routes to the right subcommand
    // instead of getting clap's "unrecognized subcommand" error. See
    // `src/cli/alias.rs` for the algorithm.
    //
    // The config we read here is the LAYERED view (system + XDG + global +
    // local + `-c`). For users not in a repo, the local layer no-ops; the
    // alias still resolves from `~/.gitconfig`. We don't apply `-c` overrides
    // here because they aren't registered until `dispatch` runs — aliases
    // defined ONLY via `-c` at the command line are a niche we'd need to
    // scan argv twice to support, and nobody does that in practice.
    {
        // Pick a gitdir candidate for the local-layer read. If we're inside a
        // repo, `Repository::discover_from_cwd` finds it; otherwise we fall
        // back to a path that won't exist, so the local-layer read is a
        // no-op and we still get the global/XDG/system layers.
        let gitdir = rustygit::repo::Repository::discover_from_cwd()
            .map(|r| r.gitdir().to_path_buf())
            .unwrap_or_else(|_| std::path::PathBuf::from(".git"));
        if let Ok(cfg) = rustygit::config::Config::load_layered(&gitdir) {
            match rustygit::cli::alias::expand(&mut argv, &cfg) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("rustygit: alias: {e}");
                    return ExitCode::from(128);
                }
            }
        }
    }

    let cli = Cli::parse_from(argv);
    match dispatch(cli) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("rustygit: {e}");
            ExitCode::from(128)
        }
    }
}

/// Install a SIGINT handler that drains the live-lock registry and unlinks
/// every outstanding `.lock` file before exiting with 130 (`128 + SIGINT(2)`,
/// the conventional shell exit code for SIGINT).
///
/// This is the second half of the crash-safe-lockfile contract (A9a): plain
/// `Drop` handles `?`-style early-returns and panics that don't bypass
/// unwinding, but Ctrl-C kills the process before any destructor runs, so we
/// have to clean up explicitly.
///
/// The `ctrlc` crate spawns a dedicated thread to handle the signal, so the
/// handler body runs in normal user space — no async-signal-safety dance
/// needed. We don't try to gracefully unwind the in-flight operation; users
/// of `git` expect Ctrl-C to behave like `kill -INT`, leaving the repo in a
/// consistent (pre-operation) state.
///
/// Windows is a no-op for now. The `ctrlc` crate does support Windows Console
/// Ctrl-C events, but the rest of the file-locking story on Windows has
/// rename-during-open peculiarities we'd rather not have a half-baked story
/// for. Users on Windows can still run `rustygit prune-locks` after a kill.
#[cfg(unix)]
fn install_sigint_cleanup() {
    // Ignore the Result — if the handler is already installed (e.g. when a
    // test framework wraps the binary), there's nothing we can do about it
    // and the existing handler is presumed to do its job. We don't want a
    // hard error on the cold path of an integration test.
    let _ = ctrlc::set_handler(|| {
        if let Ok(locks) = rustygit::lockfile::take_live_locks() {
            for path in locks {
                let _ = std::fs::remove_file(&path);
            }
        }
        std::process::exit(130);
    });
}

#[cfg(not(unix))]
fn install_sigint_cleanup() {
    // Windows path: no-op. See the comment on the unix arm.
}

/// Replace Rust's default panic-with-backtrace output with a concise,
/// user-actionable message. Users running a CLI shouldn't see a "thread
/// 'main' panicked at..." stack trace; they should see "rustygit hit an
/// internal error; please file a bug at <link>" and the file:line where it
/// happened so a maintainer can triage.
///
/// `RUST_BACKTRACE=1` still gets the full trace via the default chained
/// hook, so contributors aren't blocked.
fn install_panic_handler() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!(" at {}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_default();
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(|s| s.as_str()))
            .unwrap_or("(no message)");
        eprintln!(
            "rustygit: internal error: {msg}{location}\n\
             this is a bug — please report it at https://github.com/bsadashi/rustygit/issues\n\
             If this is a bug, rerun with `rustygit bug-report` and paste the output.\n\
             (set RUST_BACKTRACE=1 for a full stack trace)"
        );
        if std::env::var_os("RUST_BACKTRACE").is_some() {
            default_hook(info);
        }
    }));
}
