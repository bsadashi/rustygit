//! Minimal git-config parser (ADR A10).
//!
//! Why hand-rolled: git's config file looks like INI but is *not* INI in any
//! formally-compatible sense. It has subsections (`[remote "origin"]`),
//! case-insensitive section/key matching but case-sensitive subsection names,
//! its own escape-sequence rules inside double-quoted values, and a slate of
//! truth values (`true|false|yes|no|on|off|1|0|""`). No off-the-shelf INI
//! crate handles all of this, so we parse what we need and bail clearly on
//! the corners we haven't reached yet (multi-line values via trailing `\`,
//! `[include]` directives).
//!
//! What this implementation supports for M3:
//! - `[section]` and `[section "subsection"]` headers
//! - `key = value` pairs (also bare `key` → empty string)
//! - `key=value` with no whitespace
//! - leading whitespace / tab indentation (ignored)
//! - comments via `;` or `#` to end of line, but only if not inside a quoted
//!   value
//! - quoted values with `\\`, `\"`, `\n`, `\t`, `\b` escapes
//! - case-insensitive lookups for section + key
//! - multi-value keys (last value wins for `get_string` etc.; the order is
//!   preserved internally so `core.repositoryformatversion` written twice
//!   reads back the second)
//!
//! Explicitly NOT supported (errors out):
//! - line continuation with trailing `\`
//! - `[include]` and conditional `[includeIf]`

use std::fs;
use std::path::Path;

use thiserror::Error;

#[derive(Debug, Clone)]
struct Entry {
    section: String,            // lowercased
    subsection: Option<String>, // case-sensitive
    key: String,                // lowercased
    value: String,              // raw, after de-quoting and escape resolution
}

/// Parsed git config. The internal storage is a `Vec<Entry>` rather than a
/// hashmap so we can preserve insertion order (some lookups are
/// last-write-wins; iteration may matter later).
#[derive(Debug, Clone, Default)]
pub struct Config {
    entries: Vec<Entry>,
}

impl Config {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parse a config file given as a string.
    pub fn parse_str(text: &str) -> Result<Self, ConfigError> {
        let mut entries = Vec::new();
        let mut section: Option<String> = None;
        let mut subsection: Option<String> = None;
        // True iff the most recent section header was `[include]` / `[includeIf]`
        // and we're swallowing its body until the next section. Distinct from
        // `section.is_none()` (no header seen yet at all), which still errors
        // on stray `name = ...` lines at the top of the file.
        let mut in_skipped_section = false;

        for (lineno, raw_line) in text.lines().enumerate() {
            let lineno = lineno + 1;

            // Reject line continuation up front.
            if line_ends_with_unquoted_backslash(raw_line) {
                return Err(ConfigError::Unsupported {
                    line: lineno,
                    reason: "multi-line values (trailing backslash) are not supported in M3".into(),
                });
            }

            let line = strip_unquoted_comment(raw_line);
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            if line.starts_with('[') {
                let (sec, sub) = parse_section_header(line, lineno)?;
                if sec.eq_ignore_ascii_case("include") || sec.eq_ignore_ascii_case("includeif") {
                    // Per the launch-readiness plan (A2): silently skip the
                    // include section with a one-time stderr warning per
                    // process. Real `[include]` / `[includeIf]` resolution
                    // is post-launch — failing hard here would break any
                    // user with conditional configs in their `~/.gitconfig`
                    // on first rustygit run.
                    warn_include_unsupported_once();
                    in_skipped_section = true;
                    continue;
                }
                // Real section header — reset the skip flag.
                in_skipped_section = false;
                section = Some(sec.to_lowercase());
                subsection = sub.map(|s| s.to_string());
                continue;
            }

            // Inside an `[include]` / `[includeIf]` body: silently drop
            // every key/value until the next `[section]` header. (Without
            // this guard, the "key outside of any section" malformed
            // error below would fire on the include's `path = ...` line.)
            if in_skipped_section {
                continue;
            }

            // Key = value (or bare `key`).
            let cur_section = section.as_deref().ok_or_else(|| ConfigError::Malformed {
                line: lineno,
                reason: "key outside of any section".into(),
            })?;

            let (k, v) = parse_kv(line, lineno)?;
            validate_key(&k, lineno)?;
            entries.push(Entry {
                section: cur_section.to_string(),
                subsection: subsection.clone(),
                key: k.to_lowercase(),
                value: v,
            });
        }

        Ok(Self { entries })
    }

    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let bytes = fs::read(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let text = std::str::from_utf8(&bytes).map_err(|_| ConfigError::Encoding {
            path: path.to_path_buf(),
        })?;
        Self::parse_str(text)
    }

    /// Read the merged config for `gitdir`: system + XDG + global +
    /// local + `-c` overrides, in that precedence order (later layers
    /// win). Equivalent to upstream git's full early-config sequence;
    /// see [`Config::load_layered`] for the layer-by-layer details.
    ///
    /// Backward-compatible name retained because every callsite in
    /// rustygit already invokes `Config::from_repo_dir`.
    pub fn from_repo_dir(gitdir: &Path) -> Result<Self, ConfigError> {
        Self::load_layered(gitdir)
    }

