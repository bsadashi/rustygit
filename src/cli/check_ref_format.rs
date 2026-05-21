//! `rustygit check-ref-format` — validate a ref name against git's rules.
//!
//! Rules (per `git-check-ref-format(1)`):
//!   1. No slash-separated component (a "level") can begin with `.` or
//!      end with `.lock`.
//!   2. The name must contain at least one `/` unless `--allow-onelevel`.
//!   3. May not contain `..`, ASCII control characters, ` `, `~`, `^`,
//!      `:`, `?`, `*`, `[`, `\\`, or `@{`.
//!   4. Cannot start or end with `/`, or contain consecutive `/`.
//!   5. Cannot end with `.`.
//!   6. The single name `@` alone is invalid.
//!   7. May not contain a backslash anywhere.
//!
//! `--branch` validates a branch shorthand (allow-onelevel + extra
//! denylist for `-` / `HEAD`).

use std::io;

use clap::Args;

#[derive(Debug, Args)]
pub struct CheckRefFormatArgs {
    /// Permit single-level names like `master` (no slash required).
    #[arg(long = "allow-onelevel")]
    pub allow_onelevel: bool,
    /// Validate as a branch name (looser shorthand rules).
    #[arg(long = "branch")]
    pub branch: bool,
    /// Permit `*` as the last path component (refspec patterns).
    #[arg(long = "refspec-pattern")]
    pub refspec_pattern: bool,
    /// Refs to validate (one positional, like git).
    #[arg(value_name = "REF", required = true)]
    pub names: Vec<String>,
}

pub fn run(args: CheckRefFormatArgs) -> io::Result<i32> {
    if args.names.len() != 1 {
        eprintln!("rustygit: check-ref-format: exactly one <ref> argument required");
        return Ok(129);
    }
    let name = &args.names[0];
    let opts = ValidateOpts {
        allow_onelevel: args.allow_onelevel || args.branch,
        refspec_pattern: args.refspec_pattern,
        branch: args.branch,
    };
    if is_valid_ref_name(name, &opts) {
        if args.branch {
            // git's --branch echoes the name on stdout (used by completion).
            println!("{name}");
        }
        Ok(0)
    } else {
        Ok(1)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ValidateOpts {
    pub allow_onelevel: bool,
    pub refspec_pattern: bool,
    pub branch: bool,
}

/// Check `name` against every rule. Returns true iff it's a valid ref.
pub fn is_valid_ref_name(name: &str, opts: &ValidateOpts) -> bool {
    if name.is_empty() {
        return false;
    }
    if name == "@" {
        return false;
    }
    if name.starts_with('/') || name.ends_with('/') {
        return false;
    }
    if name.starts_with('-') {
        // Disallow leading "-" (matches `--` argument).
        return false;
    }
    if name.contains("//") {
        return false;
    }
    if name.contains("..") {
        return false;
    }
    if name.contains("@{") {
        return false;
    }
    if name.contains('\\') {
        return false;
    }
    // ASCII control + denylisted single chars.
    for b in name.bytes() {
        if b < 0x20 || b == 0x7f {
            return false;
        }
        match b {
            b' ' | b'~' | b'^' | b':' | b'?' | b'[' => return false,
            b'*' if !opts.refspec_pattern => return false,
            _ => {}
        }
    }
    if name.ends_with('.') {
        return false;
    }

    // Per-component rules.
    let mut components = name.split('/').collect::<Vec<_>>();
    // refspec-pattern: trailing "*" component is allowed but should not
    // trigger the "ends with .lock" check.
    if opts.refspec_pattern && components.last() == Some(&"*") {
        components.pop();
    }
    if components.is_empty() {
        return false;
    }
    if !opts.allow_onelevel && components.len() == 1 {
        return false;
    }
    for comp in &components {
        if comp.is_empty() {
            return false;
        }
        if comp.starts_with('.') {
            return false;
        }
        if comp.ends_with(".lock") {
            return false;
        }
    }

    // --branch: also forbid HEAD as a branch shorthand.
    if opts.branch && name == "HEAD" {
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vopts(allow_one: bool) -> ValidateOpts {
        ValidateOpts {
            allow_onelevel: allow_one,
            ..Default::default()
        }
    }

    #[test]
    fn rejects_empty_string() {
        assert!(!is_valid_ref_name("", &vopts(true)));
    }

    #[test]
    fn rejects_at() {
        assert!(!is_valid_ref_name("@", &vopts(true)));
    }

    #[test]
    fn rejects_two_dots() {
        assert!(!is_valid_ref_name("refs/foo..bar", &vopts(false)));
    }

    #[test]
    fn rejects_component_starting_with_dot() {
        assert!(!is_valid_ref_name("refs/.hidden/main", &vopts(false)));
    }

    #[test]
    fn rejects_component_ending_with_lock() {
        assert!(!is_valid_ref_name("refs/heads/main.lock", &vopts(false)));
    }

    #[test]
    fn rejects_double_slash() {
        assert!(!is_valid_ref_name("refs//main", &vopts(false)));
    }

    #[test]
    fn rejects_trailing_slash() {
        assert!(!is_valid_ref_name("refs/heads/", &vopts(false)));
    }

    #[test]
    fn rejects_at_brace() {
        assert!(!is_valid_ref_name("foo@{0}", &vopts(true)));
    }

    #[test]
    fn rejects_control_chars() {
        assert!(!is_valid_ref_name("refs/heads/\x01x", &vopts(false)));
    }

    #[test]
    fn rejects_space_and_special() {
        for bad in [
            "refs/with space",
            "refs/with~tilde",
            "refs/with^caret",
            "refs/with:colon",
            "refs/with?qm",
            "refs/with[bracket",
            "refs/star*",
        ] {
            assert!(
                !is_valid_ref_name(bad, &vopts(false)),
                "should reject {bad}"
            );
        }
    }

    #[test]
    fn rejects_onelevel_without_flag() {
        assert!(!is_valid_ref_name("main", &vopts(false)));
        assert!(is_valid_ref_name("main", &vopts(true)));
    }

    #[test]
    fn accepts_normal_branch_path() {
        assert!(is_valid_ref_name("refs/heads/main", &vopts(false)));
        assert!(is_valid_ref_name(
            "refs/heads/feature/long-name",
            &vopts(false)
        ));
    }

    #[test]
    fn accepts_trailing_star_with_pattern_flag() {
        let opts = ValidateOpts {
            allow_onelevel: false,
            refspec_pattern: true,
            branch: false,
        };
        assert!(is_valid_ref_name("refs/heads/*", &opts));
        // Without the flag, * is forbidden.
        assert!(!is_valid_ref_name("refs/heads/*", &ValidateOpts::default()));
    }
}
