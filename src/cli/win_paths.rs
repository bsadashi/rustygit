//! Windows-specific path normalization helpers.
//!
//! Used at workdir/index boundaries:
//!   * Translate `\` → `/` for index storage.
//!   * Strip the `\\?\` long-path prefix on display.
//!   * Honor case-insensitive FS on lookup (best-effort).
//!   * Render non-UTF-8 path bytes in `%xx` form for error messages.

pub fn to_index(path: &str) -> String {
    #[cfg(windows)]
    {
        path.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        path.to_string()
    }
}

pub fn from_index(path: &str) -> String {
    #[cfg(windows)]
    {
        path.replace('/', "\\")
    }
    #[cfg(not(windows))]
    {
        path.to_string()
    }
}

/// Drop the Windows long-path namespace prefix (`\\?\`) for display.
pub fn strip_long_prefix(path: &str) -> &str {
    path.strip_prefix(r"\\?\").unwrap_or(path)
}

/// Render `bytes` as a path-display string. ASCII-printable bytes (and
/// `/`, `\`, `.`, `_`, `-`) are emitted verbatim; everything else is
/// hex-escaped as `\xNN`. Used by [`UnpackError::PathEncodingError`] and
/// similar refusal errors so the offending bytes are recoverable even when
/// the OS layer can't surface them.
pub fn format_path_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'/' | b'\\' | b'.' | b'_' | b'-' => out.push(b as char),
            0x20..=0x7e => out.push(b as char),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\x{b:02x}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_long_prefix_works() {
        assert_eq!(strip_long_prefix(r"\\?\C:\foo"), r"C:\foo");
        assert_eq!(strip_long_prefix("/usr/local"), "/usr/local");
    }

    #[test]
    fn to_index_identity_on_unix() {
        // The doc claim "identity on Unix" is what protects 1000+ existing
        // tests from regressing when we wire `to_index` into add/rm/mv.
        #[cfg(not(windows))]
        assert_eq!(to_index("a/b/c"), "a/b/c");
        #[cfg(not(windows))]
        assert_eq!(to_index("a\\b\\c"), "a\\b\\c");
    }

    #[test]
    fn format_path_bytes_ascii_passthrough() {
        assert_eq!(format_path_bytes(b"hello/world.txt"), "hello/world.txt");
    }

    #[test]
    fn format_path_bytes_hex_escapes_non_utf8() {
        // The classic non-UTF-8 example: Latin-1 'é' as the lone byte 0xe9.
        let mixed = b"caf\xe9.txt";
        assert_eq!(format_path_bytes(mixed), r"caf\xe9.txt");
    }

    #[test]
    fn format_path_bytes_escapes_control_chars() {
        assert_eq!(format_path_bytes(b"a\x00b\x01"), r"a\x00b\x01");
    }
}