    /// Read ONLY the local `<gitdir>/config`, no global/XDG/system
    /// layers. Used by code that intentionally wants to read or rewrite
    /// the repo-local file in isolation — e.g. `rustygit config --local`
    /// or anything that mutates the on-disk file via
    /// [`crate::lockfile::Lockfile`]. Returns empty on missing.
    pub fn from_local_only(gitdir: &Path) -> Result<Self, ConfigError> {
        let path = gitdir.join("config");
        let mut cfg = match fs::read(&path) {
            Ok(bytes) => {
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| ConfigError::Encoding { path: path.clone() })?;
                Self::parse_str(text)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::empty(),
            Err(source) => {
                return Err(ConfigError::Io {
                    path: path.clone(),
                    source,
                });
            }
        };
        cfg.apply_cli_overrides();
        Ok(cfg)
    }

    /// Append `[section.subsection?]name=value` overrides as collected by the
    /// top-level `-c key=value` flag (stored in [`set_cli_overrides`]).
    /// Each `key` is split as `<section>.<name>` (no subsection) or
    /// `<section>.<subsection>.<name>`.
    ///
    /// Keys are validated against the same `[a-zA-Z][a-zA-Z0-9-]*` rule the
    /// file parser enforces, so a malformed override (e.g. `user.=x`,
    /// `1bad.name=x`, `user.name with space=x`) is rejected with a stderr
    /// warning instead of silently sitting unmatched in the config layer.
    /// Subsection content is left verbatim (case-sensitive, may contain
    /// dots).
    pub fn apply_cli_overrides(&mut self) {
        for (key, value) in cli_overrides() {
            let parts = match split_dotted_key(&key) {
                Some(p) => p,
                None => {
                    eprintln!("rustygit: -c: invalid key '{key}' (expected section.name)");
                    continue;
                }
            };
            let (section, subsection, name) = parts;
            if !is_valid_simple_name(section) {
                eprintln!("rustygit: -c: invalid section name '{section}' in '{key}'");
                continue;
            }
            if !is_valid_simple_name(name) {
                eprintln!("rustygit: -c: invalid variable name '{name}' in '{key}'");
                continue;
            }
            self.entries.push(Entry {
                section: section.to_lowercase(),
                subsection: subsection.map(String::from),
                key: name.to_lowercase(),
                value,
            });
        }
    }
}

/// Section/key validity per git: must start with `[a-zA-Z]` and otherwise
/// contain only `[a-zA-Z0-9-]`. Empty input rejects.
fn is_valid_simple_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        None => return false,
        Some(c) if !c.is_ascii_alphabetic() => return false,
        Some(_) => {}
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Split a `-c` key into `(section, subsection, name)`. The standard form is:
///
/// * `section.name` — no subsection
/// * `section.subsection.name` — subsection is the middle dot-separated chunk
/// * `section."sub.with.dots".name` — quoted subsection (rare; we don't support)
///
/// Returns `None` if the key has fewer than 2 segments.
fn split_dotted_key(key: &str) -> Option<(&str, Option<&str>, &str)> {
    let first = key.find('.')?;
    let last = key.rfind('.')?;
    if first == last {
        // Two segments: section.name
        Some((&key[..first], None, &key[first + 1..]))
    } else {
        // Three or more — section is everything before first dot, name is
        // everything after last dot, subsection is the middle (verbatim).
        let section = &key[..first];
        let name = &key[last + 1..];
        let subsection = &key[first + 1..last];
        Some((section, Some(subsection), name))
    }
}

/// Process-wide collection of `-c key=value` overrides. Set once from the CLI
/// (in [`crate::cli::dispatch`]) and read by every `Config::from_repo_dir`.
/// Empty by default, so library callers without a CLI in front of them are
/// unaffected.
static CLI_OVERRIDES: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();

/// Install the CLI's `-c` overrides. Idempotent in practice — `dispatch` is
/// the only caller and runs at most once per process. If called twice, the
/// first call wins (matches `OnceLock` semantics) and the second is silently
/// ignored.
pub fn set_cli_overrides(overrides: Vec<(String, String)>) {
    let _ = CLI_OVERRIDES.set(overrides);
}

fn cli_overrides() -> Vec<(String, String)> {
    CLI_OVERRIDES.get().cloned().unwrap_or_default()
}

/// Print a stderr warning the FIRST time we encounter an unsupported
/// `[include]` / `[includeIf]` section, then stay silent for the rest of
/// the process. Spamming "include not supported" on every config-read
/// would drown the user's actual output.
fn warn_include_unsupported_once() {
    static WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let _ = WARNED.get_or_init(|| {
        eprintln!(
            "rustygit: warning: [include] / [includeIf] directives are not yet \
             honored; ignoring (see NON_GOALS.md)"
        );
    });
}

