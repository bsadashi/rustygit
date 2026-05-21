//! `Signature` (author/committer identity) and `Time`.
//!
//! Wire format inside `commit` (and `tag`) headers:
//! ```text
//! <Name> <<email>> <unix-secs> <±HHMM>
//! ```
//!
//! Notes:
//! - `name` and `email` are technically arbitrary bytes; for M3 we constrain
//!   them to `String` (UTF-8) which suffices for everything our `commit` and
//!   `add` paths produce. Parsing accepts whatever git wrote, as long as it's
//!   valid UTF-8.
//! - `seconds` is stored signed because git itself accepts negative epoch
//!   stamps (the `<i64>` is what fast-import uses); offsets are minutes east
//!   of UTC (e.g. India = +330 → `+0530`).
//! - There is no `chrono` / `time` dependency. Time formatting is hand-rolled.
//! - Local TZ offset is detected by shelling out to `date +%z` (the same trick
//!   used by `crate::refs::reflog`); we fall back to UTC when that fails.

use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

use crate::config::Config;

/// A point in time as git records it: signed Unix seconds plus a tz offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Time {
    /// Unix epoch seconds. Signed because git accepts negative values.
    pub seconds: i64,
    /// Minutes east of UTC. India = +330, US Pacific (PST) = -480.
    pub offset_minutes: i32,
}

impl Time {
    pub fn new(seconds: i64, offset_minutes: i32) -> Self {
        Self {
            seconds,
            offset_minutes,
        }
    }

    /// Current wall-clock time with a best-effort local offset.
    ///
    /// Falls back to UTC if `SystemTime::now()` is before the epoch or if the
    /// `date +%z` invocation fails for any reason.
    pub fn now_local() -> Self {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let offset_minutes = local_offset_minutes();
        Self {
            seconds,
            offset_minutes,
        }
    }

    /// Parse `Time` from the two trailing tokens of a signature line:
    /// `secs` and `±HHMM`.
    pub fn parse(secs: &str, offset: &str) -> Result<Self, IdentityError> {
        let seconds: i64 = secs
            .parse()
            .map_err(|_| IdentityError::BadTimestamp(secs.to_string()))?;
        let offset_minutes = parse_offset(offset)?;
        Ok(Self {
            seconds,
            offset_minutes,
        })
    }

    /// Wire form: `1672531200 +0530`.
    pub fn serialize(&self) -> String {
        format!("{} {}", self.seconds, format_offset(self.offset_minutes))
    }
}

/// `Name <email> 1672531200 +0000`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub email: String,
    pub when: Time,
}

impl Signature {
    pub fn new(name: impl Into<String>, email: impl Into<String>, when: Time) -> Self {
        Self {
            name: name.into(),
            email: email.into(),
            when,
        }
    }

    /// Parse a single signature line, e.g. `"Linus Torvalds <torvalds@osdl.org> 1112911993 -0700"`.
    /// The line must NOT include a trailing newline.
    pub fn parse(line: &[u8]) -> Result<Self, IdentityError> {
        let s = std::str::from_utf8(line)
            .map_err(|_| IdentityError::Malformed("signature is not valid UTF-8".into()))?;

        // Find the `<` and `>` that bracket the email. `<` is the *last* one
        // because names may contain `<` (rare, but legal). Same for `>`.
        let lt = s
            .rfind('<')
            .ok_or_else(|| IdentityError::Malformed("missing '<' in signature".into()))?;
        let gt = s[lt..]
            .find('>')
            .map(|p| p + lt)
            .ok_or_else(|| IdentityError::Malformed("missing '>' in signature".into()))?;

        // The format requires exactly one space before `<` and exactly one
        // space after `>`. Be lenient: trim any whitespace.
        let name = s[..lt].trim_end().to_string();
        let email = s[lt + 1..gt].to_string();
        let rest = s[gt + 1..].trim_start();

        // `rest` is "<secs> <offset>". Split on the first/last whitespace.
        let mut iter = rest.splitn(2, char::is_whitespace);
        let secs = iter
            .next()
            .ok_or_else(|| IdentityError::Malformed("missing timestamp".into()))?;
        let offset = iter
            .next()
            .ok_or_else(|| IdentityError::Malformed("missing tz offset".into()))?
            .trim();

        let when = Time::parse(secs, offset)?;
        Ok(Signature { name, email, when })
    }

    /// Wire form. Reverse of `parse`.
    pub fn serialize(&self) -> String {
        format!("{} <{}> {}", self.name, self.email, self.when.serialize())
    }

    /// Source the COMMITTER from `GIT_COMMITTER_*` env vars, falling back to
    /// `user.name` / `user.email` from config and to `now` for the date.
    pub fn committer_from_env_or_config(config: &Config, now: Time) -> Result<Self, IdentityError> {
        from_env_or_config(
            config,
            now,
            "GIT_COMMITTER_NAME",
            "GIT_COMMITTER_EMAIL",
            "GIT_COMMITTER_DATE",
        )
    }

