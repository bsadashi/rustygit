//! Beta-banner emission.
//!
//! While the binary's Cargo version contains `-beta`, every invocation
//! prints a single reminder line to **stderr** explaining where to find
//! the known-divergence list and how to silence the banner. The banner
//! drops automatically once the build tag no longer contains `-beta`,
//! so GA tags pay no cost.
//!
//! The emission is split into a pure decision function
//! ([`should_emit_banner`]) and a thin side-effecting wrapper
//! ([`emit_beta_banner_if_unacknowledged`]). The pure function is
//! unit-tested below; the wrapper is what `main` calls.
//!
//! ## Acknowledgement
//!
//! Three ways to silence the banner:
//! 1. Build a non-beta version (i.e. `CARGO_PKG_VERSION` doesn't contain
//!    `-beta`).
//! 2. Set `rustygit.beta.acknowledged = true` in any config layer
//!    (system / XDG / global / local).
//! 3. Pass `--i-know-this-is-beta` on the command line. This flag is
//!    stripped from argv before clap sees it, so subcommands don't have
//!    to know it exists.
//!
//! ## Why STDERR
//!
//! Banner pollution on stdout would break every `rustygit log | …`
//! pipeline. Stderr is the channel for human-facing diagnostics that
//! must not corrupt machine-readable output.
//!
//! ## Once-per-process
//!
//! A `OnceLock<()>` guard ensures we never print the banner twice in a
//! single process, even if some future call-site invokes the wrapper
//! more than once (e.g. an in-process test harness driving multiple
//! subcommands).
//!
//! ## Module isolation
//!
//! This module lives under `src/cli/` rather than `src/main.rs` for the
//! same reason every other piece of CLI logic does — `main.rs` stays a
//! thin entry point, and the testable surface stays in the library
//! crate where `cargo test` can reach it without spawning a binary.

use std::sync::OnceLock;

use crate::config::Config;

/// The flag that, when present in argv, silences the banner for the
/// current invocation and is stripped before clap sees argv.
pub const ACK_FLAG: &str = "--i-know-this-is-beta";

/// Config storage for the persistent acknowledgement, accessed by the
/// user as `rustygit config --global rustygit.beta.acknowledged true`.
///
/// rustygit's config parser (and git's) treat a three-part dotted key
/// as `section.subsection.name`, so the on-disk representation written
/// by `git config rustygit.beta.acknowledged true` is:
///
/// ```text
/// [rustygit "beta"]
///     acknowledged = true
/// ```
///
/// We mirror that shape on the read side via [`Config::get_string_sub`].
pub const SECTION: &str = "rustygit";
pub const SUBSECTION: &str = "beta";
pub const KEY: &str = "acknowledged";

/// Once-per-process guard. We never want to print the banner twice in a
/// single rustygit invocation.
static WARNED: OnceLock<()> = OnceLock::new();

/// Parse a config value as a git boolean. Mirrors the spellings the
/// in-crate `config::parse_bool` accepts (case-insensitive
/// `true|yes|on|1` / `false|no|off|0|""`). Kept private to this module
/// because the upstream helper isn't exported.
fn parse_git_bool(s: &str) -> Option<bool> {
    let t = s.trim();
    match t.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" | "" => Some(false),
        _ => None,
    }
}

/// Pure decision function. Returns `true` iff the banner should be
/// emitted given the build version, the argv (with the ack flag still
/// present, if it was passed), and an optional config snapshot.
///
/// Decision rules, in order:
/// 1. If `version` doesn't contain `"-beta"`, return `false`. GA tags
///    never emit.
/// 2. If `argv` contains `--i-know-this-is-beta`, return `false`. The
///    caller is responsible for stripping the flag before clap sees it.
/// 3. If the config has `[rustygit "beta"] acknowledged = <truthy>`,
///    return `false`. We use [`Config::get_string_sub`] + a local bool
///    parser because there is no `get_bool_sub` on `Config` yet.
/// 4. Otherwise return `true`.
pub fn should_emit_banner(version: &str, argv: &[String], cfg: Option<&Config>) -> bool {
    if !version.contains("-beta") {
        return false;
    }
    if argv.iter().any(|a| a == ACK_FLAG) {
        return false;
    }
    if let Some(cfg) = cfg {
        if let Some(raw) = cfg.get_string_sub(SECTION, SUBSECTION, KEY) {
            if parse_git_bool(raw) == Some(true) {
                return false;
            }
        }
    }
    true
}