// ---------------------------------------------------------------------------
// Layered config discovery — system + global + XDG + local + CLI -c overrides
//
// Per `git-config(1)` precedence, lowest to highest:
//   1. system   — `/etc/gitconfig` (override via $GIT_CONFIG_SYSTEM;
//                 suppressed entirely by $GIT_CONFIG_NOSYSTEM=1)
//   2. xdg      — `$XDG_CONFIG_HOME/git/config`, else `$HOME/.config/git/config`
//   3. global   — `$HOME/.gitconfig` (override via $GIT_CONFIG_GLOBAL)
//   4. local    — `<gitdir>/config`
//   5. cli      — `-c key=value` (applied last in `apply_cli_overrides`)
//
// Our lookup function walks `entries` in REVERSE for last-write-wins
// semantics, so the layered loader just appends each layer in the order
// above. Missing files are OK; parse errors in NON-LOCAL layers print a
// one-time stderr warning and continue.
//
// To keep tests hermetic (HOME=/nonexistent or unset), every layer is a
// no-op when its discovered path doesn't exist. Tests that DO care about
// layered behavior set the env vars explicitly.
// ---------------------------------------------------------------------------

impl Config {
    /// Read and merge every config layer that applies to `gitdir`, ending with
    /// the `-c` CLI overrides. The returned `Config` is what porcelain should
    /// use. Equivalent to upstream git's `read_early_config()` for the v0.1.0
    /// non-include subset.
    ///
    /// Errors only on a local-config parse failure; non-local layers swallow
    /// errors with a one-time stderr warning (matches git's behavior — a bad
    /// `~/.gitconfig` shouldn't prevent operations in a fresh repo).
    pub fn load_layered(gitdir: &Path) -> Result<Self, ConfigError> {
        let mut cfg = Self::empty();

        // 1. System config (lowest precedence).
        if !env_truthy("GIT_CONFIG_NOSYSTEM") {
            let path = system_config_path();
            cfg.append_layer(&path, "system");
        }

        // 2. XDG config.
        if let Some(path) = xdg_config_path() {
            cfg.append_layer(&path, "xdg");
        }

        // 3. Global config (`~/.gitconfig` or `$GIT_CONFIG_GLOBAL`).
        if let Some(path) = global_config_path() {
            cfg.append_layer(&path, "global");
        }

        // 4. Local config — this one IS allowed to fail (a corrupt local
        // config should not be silently ignored).
        let local = gitdir.join("config");
        match fs::read(&local) {
            Ok(bytes) => {
                let text = std::str::from_utf8(&bytes).map_err(|_| ConfigError::Encoding {
                    path: local.clone(),
                })?;
                let parsed = Self::parse_str(text)?;
                cfg.entries.extend(parsed.entries);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(ConfigError::Io {
                    path: local.clone(),
                    source,
                });
            }
        }

        // 5. CLI -c overrides (highest precedence).
        cfg.apply_cli_overrides();

        Ok(cfg)
    }

    /// Try to load `path` and append its entries to `self`. Errors in
    /// non-local layers are demoted to a one-time stderr warning so a
    /// busted `~/.gitconfig` doesn't break `rustygit status` in a fresh
    /// repo.
    fn append_layer(&mut self, path: &Path, layer_name: &str) {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(_) => return, // missing → no-op
        };
        let text = match std::str::from_utf8(&bytes) {
            Ok(t) => t,
            Err(_) => {
                warn_non_utf8_layer_once(path, layer_name);
                return;
            }
        };
        match Self::parse_str(text) {
            Ok(parsed) => self.entries.extend(parsed.entries),
            Err(e) => warn_layer_parse_error_once(path, layer_name, e),
        }
    }
}

/// True iff `name` is set in env AND its value is one of git's truthy
/// spellings (1, true, yes, on; case-insensitive). Used for
/// `GIT_CONFIG_NOSYSTEM`.
fn env_truthy(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => parse_bool(&v).unwrap_or(false),
        Err(_) => false,
    }
}

/// Resolve the system config path. Precedence: `$GIT_CONFIG_SYSTEM` →
/// `/etc/gitconfig`. Doesn't check existence here — `append_layer` will
/// no-op on missing.
fn system_config_path() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("GIT_CONFIG_SYSTEM") {
        return std::path::PathBuf::from(p);
    }
    std::path::PathBuf::from("/etc/gitconfig")
}

/// Resolve the XDG config path: `$XDG_CONFIG_HOME/git/config`, falling
/// back to `$HOME/.config/git/config`. Returns `None` if neither env
/// var is set (so we never read from a guessed path).
fn xdg_config_path() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("XDG_CONFIG_HOME") {
        let mut path = std::path::PathBuf::from(p);
        path.push("git");
        path.push("config");
        return Some(path);
    }
    let home = std::env::var_os("HOME")?;
    let mut path = std::path::PathBuf::from(home);
    path.push(".config");
    path.push("git");
    path.push("config");
    Some(path)
}