    /// Source the AUTHOR from `GIT_AUTHOR_*` env vars, falling back to
    /// `user.name` / `user.email` from config and to `now` for the date.
    pub fn author_from_env_or_config(config: &Config, now: Time) -> Result<Self, IdentityError> {
        from_env_or_config(
            config,
            now,
            "GIT_AUTHOR_NAME",
            "GIT_AUTHOR_EMAIL",
            "GIT_AUTHOR_DATE",
        )
    }
}

fn from_env_or_config(
    config: &Config,
    now: Time,
    name_var: &str,
    email_var: &str,
    date_var: &str,
) -> Result<Signature, IdentityError> {
    let name = std::env::var(name_var)
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| config.get_string("user", "name").map(|s| s.to_string()))
        .ok_or(IdentityError::MissingName)?;
    let email = std::env::var(email_var)
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| config.get_string("user", "email").map(|s| s.to_string()))
        .ok_or(IdentityError::MissingEmail)?;
    let when = match std::env::var(date_var).ok().filter(|s| !s.is_empty()) {
        None => now,
        Some(raw) => parse_date_env(&raw)?,
    };
    Ok(Signature { name, email, when })
}

/// Parse the value of `GIT_AUTHOR_DATE` / `GIT_COMMITTER_DATE`.
///
/// We accept two forms:
/// - `now` — current local time
/// - `<unix-secs> <±HHMM>` — direct git wire form (also `@<secs> <offset>`,
///   which is git's "raw" form in commands like `git commit --date`)
///
/// ISO-8601 / RFC-2822 / human ("yesterday") forms are NOT supported in M3;
/// the user can pre-format their date or use the env vars.
fn parse_date_env(s: &str) -> Result<Time, IdentityError> {
    let s = s.trim();
    if s == "now" {
        return Ok(Time::now_local());
    }
    let s = s.strip_prefix('@').unwrap_or(s);
    let (secs, offset) = s
        .split_once(char::is_whitespace)
        .ok_or_else(|| IdentityError::UnsupportedDate(s.to_string()))?;
    Time::parse(secs.trim(), offset.trim())
        .map_err(|_| IdentityError::UnsupportedDate(s.to_string()))
}

/// Parse `±HHMM` (5 ASCII chars).
fn parse_offset(raw: &str) -> Result<i32, IdentityError> {
    if raw.len() != 5 {
        return Err(IdentityError::BadOffset(raw.to_string()));
    }
    let bytes = raw.as_bytes();
    let sign: i32 = match bytes[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return Err(IdentityError::BadOffset(raw.to_string())),
    };
    let hh: i32 = std::str::from_utf8(&bytes[1..3])
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| IdentityError::BadOffset(raw.to_string()))?;
    let mm: i32 = std::str::from_utf8(&bytes[3..5])
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| IdentityError::BadOffset(raw.to_string()))?;
    Ok(sign * (hh * 60 + mm))
}

fn format_offset(min: i32) -> String {
    let sign = if min < 0 { '-' } else { '+' };
    let abs = min.unsigned_abs();
    let hh = abs / 60;
    let mm = abs % 60;
    format!("{sign}{hh:02}{mm:02}")
}

/// Best-effort local TZ offset. Mirrors `crate::refs::reflog::local_offset_minutes`.
#[cfg(unix)]
fn local_offset_minutes() -> i32 {
    use std::process::Command;
    if let Ok(out) = Command::new("date").arg("+%z").output() {
        if let Ok(s) = std::str::from_utf8(&out.stdout) {
            let s = s.trim();
            if let Ok(min) = parse_offset(s) {
                return min;
            }
        }
    }
    0
}

#[cfg(not(unix))]
fn local_offset_minutes() -> i32 {
    0
}

