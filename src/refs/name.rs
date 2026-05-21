//! Validated ref name type.
//!
//! git's `check_ref_format` rejects names with `..`, `//`, control chars,
//! whitespace, `~`, `^`, `:`, `?`, `*`, `[`, `\`, `@{`, names ending with `.lock`
//! or `/`, names starting with `.`, lone `@`, etc. We replicate the most common
//! checks here. (The truly esoteric corner — UTF-8 BOM in component names — is
//! fine to skip until something complains.)

use std::fmt;

use thiserror::Error;

/// A fully-qualified ref name (e.g. `HEAD`, `refs/heads/main`, `refs/tags/v1`).
///
/// Construct via `FullName::new`. Stored as an owned `String` of validated bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FullName(String);

impl FullName {
    /// Construct after validating against `git check-ref-format` rules.
    pub fn new(name: impl Into<String>) -> Result<Self, RefNameError> {
        let s: String = name.into();
        validate(&s)?;
        Ok(FullName(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True for names directly under `.git/` (HEAD, FETCH_HEAD, etc.) — never
    /// under `refs/`.
    pub fn is_pseudo(&self) -> bool {
        !self.0.contains('/') && self.0.chars().all(|c| c.is_ascii_uppercase() || c == '_')
    }

    /// The path under `.git/` where the loose ref is stored. For pseudo refs
    /// like `HEAD` this is just `HEAD`; for `refs/heads/main` this is the same.
    pub fn loose_path_relative(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for FullName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for FullName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

fn validate(name: &str) -> Result<(), RefNameError> {
    if name.is_empty() {
        return Err(RefNameError::Empty);
    }
    if name == "@" {
        return Err(RefNameError::LoneAt);
    }
    if name.starts_with('/') || name.ends_with('/') {
        return Err(RefNameError::SlashEdge);
    }
    if name.starts_with('.') {
        return Err(RefNameError::LeadingDot);
    }
    if name.ends_with('.') {
        return Err(RefNameError::TrailingDot);
    }
    if name.ends_with(".lock") {
        return Err(RefNameError::LockSuffix);
    }
    if name.contains("..") {
        return Err(RefNameError::DoubleDot);
    }
    if name.contains("//") {
        return Err(RefNameError::DoubleSlash);
    }
    if name.contains("@{") {
        return Err(RefNameError::AtBrace);
    }
    for (i, b) in name.bytes().enumerate() {
        match b {
            0..=0x1f | 0x7f => return Err(RefNameError::ControlChar(i, b)),
            b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\' => {
                return Err(RefNameError::ForbiddenChar(b as char));
            }
            _ => {}
        }
    }
    // Each `/`-separated component must not start with `.` or end with `.lock`.
    for comp in name.split('/') {
        if comp.is_empty() {
            return Err(RefNameError::DoubleSlash);
        }
        if comp.starts_with('.') {
            return Err(RefNameError::LeadingDot);
        }
        if comp.ends_with(".lock") {
            return Err(RefNameError::LockSuffix);
        }
    }
    Ok(())
}

#[derive(Error, Debug, PartialEq, Eq)]
pub enum RefNameError {
    #[error("empty ref name")]
    Empty,
    #[error("ref name '@' is reserved")]
    LoneAt,
    #[error("ref name cannot start or end with '/'")]
    SlashEdge,
    #[error("ref name component cannot start with '.'")]
    LeadingDot,
    #[error("ref name cannot end with '.'")]
    TrailingDot,
    #[error("ref name component cannot end with '.lock'")]
    LockSuffix,
    #[error("ref name cannot contain '..'")]
    DoubleDot,
    #[error("ref name cannot contain '//'")]
    DoubleSlash,
    #[error("ref name cannot contain '@{{'")]
    AtBrace,
    #[error("ref name contains forbidden character {0:?}")]
    ForbiddenChar(char),
    #[error("ref name contains control character at byte {0}: {1:#x}")]
    ControlChar(usize, u8),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_common_refs() {
        for name in [
            "HEAD",
            "ORIG_HEAD",
            "FETCH_HEAD",
            "refs/heads/main",
            "refs/heads/feature/x",
            "refs/tags/v1.0.0",
            "refs/remotes/origin/main",
            "refs/remotes/origin/HEAD",
        ] {
            FullName::new(name).unwrap_or_else(|e| panic!("rejected {name}: {e}"));
        }
    }

    #[test]
    fn rejects_obvious_garbage() {
        for (name, _why) in [
            ("", "empty"),
            ("@", "lone @"),
            ("/refs/heads/main", "leading slash"),
            ("refs/heads/main/", "trailing slash"),
            ("refs/heads/.hidden", "leading dot in component"),
            ("refs/heads/main.lock", "lock suffix"),
            ("refs/heads/foo..bar", "double dot"),
            ("refs/heads//main", "double slash"),
            ("refs/heads/@{0}", "@{ pattern"),
            ("refs/heads/foo bar", "space"),
            ("refs/heads/foo~1", "tilde"),
            ("refs/heads/foo^", "caret"),
            ("refs/heads/foo*", "star"),
            ("refs/heads/foo?", "question"),
            ("refs/heads/foo:bar", "colon"),
            ("refs/heads/foo[bar", "bracket"),
            ("refs/heads/foo\\bar", "backslash"),
        ] {
            assert!(FullName::new(name).is_err(), "should reject {name}");
        }
    }

    #[test]
    fn pseudo_refs_detected() {
        assert!(FullName::new("HEAD").unwrap().is_pseudo());
        assert!(FullName::new("FETCH_HEAD").unwrap().is_pseudo());
        assert!(!FullName::new("refs/heads/main").unwrap().is_pseudo());
    }
}