/// Resolve the global config path: `$GIT_CONFIG_GLOBAL` → `$HOME/.gitconfig`.
/// `None` if neither resolvable.
fn global_config_path() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("GIT_CONFIG_GLOBAL") {
        return Some(std::path::PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")?;
    let mut path = std::path::PathBuf::from(home);
    path.push(".gitconfig");
    Some(path)
}

fn warn_non_utf8_layer_once(path: &Path, layer_name: &str) {
    static WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let _ = WARNED.get_or_init(|| {
        eprintln!(
            "rustygit: warning: {layer_name} config at {} is not valid UTF-8; ignoring",
            path.display()
        );
    });
}

fn warn_layer_parse_error_once(path: &Path, layer_name: &str, err: ConfigError) {
    static WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let _ = WARNED.get_or_init(|| {
        eprintln!(
            "rustygit: warning: {layer_name} config at {} failed to parse: {err}; ignoring",
            path.display()
        );
    });
}

impl Config {
    /// Last-value-wins lookup for `[section] name = value` (no subsection).
    pub fn get_string(&self, section: &str, name: &str) -> Option<&str> {
        self.lookup(section, None, name).map(|e| e.value.as_str())
    }

    /// Last-value-wins lookup for `[section "subsection"] name = value`.
    pub fn get_string_sub(&self, section: &str, subsection: &str, name: &str) -> Option<&str> {
        self.lookup(section, Some(subsection), name)
            .map(|e| e.value.as_str())
    }

    /// Parse the value as a git boolean. Returns `None` if absent. Returns
    /// `Some(true/false)` for known truthy/falsy spellings; for anything
    /// unrecognized we return `None` to keep the API total — callers that
    /// care can fall back to `get_string`.
    pub fn get_bool(&self, section: &str, name: &str) -> Option<bool> {
        let s = self.get_string(section, name)?;
        parse_bool(s)
    }

    /// Parse the value as i64. Returns `None` if absent or unparseable.
    pub fn get_int(&self, section: &str, name: &str) -> Option<i64> {
        let s = self.get_string(section, name)?;
        s.trim().parse().ok()
    }

    fn lookup(&self, section: &str, subsection: Option<&str>, name: &str) -> Option<&Entry> {
        let section_lc = section.to_ascii_lowercase();
        let name_lc = name.to_ascii_lowercase();
        // Walk in reverse for last-write-wins semantics.
        self.entries.iter().rev().find(|e| {
            e.section == section_lc
                && e.key == name_lc
                && match (subsection, &e.subsection) {
                    (None, None) => true,
                    (Some(want), Some(got)) => want == got, // case-sensitive
                    _ => false,
                }
        })
    }

    /// Walk every entry under `[section "subsection"]` headers and return
    /// the `(subsection, key, value)` triples in file/insertion order.
    /// Entries without a subsection (i.e. plain `[section]`) are skipped.
    ///
    /// Used by URL rewrites to enumerate every `[url "base"]` block — the
    /// caller needs the subsection name (the "base" URL) AND the per-entry
    /// key (`insteadOf` vs `pushInsteadOf`), neither of which the
    /// `get_string_sub` shape exposes when you don't yet know which
    /// subsection names exist.
    ///
    /// `section_name` is matched case-insensitively. The returned `key`
    /// names are already lower-cased (since the parser lower-cases them).
    /// Subsection names and values are returned verbatim.
    pub fn subsections_of(&self, section_name: &str) -> Vec<(&str, &str, &str)> {
        let want = section_name.to_ascii_lowercase();
        self.entries
            .iter()
            .filter_map(|e| {
                if e.section != want {
                    return None;
                }
                let sub = e.subsection.as_deref()?;
                Some((sub, e.key.as_str(), e.value.as_str()))
            })
            .collect()
    }

    /// Enumerate every `(section, subsection, key)` triple present in this
    /// `Config`. Order matches on-disk parse order plus `apply_cli_overrides`
    /// appends at the tail. Duplicate keys appear once per occurrence.
    ///
    /// Used by `doctor --import-config` to walk the user's full layered
    /// config and report which keys are honored. Values aren't returned —
    /// callers care about "is this key recognized?" not the value. For
    /// values, use [`get_string`] / [`get_string_sub`] / [`get_bool`].
    pub fn all_entries(&self) -> Vec<(String, Option<String>, String)> {
        self.entries
            .iter()
            .map(|e| (e.section.clone(), e.subsection.clone(), e.key.clone()))
            .collect()
    }
}

/// Parse `[section]` or `[section "subsection"]`. The opening `[` and
/// closing `]` must be present. Whitespace inside the brackets is allowed
/// only between `section` and the optional quoted subsection.
fn parse_section_header(line: &str, lineno: usize) -> Result<(&str, Option<&str>), ConfigError> {
    let inside = line
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| ConfigError::Malformed {
            line: lineno,
            reason: format!("malformed section header: {line}"),
        })?;

    if let Some(quote_start) = inside.find('"') {
        let section = inside[..quote_start].trim();
        if section.is_empty() {
            return Err(ConfigError::Malformed {
                line: lineno,
                reason: "empty section name".into(),
            });
        }
        let after = &inside[quote_start + 1..];
        let quote_end = after.rfind('"').ok_or_else(|| ConfigError::Malformed {
            line: lineno,
            reason: "unterminated subsection".into(),
        })?;
        let sub = &after[..quote_end];
        // Trailing junk after the closing quote? Allow only whitespace.
        if !after[quote_end + 1..].trim().is_empty() {
            return Err(ConfigError::Malformed {
                line: lineno,
                reason: "trailing characters after subsection".into(),
            });
        }
        Ok((section, Some(sub)))
    } else {
        let section = inside.trim();
        if section.is_empty() {
            return Err(ConfigError::Malformed {
                line: lineno,
                reason: "empty section name".into(),
            });
        }
        Ok((section, None))
    }
}