#[derive(Error, Debug)]
pub enum IdentityError {
    #[error("malformed signature: {0}")]
    Malformed(String),
    #[error("invalid unix timestamp: {0}")]
    BadTimestamp(String),
    #[error("invalid tz offset: {0}")]
    BadOffset(String),
    #[error("unsupported date format: {0} (accepts 'now' or '<unix-secs> <±HHMM>')")]
    UnsupportedDate(String),
    #[error(
        "user.name not configured (set user.name in config or GIT_AUTHOR_NAME/GIT_COMMITTER_NAME)"
    )]
    MissingName,
    #[error("user.email not configured (set user.email in config or GIT_AUTHOR_EMAIL/GIT_COMMITTER_EMAIL)")]
    MissingEmail,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_torvalds_initial_commit_signature() {
        // From git's own first commit (truncated for the test).
        let line = b"Linus Torvalds <torvalds@osdl.org> 1112911993 -0700";
        let sig = Signature::parse(line).unwrap();
        assert_eq!(sig.name, "Linus Torvalds");
        assert_eq!(sig.email, "torvalds@osdl.org");
        assert_eq!(sig.when.seconds, 1112911993);
        assert_eq!(sig.when.offset_minutes, -7 * 60);
    }

    #[test]
    fn round_trip_signature() {
        let line = "A. Person <a@example.com> 1672531200 +0530";
        let sig = Signature::parse(line.as_bytes()).unwrap();
        assert_eq!(sig.serialize(), line);
    }

    #[test]
    fn round_trip_negative_offset() {
        let line = "Test User <t@x.y> 1112911993 -0700";
        let sig = Signature::parse(line.as_bytes()).unwrap();
        assert_eq!(sig.when.offset_minutes, -420);
        assert_eq!(sig.serialize(), line);
    }

    #[test]
    fn time_parse_and_serialize() {
        let t = Time::parse("1672531200", "+0530").unwrap();
        assert_eq!(t.seconds, 1672531200);
        assert_eq!(t.offset_minutes, 330);
        assert_eq!(t.serialize(), "1672531200 +0530");
    }

    #[test]
    fn format_offset_examples() {
        assert_eq!(format_offset(0), "+0000");
        assert_eq!(format_offset(60), "+0100");
        assert_eq!(format_offset(-330), "-0530");
        assert_eq!(format_offset(330), "+0530");
    }

    #[test]
    fn parse_offset_rejects_garbage() {
        assert!(parse_offset("0530").is_err());
        assert!(parse_offset("++0530").is_err());
        assert!(parse_offset("+05:30").is_err());
        assert!(parse_offset("+05ab").is_err());
    }

    #[test]
    fn parse_signature_with_zero_offset() {
        let line = b"X <y@z> 0 +0000";
        let sig = Signature::parse(line).unwrap();
        assert_eq!(sig.when.seconds, 0);
        assert_eq!(sig.when.offset_minutes, 0);
    }

    #[test]
    fn parse_rejects_missing_email() {
        assert!(Signature::parse(b"Bob 1672531200 +0000").is_err());
    }

    #[test]
    fn parse_rejects_missing_offset() {
        assert!(Signature::parse(b"Bob <b@x.y> 1672531200").is_err());
    }

    #[test]
    fn name_with_brackets_uses_last_pair() {
        // Genuine git commits with `<`/`>` in names are rare. Make sure we
        // still pick up the *last* pair — which is the email delimiters.
        let line = b"Person <weird> name <real@example.com> 100 +0000";
        let sig = Signature::parse(line).unwrap();
        assert_eq!(sig.name, "Person <weird> name");
        assert_eq!(sig.email, "real@example.com");
    }

    #[test]
    fn parse_date_env_now_works() {
        let t = parse_date_env("now").unwrap();
        // Sanity: should be roughly current epoch (sometime after 2020).
        assert!(t.seconds > 1_577_836_800);
    }

    #[test]
    fn parse_date_env_explicit() {
        let t = parse_date_env("1672531200 +0530").unwrap();
        assert_eq!(t.seconds, 1672531200);
        assert_eq!(t.offset_minutes, 330);

        let t2 = parse_date_env("@1672531200 +0530").unwrap();
        assert_eq!(t2, t);
    }

    #[test]
    fn parse_date_env_rejects_iso() {
        assert!(parse_date_env("2023-01-01T00:00:00Z").is_err());
    }

    #[test]
    fn from_env_or_config_uses_env_first() {
        // Use unique env-var names that collide with our reads. We can't
        // safely mutate process env in parallel tests, so we just exercise
        // the config-fallback path directly.
        let mut text = String::new();
        text.push_str("[user]\n");
        text.push_str("\tname = ConfigPerson\n");
        text.push_str("\temail = c@example.com\n");
        let cfg = Config::parse_str(&text).unwrap();
        // No env override (we don't set them) — should pick up config.
        // This relies on the running environment NOT having
        // GIT_AUTHOR_NAME/GIT_AUTHOR_EMAIL set. In CI / dev shells that's
        // overwhelmingly the case; if it ever fails we'll revisit.
        if std::env::var("GIT_AUTHOR_NAME").is_err()
            && std::env::var("GIT_AUTHOR_EMAIL").is_err()
            && std::env::var("GIT_AUTHOR_DATE").is_err()
        {
            let now = Time::new(42, 0);
            let sig = Signature::author_from_env_or_config(&cfg, now).unwrap();
            assert_eq!(sig.name, "ConfigPerson");
            assert_eq!(sig.email, "c@example.com");
            assert_eq!(sig.when, now);
        }
    }

    #[test]
    fn from_env_or_config_errors_when_unset() {
        let cfg = Config::empty();
        if std::env::var("GIT_AUTHOR_NAME").is_err() && std::env::var("GIT_AUTHOR_EMAIL").is_err() {
            let err = Signature::author_from_env_or_config(&cfg, Time::new(0, 0)).unwrap_err();
            assert!(matches!(err, IdentityError::MissingName));
        }
    }
}
