//! `[alias]` config expansion — NON_GOALS A1 / Launch-readiness plan A1.
//!
//! ## Why
//!
//! Every user switching from upstream git has a `~/.gitconfig` like:
//!
//! ```text
//! [alias]
//!     st = status
//!     co = checkout
//!     ci = commit
//!     last = log -1 HEAD
//!     unstage = restore --staged
//! ```
//!
//! When they type `rustygit st`, clap-derive sees an unknown subcommand
//! and rejects with "unrecognized subcommand 'st'". Without alias
//! expansion, every switching user hits this on day one. That's the
//! single most common muscle-memory break.
//!
//! ## How
//!
//! `expand(argv, cfg)` runs in `main.rs` AFTER the hardcoded-alias
//! translations and BEFORE `Cli::parse_from`. It:
//!
//! 1. Locates `argv[1]` — the candidate subcommand. (Skips the `argv[0]`
//!    binary name.)
//! 2. If it starts with `-` (a flag, e.g. `--version`) → no expansion.
//! 3. If it matches a known built-in subcommand → no expansion (built-ins
//!    win over aliases, matching git).
//! 4. Otherwise looks up `alias.<name>` in the layered config.
//! 5. If found, splits the alias body via a shell-words parser
//!    (single-quote, double-quote, backslash escape) and SPLICES the
//!    resulting tokens into argv in place of the alias name.
//! 6. Recursively expands the result, capped at 10 hops.
//!
//! ## What we deliberately don't do
//!
//! - **Shell-execute aliases** (`!` prefix). `[alias] sync = !git fetch
//!   && git rebase` would `Command::new("sh").arg("-c").arg(...).status()`
//!   in upstream git. We REFUSE these with a clear error rather than
//!   silently executing arbitrary shell — the security surface is too
//!   broad for the v0.1.0 launch.
//! - **Alias body argument-interpolation** (`$1`, `$@`). Not part of git's
//!   own alias semantics either — git's aliases just prepend; user args
//!   follow. We do the same.

use std::collections::HashSet;

use thiserror::Error;

use crate::config::Config;

/// Maximum number of recursive alias expansions before we give up.
/// Matches upstream git's `max_aliases = 10` to defeat alias loops like
/// `alias.foo = bar` + `alias.bar = foo`.
const MAX_HOPS: usize = 10;

/// All built-in subcommand names. Derived dynamically from the clap
/// `Cli::command()` structure on first call so the list stays in sync
/// with `cli::Command` automatically — no hand-curated array to drift
/// out of date as new subcommands land.
///
/// Built-ins always win over like-named aliases (matches upstream git in
/// `builtin/main.c::handle_builtin`).
fn builtin_subcommands() -> &'static HashSet<String> {
    static BUILTINS: std::sync::OnceLock<HashSet<String>> = std::sync::OnceLock::new();
    BUILTINS.get_or_init(|| {
        use clap::CommandFactory;
        let mut set: HashSet<String> = super::Cli::command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        // The hardcoded aliases applied earlier in `main.rs::main` mean a
        // user typing `init-db` already gets routed to `init` (etc.) before
        // we reach this code path. Include those translated names too so a
        // user-defined `[alias] init-db = something` never accidentally
        // beats our hardcoded translation.
        for synonym in [
            "annotate",
            "init-db",
            "gui",
            "git-gui",
            "svn",
            "git-svn",
            "p4",
            "git-p4",
            "instaweb",
            "git-instaweb",
        ] {
            set.insert(synonym.to_string());
        }
        set
    })
}

#[derive(Debug, Error)]
pub enum AliasError {
    #[error(
        "alias '{name}' starts with '!' (shell execution); not supported in rustygit. \
         Use a shell function or a real subcommand instead."
    )]
    ShellAliasRejected { name: String },
    #[error("alias '{name}' is empty in config")]
    EmptyAlias { name: String },
    #[error(
        "alias expansion loop detected (limit {limit} hops); chain: {chain}. \
         Check your [alias] section for circular references."
    )]
    LoopDetected { limit: usize, chain: String },
    #[error("alias body for '{name}' has unterminated quote at position {pos}")]
    UnterminatedQuote { name: String, pos: usize },
    #[error("alias body for '{name}' ends with bare backslash")]
    BareBackslash { name: String },
}