fn parse_kv(line: &str, lineno: usize) -> Result<(String, String), ConfigError> {
    let (k, raw_v) = match line.find('=') {
        Some(eq) => (line[..eq].trim().to_string(), line[eq + 1..].to_string()),
        None => (line.to_string(), String::new()),
    };

    let value = parse_value(&raw_v, lineno)?;
    Ok((k, value))
}

/// Resolve quoting and escapes inside a config value.
///
/// Per `git-config(1)`:
/// - leading/trailing whitespace outside quotes is stripped
/// - `"` opens a quoted run; characters between the quotes are literal except
///   for `\\`, `\"`, `\n`, `\t`, `\b`
/// - `;` and `#` outside of quotes start a comment (already stripped)
fn parse_value(raw: &str, lineno: usize) -> Result<String, ConfigError> {
    let mut out = String::new();
    let mut in_quote = false;
    let mut iter = raw.chars().peekable();

    // Skip leading whitespace outside quotes.
    while let Some(&c) = iter.peek() {
        if c == ' ' || c == '\t' {
            iter.next();
        } else {
            break;
        }
    }

    while let Some(c) = iter.next() {
        match c {
            '\\' => {
                let next = iter.next().ok_or_else(|| ConfigError::Malformed {
                    line: lineno,
                    reason: "value ends with bare backslash".into(),
                })?;
                match next {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'b' => out.push('\u{08}'),
                    other => {
                        return Err(ConfigError::Malformed {
                            line: lineno,
                            reason: format!("unknown escape '\\{other}' in value"),
                        });
                    }
                }
            }
            '"' => {
                in_quote = !in_quote;
            }
            _ => out.push(c),
        }
    }
    if in_quote {
        return Err(ConfigError::Malformed {
            line: lineno,
            reason: "unterminated quoted value".into(),
        });
    }

    // Strip trailing whitespace that came from outside any quoted span.
    // We append to `out` only what's between the quotes or what's literal,
    // so the only trailing whitespace we'd have is from text after the last
    // closing quote (or a fully-unquoted value). Trim it.
    Ok(out.trim_end_matches([' ', '\t']).to_string())
}

/// Strip the first `;` or `#` outside of a quoted span, returning the prefix.
/// Inside a quoted span, `\"` and `\\` are escapes that hide the `"` from
/// closing the span and a `\` from being a literal terminator. Outside a
/// quoted span, `\` is just a literal byte (no escape).
fn strip_unquoted_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_quote = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_quote && b == b'\\' && i + 1 < bytes.len() {
            // Skip the escape and the next byte (so a \" doesn't close the quote).
            i += 2;
            continue;
        }
        if b == b'"' {
            in_quote = !in_quote;
            i += 1;
            continue;
        }
        if !in_quote && (b == b'#' || b == b';') {
            return &line[..i];
        }
        i += 1;
    }
    line
}

/// True if the line ends with a `\` that is not inside a quoted region and
/// not itself escaped. We use this only as a "bail out" detector — full
/// multi-line value support is deferred.
fn line_ends_with_unquoted_backslash(line: &str) -> bool {
    let trimmed = line.trim_end_matches([' ', '\t']);
    if !trimmed.ends_with('\\') {
        return false;
    }
    // Count trailing backslashes — if odd, the last one starts a continuation.
    let count = trimmed.bytes().rev().take_while(|&b| b == b'\\').count();
    count % 2 == 1
}

/// Keys are `[a-zA-Z][a-zA-Z0-9-]*`. Reject otherwise — it almost certainly
/// indicates a malformed file rather than something we should silently
/// accept.
fn validate_key(k: &str, lineno: usize) -> Result<(), ConfigError> {
    let mut chars = k.chars();
    let first = chars.next().ok_or_else(|| ConfigError::Malformed {
        line: lineno,
        reason: "empty key".into(),
    })?;
    if !first.is_ascii_alphabetic() {
        return Err(ConfigError::Malformed {
            line: lineno,
            reason: format!("invalid key '{k}'"),
        });
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '-') {
            return Err(ConfigError::Malformed {
                line: lineno,
                reason: format!("invalid key '{k}'"),
            });
        }
    }
    Ok(())
}

/// git's truth values, all case-insensitive: true|on|yes|1, false|off|no|0|"".
pub(crate) fn parse_bool(s: &str) -> Option<bool> {
    let t = s.trim();
    match t.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" | "" => Some(false),
        _ => None,
    }
}

/// `core.autocrlf` — the minimal subset rustygit honors.
///
/// See `git-config(1)`. `True` means CRLF→LF on `add`, LF→CRLF on checkout.
/// `Input` means CRLF→LF on `add`, no conversion on checkout (Unix default
/// for repos that ingest from CRLF systems). `False` means no conversion in
/// either direction.
///
/// rustygit's autocrlf is config-driven only — the `.gitattributes`-based
/// `text` / `text=auto` driver is NOT honored yet. This matches the
/// "best-effort Windows" posture documented in NON_GOALS A10.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoCrlf {
    /// CRLF→LF on add. LF→CRLF on checkout.
    True,
    /// CRLF→LF on add. No conversion on checkout.
    Input,
    /// No conversion in either direction.
    False,
}