/// Strip every occurrence of `--i-know-this-is-beta` from argv in place.
///
/// We use `retain` rather than `swap_remove` to preserve argument order
/// (positional pathspecs and revs care about order).
pub fn strip_ack_flag(argv: &mut Vec<String>) {
    argv.retain(|a| a != ACK_FLAG);
}

/// Side-effecting wrapper called from `main()`. Strips the ack flag
/// from argv unconditionally (so clap never sees it) and prints the
/// one-line banner to stderr if [`should_emit_banner`] says we should.
///
/// Idempotent: the [`WARNED`] guard ensures the banner is printed at
/// most once per process, even if this function is called multiple
/// times.
pub fn emit_beta_banner_if_unacknowledged(argv: &mut Vec<String>) {
    // Capture whether the flag was present BEFORE stripping it, so the
    // decision function sees the same argv shape the user typed.
    let had_ack_flag = argv.iter().any(|a| a == ACK_FLAG);
    strip_ack_flag(argv);

    // The version comes from Cargo at compile time. We rebuild the
    // decision input here rather than threading `argv` through with the
    // flag still attached, because once the flag is stripped the
    // function would always decide "emit" — so reconstruct the decision
    // with the captured flag bit.
    let version = env!("CARGO_PKG_VERSION");
    if !version.contains("-beta") {
        return;
    }
    if had_ack_flag {
        return;
    }

    // Try to load the layered config. If we're inside a repo,
    // `Repository::discover_from_cwd` finds it; otherwise we fall back
    // to a path that won't exist, so the local-layer read is a no-op
    // and we still get global/XDG/system. Either failure mode (no
    // repo, no config file) means "no acknowledgement found" — fall
    // through to the print step.
    let gitdir = crate::repo::Repository::discover_from_cwd()
        .map(|r| r.gitdir().to_path_buf())
        .unwrap_or_else(|_| std::path::PathBuf::from(".git"));
    let cfg = Config::load_layered(&gitdir).ok();
    let cfg_ref = cfg.as_ref();

    // Use the pure decision function so its rules are the single source
    // of truth. We pass an argv without the ack flag because we've
    // already accounted for it above.
    if !should_emit_banner(version, argv, cfg_ref) {
        return;
    }

    // OnceLock guard: print at most once per process.
    if WARNED.set(()).is_err() {
        return;
    }

    eprintln!("rustygit beta — see BETA.md for known divergences and how to acknowledge.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn cfg_from(text: &str) -> Config {
        Config::parse_str(text).expect("test config parses")
    }

    #[test]
    fn ga_version_never_emits() {
        let cfg = cfg_from("");
        assert!(!should_emit_banner(
            "0.1.0",
            &argv(&["rustygit", "status"]),
            Some(&cfg)
        ));
        assert!(!should_emit_banner(
            "1.0.0",
            &argv(&["rustygit", "status"]),
            Some(&cfg)
        ));
        // No config also fine.
        assert!(!should_emit_banner(
            "0.1.0",
            &argv(&["rustygit", "status"]),
            None
        ));
    }

    #[test]
    fn beta_version_unacknowledged_emits() {
        let cfg = cfg_from("");
        assert!(should_emit_banner(
            "0.1.0-beta",
            &argv(&["rustygit", "status"]),
            Some(&cfg)
        ));
        // No config at all — still emit, the flag isn't set anywhere.
        assert!(should_emit_banner(
            "0.1.0-beta",
            &argv(&["rustygit", "status"]),
            None
        ));
    }

    #[test]
    fn beta_version_with_ack_flag_does_not_emit() {
        let cfg = cfg_from("");
        assert!(!should_emit_banner(
            "0.1.0-beta",
            &argv(&["rustygit", "--i-know-this-is-beta", "status"]),
            Some(&cfg)
        ));
    }

    #[test]
    fn beta_version_with_config_ack_does_not_emit() {
        // The on-disk shape `rustygit config rustygit.beta.acknowledged
        // true` writes: a subsection-style `[rustygit "beta"]` block
        // with `acknowledged = true`.
        let cfg = cfg_from("[rustygit \"beta\"]\n\tacknowledged = true\n");
        assert!(!should_emit_banner(
            "0.1.0-beta",
            &argv(&["rustygit", "status"]),
            Some(&cfg)
        ));
    }

    #[test]
    fn beta_version_with_config_false_still_emits() {
        let cfg = cfg_from("[rustygit \"beta\"]\n\tacknowledged = false\n");
        assert!(should_emit_banner(
            "0.1.0-beta",
            &argv(&["rustygit", "status"]),
            Some(&cfg)
        ));
    }

    #[test]
    fn rc_version_does_not_count_as_beta() {
        let cfg = cfg_from("");
        // GA contract: only the literal substring `-beta` triggers the
        // banner. Release candidates like `0.1.0-rc1` are GA-shape and
        // must not banner.
        assert!(!should_emit_banner(
            "0.1.0-rc1",
            &argv(&["rustygit", "status"]),
            Some(&cfg)
        ));
    }

    #[test]
    fn strip_removes_flag_anywhere_in_argv() {
        let mut a = argv(&["rustygit", "--i-know-this-is-beta", "status"]);
        strip_ack_flag(&mut a);
        assert_eq!(a, vec!["rustygit", "status"]);

        let mut a = argv(&["rustygit", "status", "--i-know-this-is-beta"]);
        strip_ack_flag(&mut a);
        assert_eq!(a, vec!["rustygit", "status"]);

        // Multiple occurrences (duplicate flag) — strip all.
        let mut a = argv(&[
            "rustygit",
            "--i-know-this-is-beta",
            "status",
            "--i-know-this-is-beta",
        ]);
        strip_ack_flag(&mut a);
        assert_eq!(a, vec!["rustygit", "status"]);

        // No flag — argv unchanged.
        let mut a = argv(&["rustygit", "status"]);
        strip_ack_flag(&mut a);
        assert_eq!(a, vec!["rustygit", "status"]);
    }

    #[test]
    fn beta_version_with_ack_uppercase_truthy() {
        // git's bool parser is case-insensitive — verify we honor that.
        let cfg = cfg_from("[rustygit \"beta\"]\n\tacknowledged = YES\n");
        assert!(!should_emit_banner(
            "0.1.0-beta",
            &argv(&["rustygit", "status"]),
            Some(&cfg)
        ));
    }

    #[test]
    fn parse_git_bool_accepts_canonical_spellings() {
        // Spot-check the spellings we care about; full coverage is in the
        // upstream `config::parse_bool` tests.
        assert_eq!(parse_git_bool("true"), Some(true));
        assert_eq!(parse_git_bool("TRUE"), Some(true));
        assert_eq!(parse_git_bool("yes"), Some(true));
        assert_eq!(parse_git_bool("on"), Some(true));
        assert_eq!(parse_git_bool("1"), Some(true));
        assert_eq!(parse_git_bool("false"), Some(false));
        assert_eq!(parse_git_bool("off"), Some(false));
        assert_eq!(parse_git_bool("0"), Some(false));
        assert_eq!(parse_git_bool(""), Some(false));
        assert_eq!(parse_git_bool("nonsense"), None);
    }
}
