//! `rustygit count-objects` — print loose / packed object counts and
//! their on-disk sizes.
//!
//! Output (terse, default):
//!   `<N> objects, <KB> kilobytes`
//!
//! Verbose (`-v`):
//!   ```text
//!   count: <N>
//!   size: <KB>
//!   in-pack: <P>
//!   packs: <K>
//!   size-pack: <KB>
//!   prune-packable: <N>
//!   garbage: <N>
//!   size-garbage: <KB>
//!   ```

use std::io;
use std::path::Path;

use clap::Args;

use crate::repo::Repository;

#[derive(Debug, Args)]
pub struct CountObjectsArgs {
    /// Verbose multi-line output.
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,
    /// Print human-readable sizes.
    #[arg(short = 'H', long = "human-readable")]
    pub human_readable: bool,
}

pub fn run(args: CountObjectsArgs) -> io::Result<i32> {
    let repo = Repository::discover_from_cwd().map_err(io_err)?;
    let stats = collect_stats(repo.gitdir().join("objects").as_path())?;

    if args.verbose {
        println!("count: {}", stats.loose_count);
        println!(
            "size: {}",
            format_size(stats.loose_bytes, args.human_readable)
        );
        println!("in-pack: {}", stats.in_pack);
        println!("packs: {}", stats.pack_files);
        println!(
            "size-pack: {}",
            format_size(stats.pack_bytes, args.human_readable)
        );
        println!("prune-packable: {}", stats.prune_packable);
        println!("garbage: {}", stats.garbage_count);
        println!(
            "size-garbage: {}",
            format_size(stats.garbage_bytes, args.human_readable)
        );
    } else {
        println!(
            "{} objects, {} kilobytes",
            stats.loose_count,
            stats.loose_bytes / 1024
        );
    }
    Ok(0)
}

#[derive(Debug, Default)]
pub struct ObjectStats {
    pub loose_count: u64,
    pub loose_bytes: u64,
    pub in_pack: u64,
    pub pack_files: u64,
    pub pack_bytes: u64,
    pub prune_packable: u64,
    pub garbage_count: u64,
    pub garbage_bytes: u64,
}

pub fn collect_stats(objects_dir: &Path) -> io::Result<ObjectStats> {
    let mut stats = ObjectStats::default();

    // Walk the 256 fanout dirs `objects/xx/`.
    let entries = match std::fs::read_dir(objects_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(stats),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.len() == 2 && name_str.chars().all(|c| c.is_ascii_hexdigit()) {
            // Loose-object fanout dir.
            if let Ok(inner) = std::fs::read_dir(&path) {
                for f in inner.flatten() {
                    let fname = f.file_name();
                    let fname_str = fname.to_string_lossy();
                    if fname_str.len() == 38 && fname_str.chars().all(|c| c.is_ascii_hexdigit()) {
                        stats.loose_count += 1;
                        if let Ok(meta) = f.metadata() {
                            stats.loose_bytes += meta.len();
                        }
                    } else {
                        // Garbage in a fanout dir.
                        stats.garbage_count += 1;
                        if let Ok(meta) = f.metadata() {
                            stats.garbage_bytes += meta.len();
                        }
                    }
                }
            }
        } else if name_str == "pack" {
            if let Ok(packs) = std::fs::read_dir(&path) {
                for p in packs.flatten() {
                    let pname = p.file_name();
                    let pname_str = pname.to_string_lossy();
                    if pname_str.ends_with(".pack") {
                        stats.pack_files += 1;
                        if let Ok(meta) = p.metadata() {
                            stats.pack_bytes += meta.len();
                        }
                        // Count objects in this pack via its idx file.
                        let idx = path.join(pname_str.replace(".pack", ".idx"));
                        if let Ok(in_pack) = count_objects_in_idx(&idx) {
                            stats.in_pack += in_pack;
                        }
                    } else if !pname_str.ends_with(".idx")
                        && !pname_str.ends_with(".rev")
                        && !pname_str.ends_with(".bitmap")
                        && !pname_str.ends_with(".keep")
                        && !pname_str.ends_with(".mtimes")
                        && !pname_str.ends_with(".promisor")
                    {
                        stats.garbage_count += 1;
                        if let Ok(meta) = p.metadata() {
                            stats.garbage_bytes += meta.len();
                        }
                    }
                }
            }
        }
    }
    Ok(stats)
}

/// Read the fan-out table from a v2 .idx file's last fanout slot to get
/// the object count.
fn count_objects_in_idx(path: &Path) -> io::Result<u64> {
    let bytes = std::fs::read(path)?;
    // v2 .idx starts with: magic(4) + version(4) + 256*u32 fanout.
    // The last fanout slot is the total object count.
    if bytes.len() < 8 + 256 * 4 {
        return Ok(0);
    }
    let off = 8 + 255 * 4;
    let n = u32::from_be_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]);
    Ok(n as u64)
}

fn format_size(bytes: u64, human: bool) -> String {
    if !human {
        // KB (1000-based to match git's --bytes-per-1000 behavior would be
        // wrong; git actually uses 1024-based KB here).
        return format!("{}", bytes / 1024);
    }
    if bytes < 1024 {
        format!("{bytes} bytes")
    } else if bytes < 1024 * 1024 {
        format!("{:.2} KiB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.2} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_objects_dir_yields_zeroes() {
        let tmp = tempfile::tempdir().unwrap();
        let stats = collect_stats(tmp.path()).unwrap();
        assert_eq!(stats.loose_count, 0);
        assert_eq!(stats.pack_files, 0);
    }

    #[test]
    fn loose_object_in_fanout_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let fanout = tmp.path().join("ab");
        std::fs::create_dir_all(&fanout).unwrap();
        let oid_rest = "c".repeat(38);
        std::fs::write(fanout.join(&oid_rest), b"loose-data").unwrap();
        let stats = collect_stats(tmp.path()).unwrap();
        assert_eq!(stats.loose_count, 1);
        assert_eq!(stats.loose_bytes, b"loose-data".len() as u64);
    }

    #[test]
    fn garbage_file_in_fanout_is_counted_as_garbage() {
        let tmp = tempfile::tempdir().unwrap();
        let fanout = tmp.path().join("ab");
        std::fs::create_dir_all(&fanout).unwrap();
        std::fs::write(fanout.join("not-an-oid"), b"junk").unwrap();
        let stats = collect_stats(tmp.path()).unwrap();
        assert_eq!(stats.loose_count, 0);
        assert_eq!(stats.garbage_count, 1);
    }
}