impl AutoCrlf {
    /// Parse the config value. Accepts `true`/`false` (any boolean spelling)
    /// and the literal `input` (case-insensitive). Returns `None` for
    /// unrecognized values so the caller can fall back to the default.
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.eq_ignore_ascii_case("input") {
            return Some(AutoCrlf::Input);
        }
        match parse_bool(trimmed) {
            Some(true) => Some(AutoCrlf::True),
            Some(false) => Some(AutoCrlf::False),
            None => None,
        }
    }

    /// True when this mode normalizes CRLF→LF before hashing on `add`.
    pub fn normalizes_on_add(self) -> bool {
        matches!(self, AutoCrlf::True | AutoCrlf::Input)
    }

    /// True when this mode converts LF→CRLF when writing to the workdir.
    pub fn converts_on_checkout(self) -> bool {
        matches!(self, AutoCrlf::True)
    }
}

/// Upstream git's text-blob heuristic: "no NUL byte in the first 8000 bytes."
/// Used to gate `core.autocrlf` line-ending conversion. Empty buffers count
/// as text (matches `git`'s behavior — converting an empty file is still a
/// no-op).
pub fn is_text_blob(bytes: &[u8]) -> bool {
    let head_len = bytes.len().min(8000);
    !bytes[..head_len].contains(&0u8)
}