/// Expand a single layer of alias substitution in-place on `argv`.
///
/// Returns `Ok(true)` if an alias was expanded (`argv` was rewritten),
/// `Ok(false)` if no expansion happened (built-in subcommand, flag, or
/// no matching alias). Returns `Err(...)` for malformed aliases.
///
/// Callers should loop while this returns `Ok(true)`, with their own
/// hop counter for defense-in-depth (this function uses an internal
/// `seen` set; the caller's counter guards against pathological
/// non-cycle chains).
pub fn expand_once(argv: &mut Vec<String>, cfg: &Config) -> Result<bool, AliasError> {
    // argv[0] is the binary name; argv[1] is the candidate subcommand.
    if argv.len() < 2 {
        return Ok(false);
    }
    let candidate = &argv[1];
    if candidate.starts_with('-') {
        return Ok(false); // it's a flag, not a subcommand
    }
    if builtin_subcommands().contains(candidate) {
        return Ok(false); // built-in wins over any like-named alias
    }

    // Look up `[alias] <candidate> = <body>` in the layered config.
    let body = match cfg.get_string("alias", candidate) {
        Some(s) => s.to_string(),
        None => return Ok(false), // no such alias
    };

    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(AliasError::EmptyAlias {
            name: candidate.clone(),
        });
    }
    if let Some(stripped) = trimmed.strip_prefix('!') {
        let _ = stripped; // intentionally unused
        return Err(AliasError::ShellAliasRejected {
            name: candidate.clone(),
        });
    }

    let tokens = split_shell_words(trimmed, candidate)?;
    if tokens.is_empty() {
        return Err(AliasError::EmptyAlias {
            name: candidate.clone(),
        });
    }

    // Splice: remove argv[1], insert the alias-body tokens in its place.
    argv.splice(1..2, tokens);
    Ok(true)
}

/// Expand recursively, with a hop limit. Tracks the chain of alias names
/// we've expanded for a useful loop-detected error message.
pub fn expand(argv: &mut Vec<String>, cfg: &Config) -> Result<(), AliasError> {
    let mut seen: Vec<String> = Vec::new();
    for _ in 0..MAX_HOPS {
        if argv.len() >= 2 {
            seen.push(argv[1].clone());
        }
        if !expand_once(argv, cfg)? {
            return Ok(());
        }
    }
    Err(AliasError::LoopDetected {
        limit: MAX_HOPS,
        chain: seen.join(" → "),
    })
}

