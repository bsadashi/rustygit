//! `rustygit init` — create or reinitialize a repository.
//!
//! Targets byte-compatibility with `git init` for: `HEAD`, `config`,
//! `info/exclude`, `description`, and the empty `objects/{info,pack}`,
//! `refs/{heads,tags}` directories. The `hooks/*.sample` files git ships
//! are explicitly NOT recreated (we're a binary-only port; per the plan).

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::Args;

use crate::hash::HashKind;

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Directory to initialize. Defaults to the current directory.
    #[arg(value_name = "DIRECTORY", default_value = ".")]
    pub directory: PathBuf,

    /// Object format / hash algorithm. Defaults to sha1.
    #[arg(long = "object-format", value_name = "FORMAT", value_parser = super::parse_hash_kind)]
    pub object_format: Option<HashKind>,

    /// Initial branch name. Defaults to `master` (matching `git init`'s
    /// compiled default; `init.defaultBranch` config lookup arrives in M2).
    #[arg(short = 'b', long = "initial-branch", value_name = "NAME")]
    pub initial_branch: Option<String>,

    /// Print less output.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,

    /// Create a bare repository. Not yet implemented; reserved.
    #[arg(long = "bare")]
    pub bare: bool,
}

pub fn run(args: InitArgs) -> io::Result<i32> {
    if args.bare {
        eprintln!("rustygit: --bare is not yet implemented (M3+)");
        return Ok(128);
    }
    let work = match args.directory.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            fs::create_dir_all(&args.directory)?;
            args.directory.canonicalize()?
        }
    };
    let gitdir = work.join(".git");
    let already = gitdir.is_dir();

    create_layout(&gitdir)?;

    let hash_kind = args.object_format.unwrap_or(HashKind::Sha1);
    let initial_branch = args
        .initial_branch
        .as_deref()
        .unwrap_or(super::DEFAULT_INITIAL_BRANCH);

    write_head(&gitdir, initial_branch)?;
    write_config(&gitdir, hash_kind, &work)?;
    write_description(&gitdir)?;
    write_info_exclude(&gitdir)?;

    if !args.quiet {
        let kind = if already {
            "Reinitialized existing"
        } else {
            "Initialized empty"
        };
        println!(
            "{kind} rustygit repository in {}",
            display_init_path(&gitdir)
        );
    }
    Ok(0)
}

fn create_layout(gitdir: &Path) -> io::Result<()> {
    for sub in [
        "",
        "objects",
        "objects/info",
        "objects/pack",
        "refs",
        "refs/heads",
        "refs/tags",
        "info",
        "hooks",
    ] {
        fs::create_dir_all(gitdir.join(sub))?;
    }
    Ok(())
}

fn write_head(gitdir: &Path, branch: &str) -> io::Result<()> {
    let head = format!("ref: refs/heads/{branch}\n");
    write_atomic(&gitdir.join("HEAD"), head.as_bytes())
}

fn write_description(gitdir: &Path) -> io::Result<()> {
    let body = "Unnamed repository; edit this file 'description' to name the repository.\n";
    write_atomic(&gitdir.join("description"), body.as_bytes())
}

fn write_info_exclude(gitdir: &Path) -> io::Result<()> {
    let body = "\
# git ls-files --others --exclude-from=.git/info/exclude
# Lines that start with '#' are comments.
# For a project mostly in C, the following would be a good set of
# exclude patterns (uncomment them if you want to use them):
# *.[oa]
# *~
";
    write_atomic(&gitdir.join("info").join("exclude"), body.as_bytes())
}

/// Build the `[core]` section that git would produce on this platform.
///
/// git's behavior:
/// - `repositoryformatversion`: 0 for sha1, 1 for sha256 (the latter requires
///   the `objectFormat` extension).
/// - `filemode`: probe the working tree by toggling exec bits.
/// - `bare`: false (we don't support `--bare` in M0).
/// - `logallrefupdates`: true.
/// - `ignorecase`: probe whether the FS treats `foo` and `FOO` as the same.
/// - `precomposeunicode`: macOS only — defaults to true.
///
/// `extensions.objectFormat = sha256` is added for SHA-256 repos.
fn write_config(gitdir: &Path, hash_kind: HashKind, workdir: &Path) -> io::Result<()> {
    let format_version = match hash_kind {
        HashKind::Sha1 => 0,
        HashKind::Sha256 => 1,
    };
    let filemode = probe_filemode(workdir);
    let ignorecase = probe_ignorecase(workdir);
    let precompose = cfg!(target_os = "macos");

    let mut s = String::new();
    s.push_str("[core]\n");
    s.push_str(&format!("\trepositoryformatversion = {format_version}\n"));
    s.push_str(&format!("\tfilemode = {}\n", b2s(filemode)));
    s.push_str("\tbare = false\n");
    s.push_str("\tlogallrefupdates = true\n");
    if ignorecase {
        s.push_str("\tignorecase = true\n");
    }
    if precompose {
        s.push_str("\tprecomposeunicode = true\n");
    }
    if matches!(hash_kind, HashKind::Sha256) {
        s.push_str("[extensions]\n");
        s.push_str("\tobjectformat = sha256\n");
    }
    write_atomic(&gitdir.join("config"), s.as_bytes())
}

fn b2s(b: bool) -> &'static str {
    if b {
        "true"
    } else {
        "false"
    }
}

/// Atomic file write: temp + rename into place. We don't yet have the M1
/// `Lockfile` type; this is the local stand-in.
fn write_atomic(target: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = target.parent().expect("target has parent");
    fs::create_dir_all(parent)?;
    let tmp = target.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, target)?;
    Ok(())
}

/// Probe whether the filesystem honors POSIX execute bits. We create a temp
/// file, set 0o644, stat, then 0o755, stat again. If the bits stuck the FS
/// supports filemode. On Windows / FAT the bits are coerced.
fn probe_filemode(workdir: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = workdir.join(".rustygit-probe");
        let res = (|| -> io::Result<bool> {
            let f = fs::File::create(&path)?;
            f.set_permissions(fs::Permissions::from_mode(0o644))?;
            let m1 = fs::metadata(&path)?.permissions().mode() & 0o777;
            f.set_permissions(fs::Permissions::from_mode(0o755))?;
            let m2 = fs::metadata(&path)?.permissions().mode() & 0o777;
            Ok(m1 == 0o644 && m2 == 0o755)
        })();
        let _ = fs::remove_file(&path);
        res.unwrap_or(true)
    }
    #[cfg(not(unix))]
    {
        let _ = workdir;
        false
    }
}

/// Probe whether the filesystem is case-insensitive for filenames.
fn probe_ignorecase(workdir: &Path) -> bool {
    let probe = workdir.join(".rustygit-case-probe");
    let upper = workdir.join(".RUSTYGIT-CASE-PROBE");
    if fs::write(&probe, b"x").is_err() {
        return false;
    }
    let answer = fs::metadata(&upper).is_ok();
    let _ = fs::remove_file(&probe);
    answer
}

/// git prints the absolute path to `.git`. We do the same with a couple of
/// caveats matched to git's `init-db.c` output: trailing slash on the
/// gitdir-as-displayed.
fn display_init_path(gitdir: &Path) -> String {
    let mut s = gitdir.display().to_string();
    if !s.ends_with('/') {
        s.push('/');
    }
    s
}