/// CRLF → LF in-place over a byte buffer. Cheap allocation-free pass when
/// no CRLF is present (returns the input unchanged).
pub fn normalize_crlf_to_lf(bytes: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    if !bytes.windows(2).any(|w| w == b"\r\n") {
        return std::borrow::Cow::Borrowed(bytes);
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'\r' && bytes[i + 1] == b'\n' {
            out.push(b'\n');
            i += 2;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    std::borrow::Cow::Owned(out)
}

/// LF → CRLF over a byte buffer, leaving existing CRLFs untouched. Used by
/// checkout when `core.autocrlf = true`.
pub fn convert_lf_to_crlf(bytes: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    // Fast path: no bare LF.
    if !bytes.contains(&b'\n') {
        return std::borrow::Cow::Borrowed(bytes);
    }
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 16);
    let mut prev = 0u8;
    for &b in bytes {
        if b == b'\n' && prev != b'\r' {
            out.push(b'\r');
        }
        out.push(b);
        prev = b;
    }
    std::borrow::Cow::Owned(out)
}

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("config malformed at line {line}: {reason}")]
    Malformed { line: usize, reason: String },
    #[error("config feature not yet supported at line {line}: {reason}")]
    Unsupported { line: usize, reason: String },
    #[error("config file is not valid UTF-8: {}", path.display())]
    Encoding { path: std::path::PathBuf },
    #[error("io error on {}: {source}", path.display())]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reconstruct the literal config that `git init` (and our `init.rs`)
    /// produces and verify we can read every key the rest of M3 cares about.
    #[test]
    fn parse_git_init_default_config() {
        let text = "\
[core]
\trepositoryformatversion = 0
\tfilemode = true
\tbare = false
\tlogallrefupdates = true
\tignorecase = true
\tprecomposeunicode = true
";
        let cfg = Config::parse_str(text).unwrap();
        assert_eq!(cfg.get_int("core", "repositoryformatversion"), Some(0));
        assert_eq!(cfg.get_bool("core", "filemode"), Some(true));
        assert_eq!(cfg.get_bool("core", "bare"), Some(false));
        assert_eq!(cfg.get_bool("core", "logallrefupdates"), Some(true));
        assert_eq!(cfg.get_bool("core", "ignorecase"), Some(true));
    }

    #[test]
    fn parse_sha256_init_config() {
        let text = "\
[core]
\trepositoryformatversion = 1
\tfilemode = true
[extensions]
\tobjectformat = sha256
";
        let cfg = Config::parse_str(text).unwrap();
        assert_eq!(cfg.get_int("core", "repositoryformatversion"), Some(1));
        assert_eq!(cfg.get_string("extensions", "objectformat"), Some("sha256"));
    }

    #[test]
    fn user_section_lookups() {
        let text = "\
[user]
\tname = Test Person
\temail = t@example.com
";
        let cfg = Config::parse_str(text).unwrap();
        assert_eq!(cfg.get_string("user", "name"), Some("Test Person"));
        assert_eq!(cfg.get_string("user", "email"), Some("t@example.com"));
    }

    #[test]
    fn case_insensitive_section_and_key() {
        let text = "\
[Core]
\tRepositoryFormatVersion = 0
";
        let cfg = Config::parse_str(text).unwrap();
        assert_eq!(cfg.get_int("core", "repositoryformatversion"), Some(0));
        assert_eq!(cfg.get_int("CORE", "RepositoryFormatVersion"), Some(0));
    }

    #[test]
    fn case_sensitive_subsection() {
        let text = r#"
[remote "origin"]
	url = git@github.com:foo/bar
"#;
        let cfg = Config::parse_str(text).unwrap();
        assert_eq!(
            cfg.get_string_sub("remote", "origin", "url"),
            Some("git@github.com:foo/bar")
        );
        // Wrong case for the subsection: should NOT match.
        assert_eq!(cfg.get_string_sub("remote", "Origin", "url"), None);
    }

    #[test]
    fn comments_and_blank_lines() {
        let text = "\
# top comment
[core]
; another comment
\tbare = false  # trailing comment
\tfilemode = true ; trailing semi comment
";
        let cfg = Config::parse_str(text).unwrap();
        assert_eq!(cfg.get_bool("core", "bare"), Some(false));
        assert_eq!(cfg.get_bool("core", "filemode"), Some(true));
    }

    #[test]
    fn quoted_value_preserves_spaces_and_escapes() {
        let text = "\
[user]
\tname = \"Some One\"
\temail = \"q\\\"uote@x.y\"
";
        let cfg = Config::parse_str(text).unwrap();
        assert_eq!(cfg.get_string("user", "name"), Some("Some One"));
        assert_eq!(cfg.get_string("user", "email"), Some("q\"uote@x.y"));
    }

    #[test]
    fn last_value_wins() {
        let text = "\
[core]
\tfilemode = false
[core]
\tfilemode = true
";
        let cfg = Config::parse_str(text).unwrap();
        assert_eq!(cfg.get_bool("core", "filemode"), Some(true));
    }

    #[test]
    fn bool_truth_values() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("True"), Some(true));
        assert_eq!(parse_bool("yes"), Some(true));
        assert_eq!(parse_bool("on"), Some(true));
        assert_eq!(parse_bool("1"), Some(true));
        assert_eq!(parse_bool("false"), Some(false));
        assert_eq!(parse_bool("OFF"), Some(false));
        assert_eq!(parse_bool("no"), Some(false));
        assert_eq!(parse_bool("0"), Some(false));
        assert_eq!(parse_bool(""), Some(false));
        assert_eq!(parse_bool("maybe"), None);
    }

    #[test]
    fn init_default_branch_and_extensions() {
        let text = "\
[init]
\tdefaultBranch = main
[extensions]
\tobjectFormat = sha256
";
        let cfg = Config::parse_str(text).unwrap();
        assert_eq!(cfg.get_string("init", "defaultbranch"), Some("main"));
        assert_eq!(cfg.get_string("extensions", "objectformat"), Some("sha256"));
    }

    #[test]
    fn rejects_line_continuation() {
        let text = "[user]\n\tname = first \\\n\tpart\n";
        let err = Config::parse_str(text).unwrap_err();
        assert!(matches!(err, ConfigError::Unsupported { .. }));
    }

    #[test]
    fn silently_skips_include_directives() {
        // Per the launch-readiness plan (A2): a `[include]` / `[includeIf]`
        // section is no longer a hard error — it's silently dropped with a
        // one-time stderr warning per process. This lets `~/.gitconfig`
        // files that contain `[includeIf]` (very common) load cleanly
        // instead of breaking every command on first run.
        let text = "[include]\n\tpath = ~/.gitconfig.local\n[user]\n\tname = X\n";
        let cfg = Config::parse_str(text).expect("include should be skipped, not error");
        // The keys INSIDE [include] should not be present.
        assert!(cfg.get_string("include", "path").is_none());
        // Subsequent real sections should still load.
        assert_eq!(cfg.get_string("user", "name"), Some("X"));
    }

    #[test]
    fn silently_skips_includeif_directives() {
        let text = "[includeIf \"gitdir:~/work/\"]\n\tpath = work.config\n\
                    [user]\n\temail = u@e\n";
        let cfg = Config::parse_str(text).expect("includeIf should be skipped");
        assert!(cfg.get_string("includeif", "path").is_none());
        assert_eq!(cfg.get_string("user", "email"), Some("u@e"));
    }

    #[test]
    fn rejects_key_outside_section() {
        let text = "name = foo\n";
        let err = Config::parse_str(text).unwrap_err();
        assert!(matches!(err, ConfigError::Malformed { .. }));
    }

    #[test]
    fn from_repo_dir_returns_empty_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::from_repo_dir(dir.path()).unwrap();
        assert!(cfg.get_string("core", "filemode").is_none());
    }

    #[test]
    fn subsections_of_enumerates_url_blocks() {
        // The shape that NON_GOALS A3 cares about: multiple `[url "X"]`
        // blocks with `insteadOf` / `pushInsteadOf` entries each. The
        // helper has to yield every (subsection, key, value) triple in
        // insertion order, skip the bare `[user]` section, and ignore
        // case on the section name.
        let text = r#"
[user]
	name = test
[URL "https://github.com/"]
	insteadOf = git@github.com:
[url "ssh://git@gitlab.com/"]
	pushInsteadOf = https://gitlab.com/
	insteadOf = git@gitlab.com:
"#;
        let cfg = Config::parse_str(text).unwrap();
        let mut triples = cfg.subsections_of("url");
        // Order is insertion order — the parser walks the file top-to-bottom.
        assert_eq!(triples.len(), 3);
        // First: https://github.com/ block.
        assert_eq!(triples[0].0, "https://github.com/");
        assert_eq!(triples[0].1, "insteadof"); // key is lowercased by parser
        assert_eq!(triples[0].2, "git@github.com:");
        // Second + third: ssh:// gitlab block, both entries.
        assert_eq!(triples[1].0, "ssh://git@gitlab.com/");
        assert_eq!(triples[1].1, "pushinsteadof");
        assert_eq!(triples[1].2, "https://gitlab.com/");
        assert_eq!(triples[2].0, "ssh://git@gitlab.com/");
        assert_eq!(triples[2].1, "insteadof");
        assert_eq!(triples[2].2, "git@gitlab.com:");

        // Case-insensitive section match.
        assert_eq!(cfg.subsections_of("URL").len(), 3);
        assert_eq!(cfg.subsections_of("Url").len(), 3);

        // A section that exists only without a subsection returns empty —
        // `[user] name = ...` has no subsection.
        triples = cfg.subsections_of("user");
        assert!(triples.is_empty());

        // A nonexistent section returns empty.
        assert!(cfg.subsections_of("missing").is_empty());
    }

    #[test]
    fn empty_value_keys_parse_as_empty_string() {
        let text = "\
[core]
\tlogallrefupdates
";
        let cfg = Config::parse_str(text).unwrap();
        assert_eq!(cfg.get_string("core", "logallrefupdates"), Some(""));
        assert_eq!(cfg.get_bool("core", "logallrefupdates"), Some(false));
    }

    #[test]
    fn autocrlf_parses_known_spellings() {
        assert_eq!(AutoCrlf::parse("true"), Some(AutoCrlf::True));
        assert_eq!(AutoCrlf::parse("True"), Some(AutoCrlf::True));
        assert_eq!(AutoCrlf::parse("1"), Some(AutoCrlf::True));
        assert_eq!(AutoCrlf::parse("input"), Some(AutoCrlf::Input));
        assert_eq!(AutoCrlf::parse("INPUT"), Some(AutoCrlf::Input));
        assert_eq!(AutoCrlf::parse("false"), Some(AutoCrlf::False));
        assert_eq!(AutoCrlf::parse("0"), Some(AutoCrlf::False));
        assert_eq!(AutoCrlf::parse("garbage"), None);
    }

    #[test]
    fn autocrlf_modes_describe_their_behavior() {
        assert!(AutoCrlf::True.normalizes_on_add());
        assert!(AutoCrlf::Input.normalizes_on_add());
        assert!(!AutoCrlf::False.normalizes_on_add());

        assert!(AutoCrlf::True.converts_on_checkout());
        assert!(!AutoCrlf::Input.converts_on_checkout());
        assert!(!AutoCrlf::False.converts_on_checkout());
    }

    #[test]
    fn is_text_blob_pure_text() {
        assert!(is_text_blob(b"hello world\n"));
    }

    #[test]
    fn is_text_blob_with_nul_in_middle_is_binary() {
        // NUL well inside the 8000-byte window — binary.
        let mut buf = vec![b'a'; 100];
        buf.push(0);
        buf.extend_from_slice(&[b'b'; 100]);
        assert!(!is_text_blob(&buf));
    }

    #[test]
    fn is_text_blob_with_nul_first_byte_is_binary() {
        assert!(!is_text_blob(b"\x00rest"));
    }

    #[test]
    fn is_text_blob_empty_is_text() {
        assert!(is_text_blob(b""));
    }

    #[test]
    fn is_text_blob_nul_past_8000_bytes_is_text() {
        // NUL just past the 8000-byte heuristic window — should be "text".
        let mut buf = vec![b'a'; 8001];
        buf[8000] = 0;
        assert!(is_text_blob(&buf));
    }

    #[test]
    fn crlf_to_lf_handles_mixed_and_idempotent() {
        assert_eq!(&*normalize_crlf_to_lf(b"a\r\nb\r\nc"), b"a\nb\nc");
        // Already LF — borrowed (no allocation).
        let lf = b"a\nb\n";
        let cow = normalize_crlf_to_lf(lf);
        assert!(matches!(cow, std::borrow::Cow::Borrowed(_)));
        assert_eq!(&*cow, lf);
        // Lone CR not followed by LF is preserved.
        assert_eq!(&*normalize_crlf_to_lf(b"a\rb"), b"a\rb");
    }

    #[test]
    fn lf_to_crlf_round_trip() {
        assert_eq!(&*convert_lf_to_crlf(b"a\nb\nc"), b"a\r\nb\r\nc");
        // Already CRLF: stays CRLF, no double-conversion.
        assert_eq!(&*convert_lf_to_crlf(b"a\r\nb\r\nc"), b"a\r\nb\r\nc");
        // No LF: borrowed (no allocation).
        let plain = b"abc";
        let cow = convert_lf_to_crlf(plain);
        assert!(matches!(cow, std::borrow::Cow::Borrowed(_)));
    }
}
