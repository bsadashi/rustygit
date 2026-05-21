//! `rustygit` i18n runtime: load message catalogs at startup.
//!
//! The `i18n::tr` hook is a `const fn` identity today. To enable real
//! translation:
//!   1. At startup, look up `LC_ALL`/`LC_MESSAGES`/`LANG`.
//!   2. If a `.mo` file for that locale is bundled (under `share/locale/`
//!      relative to the binary) load it into a process-wide map.
//!   3. Replace `tr()` calls' compile-time lookup with a runtime lookup.
//!
//! Note: this is a no-op when no `.mo` is found, preserving the
//! English-only default. Real catalog content for non-English locales
//! is left as a documented "ship empty catalogs" item: the gettext
//! pipeline (xgettext → msgfmt) needs to run as part of the release
//! to populate the bundles.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

static CATALOG: OnceLock<HashMap<String, String>> = OnceLock::new();

pub fn init() {
    // Pick locale.
    let locale = std::env::var("LC_ALL")
        .ok()
        .or_else(|| std::env::var("LC_MESSAGES").ok())
        .or_else(|| std::env::var("LANG").ok())
        .unwrap_or_default();
    // Strip the encoding suffix and any @modifier.
    let base = locale
        .split('.')
        .next()
        .unwrap_or("")
        .split('@')
        .next()
        .unwrap_or("");
    if base.is_empty() || base == "C" || base == "POSIX" {
        let _ = CATALOG.set(HashMap::new());
        return;
    }
    // Look under `share/locale/<base>/LC_MESSAGES/rustygit.mo` relative
    // to the binary path.
    let exe = std::env::current_exe().ok();
    let candidates: Vec<PathBuf> = exe
        .into_iter()
        .flat_map(|p| {
            let parent = p.parent().map(PathBuf::from).unwrap_or_default();
            vec![
                parent
                    .join("..")
                    .join("share/locale")
                    .join(base)
                    .join("LC_MESSAGES/rustygit.mo"),
                parent
                    .join("share/locale")
                    .join(base)
                    .join("LC_MESSAGES/rustygit.mo"),
            ]
        })
        .collect();
    for path in candidates {
        if let Ok(bytes) = std::fs::read(&path) {
            if let Some(map) = parse_mo(&bytes) {
                let _ = CATALOG.set(map);
                return;
            }
        }
    }
    let _ = CATALOG.set(HashMap::new());
}

/// Look up an English-source string in the loaded catalog. Returns
/// the translated string if present, else the original.
pub fn translate(source: &str) -> &str {
    if let Some(map) = CATALOG.get() {
        if let Some(t) = map.get(source) {
            return t.as_str();
        }
    }
    source
}

/// Minimal `.mo` file parser. The format (per GNU gettext):
///   0x00–03: magic (`0x950412de` little-endian or `0xde120495` big-endian)
///   0x04–07: format-revision
///   0x08–0B: number of strings
///   0x0C–0F: offset of original-strings table
///   0x10–13: offset of translated-strings table
///   Each string table is N×(len,offset).
fn parse_mo(bytes: &[u8]) -> Option<HashMap<String, String>> {
    if bytes.len() < 28 {
        return None;
    }
    let magic_le = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let big_endian = magic_le != 0x950412de;
    let read_u32 = |off: usize| -> u32 {
        if big_endian {
            u32::from_be_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
        } else {
            u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
        }
    };
    let n = read_u32(8) as usize;
    let orig_off = read_u32(12) as usize;
    let trans_off = read_u32(16) as usize;
    let mut out = HashMap::new();
    for i in 0..n {
        let o = orig_off + 8 * i;
        let t = trans_off + 8 * i;
        if o + 8 > bytes.len() || t + 8 > bytes.len() {
            return None;
        }
        let olen = read_u32(o) as usize;
        let ooff = read_u32(o + 4) as usize;
        let tlen = read_u32(t) as usize;
        let toff = read_u32(t + 4) as usize;
        if ooff + olen > bytes.len() || toff + tlen > bytes.len() {
            return None;
        }
        let key = String::from_utf8_lossy(&bytes[ooff..ooff + olen]).into_owned();
        let val = String::from_utf8_lossy(&bytes[toff..toff + tlen]).into_owned();
        out.insert(key, val);
    }
    Some(out)
}