/// Split an alias body into argv tokens using shell-style quoting rules.
///
/// Per git's `sq_quote_buf` / `sq_dequote_to_argv_array`:
///   * Whitespace (`' '`, `'\t'`) is a token boundary outside quotes.
///   * `'...'` is literal — no escape interpretation inside; the next
///     `'` ends the run.
///   * `"..."` allows `\\` and `\"` escapes (others are literal).
///   * Outside quotes, `\` escapes the next character.
///
/// `alias_name` is only used for error messages.
fn split_shell_words(body: &str, alias_name: &str) -> Result<Vec<String>, AliasError> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut at_token_start = true; // tracks whether we're between tokens

    let chars: Vec<char> = body.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_single {
            if c == '\'' {
                in_single = false;
                at_token_start = false;
            } else {
                current.push(c);
            }
            i += 1;
            continue;
        }
        if in_double {
            if c == '"' {
                in_double = false;
                at_token_start = false;
            } else if c == '\\' && i + 1 < chars.len() {
                let next = chars[i + 1];
                match next {
                    '\\' | '"' => current.push(next),
                    other => {
                        // Inside double quotes, only `\\` and `\"` are
                        // recognized escapes; preserve the rest verbatim.
                        current.push('\\');
                        current.push(other);
                    }
                }
                i += 2;
                continue;
            } else {
                current.push(c);
            }
            i += 1;
            continue;
        }
        // Outside quotes.
        match c {
            ' ' | '\t' | '\n' => {
                if !at_token_start {
                    tokens.push(std::mem::take(&mut current));
                    at_token_start = true;
                }
            }
            '\'' => {
                in_single = true;
                at_token_start = false;
            }
            '"' => {
                in_double = true;
                at_token_start = false;
            }
            '\\' => {
                if i + 1 >= chars.len() {
                    return Err(AliasError::BareBackslash {
                        name: alias_name.to_string(),
                    });
                }
                current.push(chars[i + 1]);
                at_token_start = false;
                i += 2;
                continue;
            }
            _ => {
                current.push(c);
                at_token_start = false;
            }
        }
        i += 1;
    }

    if in_single || in_double {
        return Err(AliasError::UnterminatedQuote {
            name: alias_name.to_string(),
            pos: i,
        });
    }
    if !at_token_start {
        tokens.push(current);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn cfg_from(text: &str) -> Config {
        Config::parse_str(text).unwrap()
    }

    #[test]
    fn expand_simple_alias() {
        let cfg = cfg_from("[alias]\n\tst = status\n");
        let mut argv = vec!["rustygit".into(), "st".into()];
        assert!(expand_once(&mut argv, &cfg).unwrap());
        assert_eq!(argv, vec!["rustygit", "status"]);
    }

    #[test]
    fn expand_alias_with_args() {
        let cfg = cfg_from("[alias]\n\tlast = log -1 HEAD\n");
        let mut argv = vec!["rustygit".into(), "last".into()];
        expand(&mut argv, &cfg).unwrap();
        assert_eq!(argv, vec!["rustygit", "log", "-1", "HEAD"]);
    }

    #[test]
    fn user_args_appended_after_alias() {
        let cfg = cfg_from("[alias]\n\tlg = log --oneline\n");
        let mut argv = vec!["rustygit".into(), "lg".into(), "--all".into()];
        expand(&mut argv, &cfg).unwrap();
        assert_eq!(argv, vec!["rustygit", "log", "--oneline", "--all"]);
    }

    #[test]
    fn recursive_alias_resolved() {
        let cfg = cfg_from("[alias]\n\tfoo = bar\n\tbar = status\n");
        let mut argv = vec!["rustygit".into(), "foo".into()];
        expand(&mut argv, &cfg).unwrap();
        assert_eq!(argv, vec!["rustygit", "status"]);
    }

    #[test]
    fn builtin_subcommand_not_aliased_even_if_config_present() {
        let cfg = cfg_from("[alias]\n\tstatus = log\n");
        // `status` is a built-in — alias.status MUST be ignored.
        let mut argv = vec!["rustygit".into(), "status".into()];
        assert!(!expand_once(&mut argv, &cfg).unwrap());
        assert_eq!(argv, vec!["rustygit", "status"]);
    }

    #[test]
    fn flag_argv_no_expansion() {
        let cfg = cfg_from("[alias]\n\thelp = log\n");
        let mut argv = vec!["rustygit".into(), "--version".into()];
        assert!(!expand_once(&mut argv, &cfg).unwrap());
    }

    #[test]
    fn no_alias_no_expansion() {
        let cfg = cfg_from("");
        let mut argv = vec!["rustygit".into(), "zzz".into()];
        // No alias.zzz, no built-in zzz — we don't error; we just return
        // false and let clap reject the unknown subcommand downstream.
        assert!(!expand_once(&mut argv, &cfg).unwrap());
    }

    #[test]
    fn shell_alias_rejected() {
        let cfg = cfg_from("[alias]\n\tsync = !git fetch && git rebase\n");
        let mut argv = vec!["rustygit".into(), "sync".into()];
        let err = expand(&mut argv, &cfg).unwrap_err();
        assert!(matches!(err, AliasError::ShellAliasRejected { .. }));
    }

    #[test]
    fn empty_alias_rejected() {
        let cfg = cfg_from("[alias]\n\tbroken = \n");
        let mut argv = vec!["rustygit".into(), "broken".into()];
        let err = expand(&mut argv, &cfg).unwrap_err();
        assert!(matches!(err, AliasError::EmptyAlias { .. }));
    }

    #[test]
    fn loop_detected() {
        let cfg = cfg_from("[alias]\n\ta = b\n\tb = a\n");
        let mut argv = vec!["rustygit".into(), "a".into()];
        let err = expand(&mut argv, &cfg).unwrap_err();
        assert!(matches!(err, AliasError::LoopDetected { .. }));
    }

    // Shell-words parser unit tests.

    #[test]
    fn split_simple_whitespace() {
        let v = split_shell_words("log -1 HEAD", "test").unwrap();
        assert_eq!(v, vec!["log", "-1", "HEAD"]);
    }

    #[test]
    fn split_single_quotes_preserve_spaces() {
        let v = split_shell_words("log --pretty='format:%h %s'", "test").unwrap();
        assert_eq!(v, vec!["log", "--pretty=format:%h %s"]);
    }

    #[test]
    fn split_double_quotes_with_escapes() {
        let v = split_shell_words(r#"log --grep="he said \"hi\"""#, "test").unwrap();
        assert_eq!(v, vec!["log", r#"--grep=he said "hi""#]);
    }

    #[test]
    fn split_backslash_escape_outside_quotes() {
        let v = split_shell_words(r#"log path\ with\ spaces"#, "test").unwrap();
        assert_eq!(v, vec!["log", "path with spaces"]);
    }

    #[test]
    fn split_unterminated_single_quote() {
        let err = split_shell_words("log --grep='oops", "test").unwrap_err();
        assert!(matches!(err, AliasError::UnterminatedQuote { .. }));
    }

    #[test]
    fn split_bare_trailing_backslash() {
        let err = split_shell_words(r#"log \"#, "test").unwrap_err();
        assert!(matches!(err, AliasError::BareBackslash { .. }));
    }
}
