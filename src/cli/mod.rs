//! Command-line dispatch (ADR A7).
//!
//! Each command's argv parsing lives in its own submodule. The `dispatch`
//! function consumes a parsed `Cli` and runs the matching command. Plumbing
//! commands (M1+) will additionally expose pure-function entry points that
//! porcelain can call without re-parsing argv.

pub mod add;
pub mod add_patch;
pub mod alias;
pub mod am;
pub mod apply;
pub mod archive;
pub mod beta;
pub mod bisect;
pub mod blame;
pub mod branch;
pub mod bug_report;
pub mod bundle;
pub mod cat_file;
pub mod check_attr;
pub mod check_ignore;
pub mod check_ref_format;
pub mod checkout;
pub mod cherry_pick;
pub mod clean;
pub mod clone;
pub mod commit;
pub mod commit_graph;
pub mod commit_tree;
pub mod completions;
pub mod config_cmd;
pub mod count_objects;
pub mod describe;
pub mod diff;
pub mod diff_files;
pub mod diff_index;
pub mod diff_tree;
pub mod doctor;
pub mod fetch;
pub mod fetch_pack;
pub mod filter_branch;
pub mod filters;
pub mod for_each_ref;
pub mod format_patch;
pub mod fsck;
pub mod gc;
pub mod git_daemon;
pub mod gitweb;
pub mod grep;
pub mod hash_object;
pub mod i18n_load;
pub mod index_pack;
pub mod init;
pub mod internals;
pub mod interpret_trailers;
pub mod lfs;
pub mod log;
pub mod ls_files;
pub mod ls_remote;
pub mod ls_tree;
pub mod mailinfo;
pub mod mailsplit;
pub mod merge;
pub mod merge_base;
pub mod merge_file;
pub mod merge_index;
pub mod merge_tree;
pub mod mktag;
pub mod mktree;
pub mod multi_pack_index;
pub mod mv;
pub mod name_rev;
pub mod notes;
pub mod pack_objects;
pub mod pack_refs;
pub mod pager;
pub mod patch_id;
pub mod prune;
pub mod prune_locks;
pub mod pull;
pub mod push;
pub mod range_diff;
pub mod read_tree;
pub mod rebase;
pub mod rebase_interactive;
pub mod recent;
pub mod reflog;
pub mod repack;
pub mod replace;
pub mod request_pull;
pub mod rerere;
pub mod reset;
pub mod restore;
pub mod rev_list;
pub mod rev_parse;
pub mod revert;
pub mod rm;
pub mod send_email;
pub mod send_pack;
pub mod server_side;
pub mod shortlog;
pub mod show;
pub mod show_branch;
pub mod show_index;
pub mod show_ref;
pub mod sparse_checkout;
pub mod stash;
pub mod status;
pub mod stripspace;
pub mod submodule;
pub mod switch;
pub mod symbolic_ref;
pub mod tag;
pub mod tools;
pub mod unpack_objects;
pub mod update_index;
pub mod update_ref;
pub mod var;
pub mod vcs_bridges;
pub mod verify_commit;
pub mod verify_pack;
pub mod verify_tag;
pub mod win_paths;
pub mod worktree;
pub mod write_tree;

use std::io;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::hash::HashKind;

/// Top-level CLI. Mirrors `git`'s shape.
#[derive(Debug, Parser)]
#[command(
    name = "rustygit",
    version,
    about = "A from-scratch Rust reimplementation of git",
    long_about = None,
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Command,
}

/// Flags accepted by every subcommand. We keep the names compatible with git.
#[derive(Debug, clap::Args)]
pub struct GlobalArgs {
    /// Run as if started in the given directory.
    #[arg(short = 'C', value_name = "PATH", global = true)]
    pub cwd: Option<PathBuf>,

    /// Pass a configuration parameter to the command. The value uses the same
    /// `<name>=<value>` form as `git -c`: e.g. `-c user.name="Alice"` or
    /// `-c color.ui=always`. Repeat the flag to set multiple. Overrides apply
    /// on top of any per-repo config; the rightmost `-c` for the same key
    /// wins. Like upstream git, `-c` is only valid BEFORE the subcommand —
    /// after the subcommand it's parsed by the subcommand itself (e.g.
    /// `switch -c <branch>`).
    #[arg(short = 'c', value_name = "KEY=VALUE", action = clap::ArgAction::Append)]
    pub config_overrides: Vec<String>,

    /// Path to the repository (sets `$GIT_DIR` for the dispatched command).
    #[arg(long = "git-dir", value_name = "PATH", global = true)]
    pub git_dir: Option<PathBuf>,

    /// Path to the working tree (sets `$GIT_WORK_TREE` for the dispatched command).
    #[arg(long = "work-tree", value_name = "PATH", global = true)]
    pub work_tree: Option<PathBuf>,

    /// Do not pipe output through the pager.
    #[arg(long = "no-pager", global = true)]
    pub no_pager: bool,

    /// Treat the repository as bare. Only meaningful to `init`; accepted
    /// elsewhere for argv compatibility.
    #[arg(long = "bare", global = true)]
    pub bare: bool,

    /// Path to the helper-command lookup directory. With no value, print
    /// rustygit's own helper directory and exit.
    #[arg(long = "exec-path", value_name = "PATH", num_args = 0..=1, default_missing_value = "")]
    pub exec_path: Option<String>,
}

// --- Exit codes ----------------------------------------------------------
//
// Centralized so call-sites use the named constant rather than a magic
// number. Matches git's published exit-code conventions.

/// Successful completion.
pub const EXIT_OK: i32 = 0;
/// `diff --exit-code` / `--quiet` found differences. Not an error — the
/// command worked as expected and merely reports that the trees diverge.
pub const EXIT_DIFF_FOUND: i32 = 1;
/// Generic command failure (anything not caught by a more specific code).
pub const EXIT_FATAL: i32 = 128;
/// Usage error (argv didn't parse, conflicting flags, etc.).
pub const EXIT_USAGE: i32 = 129;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create an empty rustygit repository or reinitialize an existing one.
    Init(init::InitArgs),
    /// Compute the object id of a file (or stdin), optionally writing it.
    HashObject(hash_object::HashObjectArgs),
    /// Print object contents/type/size given an object id.
    CatFile(cat_file::CatFileArgs),
    /// List the contents of a tree object.
    LsTree(ls_tree::LsTreeArgs),
    /// Atomically create, update, or delete a ref.
    UpdateRef(update_ref::UpdateRefArgs),
    /// List references and the objects they point to.
    ShowRef(show_ref::ShowRefArgs),
    /// Read or write a symbolic reference (e.g. HEAD).
    SymbolicRef(symbolic_ref::SymbolicRefArgs),
    /// Resolve names/expressions to object ids.
    RevParse(rev_parse::RevParseArgs),
    /// Stage paths into the index.
    Add(add::AddArgs),
    /// Write the current index to a tree object and print its oid.
    WriteTree(write_tree::WriteTreeArgs),
    /// Build a commit object from a tree, optional parents, and a message.
    CommitTree(commit_tree::CommitTreeArgs),
    /// Record the index as a new commit on the current branch.
    Commit(commit::CommitArgs),
    /// Walk the commit ancestor chain.
    Log(log::LogArgs),
    /// Show one or more objects (commits get header + diff against parent).
    Show(show::ShowArgs),
    /// Show the working tree status.
    Status(status::StatusArgs),
    /// Remove paths from the index and (optionally) the working tree.
    Rm(rm::RmArgs),
    /// Move or rename a tracked path.
    Mv(mv::MvArgs),
    /// Show changes between commits, the index, and/or the working tree.
    Diff(diff::DiffArgs),
    /// Plumbing: diff between two trees.
    DiffTree(diff_tree::DiffTreeArgs),
    /// Plumbing: diff between a tree and the index (or working tree).
    DiffIndex(diff_index::DiffIndexArgs),
    /// Plumbing: diff between the index and the working tree.
    DiffFiles(diff_files::DiffFilesArgs),
    /// List, create, delete, or rename branches.
    Branch(branch::BranchArgs),
    /// Switch branches or restore working-tree files (legacy, broad).
    Checkout(checkout::CheckoutArgs),
    /// Switch to a branch (safer alternative to `checkout`).
    Switch(switch::SwitchArgs),
    /// Restore working-tree files or staged paths from a tree.
    Restore(restore::RestoreArgs),
    /// Move HEAD (and optionally index/workdir) to a given commit.
    Reset(reset::ResetArgs),
    /// Validate a packfile and print its objects (matches `git verify-pack`).
    VerifyPack(verify_pack::VerifyPackArgs),
    /// Read a pack from stdin and explode every object into the loose store.
    UnpackObjects(unpack_objects::UnpackObjectsArgs),
    /// Clone a repository (local source only in M8).
    Clone(clone::CloneArgs),
    /// Build a pack from a list of object ids (read from stdin).
    PackObjects(pack_objects::PackObjectsArgs),
    /// Combine all reachable objects into a single new pack.
    Repack(repack::RepackArgs),
    /// Run housekeeping: consolidate packs and prune redundant loose objects.
    Gc(gc::GcArgs),
    /// List references on a remote repository.
    LsRemote(ls_remote::LsRemoteArgs),
    /// Download objects and refs from a remote.
    Fetch(fetch::FetchArgs),
    /// Fetch from a remote and merge (merge step deferred to M13).
    Pull(pull::PullArgs),
    /// Upload refs and objects to a remote.
    Push(push::PushArgs),
    /// Join two or more development histories together.
    Merge(merge::MergeArgs),
    /// Find the best common ancestor(s) of two commits.
    MergeBase(merge_base::MergeBaseArgs),
    /// Plumbing: three-way merge of two trees against a base.
    MergeTree(merge_tree::MergeTreeArgs),
    /// Apply changes from existing commits onto HEAD.
    CherryPick(cherry_pick::CherryPickArgs),
    /// Apply the inverse of existing commits onto HEAD.
    Revert(revert::RevertArgs),
    /// Create, list, or delete tags.
    Tag(tag::TagArgs),
    /// Save, list, apply, or drop work-in-progress changes.
    Stash(stash::StashArgs),
    /// Reapply commits on top of another base tip.
    Rebase(rebase::RebaseArgs),
    /// Show the reflog for a given ref (default: HEAD).
    Reflog(reflog::ReflogArgs),
    /// Write or verify the commit-graph cache.
    CommitGraph(commit_graph::CommitGraphArgs),
    /// Write or verify the multi-pack-index.
    MultiPackIndex(multi_pack_index::MultiPackIndexArgs),
    /// Show what commit + author last touched each line of a file.
    Blame(blame::BlameArgs),
    /// Binary-search commit history to find a regression.
    Bisect(bisect::BisectArgs),
    /// Verify the integrity of the object database.
    Fsck(fsck::FsckArgs),
    /// Create, list, or delete object replacements (NON_GOALS Batch E: only --list is functional).
    Replace(replace::ReplaceArgs),
    /// Reuse Recorded Resolution (NON_GOALS Batch E: stub, prints "not implemented").
    Rerere(rerere::RerereArgs),
    /// Verify the GPG signature on one or more commits.
    VerifyCommit(verify_commit::VerifyCommitArgs),
    /// Verify the PGP signature on one or more tags.
    VerifyTag(verify_tag::VerifyTagArgs),
    /// Plumbing: read a tag body from stdin and write it as a tag object.
    Mktag(mktag::MktagArgs),
    /// Plumbing: build a tree object from ls-tree-format input on stdin.
    Mktree(mktree::MktreeArgs),
    /// Compute a stable patch identity from a unified diff on stdin.
    PatchId(patch_id::PatchIdArgs),
    /// Dump the contents of a pack `.idx` read from stdin.
    ShowIndex(show_index::ShowIndexArgs),
    /// Three-way file merge plumbing (current/base/other paths).
    MergeFile(merge_file::MergeFileArgs),
    /// Consolidate loose refs into the `packed-refs` file.
    PackRefs(pack_refs::PackRefsArgs),
    /// Print object database statistics.
    CountObjects(count_objects::CountObjectsArgs),
    /// Validate a ref name against git's well-formedness rules.
    CheckRefFormat(check_ref_format::CheckRefFormatArgs),
    /// Clean whitespace in commit-message-style input on stdin.
    Stripspace(stripspace::StripspaceArgs),
    /// Print internal git variables.
    Var(var::VarArgs),
    /// Group commits by author and print a summary.
    Shortlog(shortlog::ShortlogArgs),
    /// Find a symbolic name for one or more oids.
    NameRev(name_rev::NameRevArgs),
    /// Show a multi-branch ASCII matrix of commit reachability.
    ShowBranch(show_branch::ShowBranchArgs),
    /// List refs with a `--format=` template.
    ForEachRef(for_each_ref::ForEachRefArgs),
    /// List files known to the index.
    LsFiles(ls_files::LsFilesArgs),
    /// Explain whether and why each path would be gitignored.
    CheckIgnore(check_ignore::CheckIgnoreArgs),
    /// Print effective gitattributes values for given paths.
    CheckAttr(check_attr::CheckAttrArgs),
    /// Remove untracked files from the working tree.
    Clean(clean::CleanArgs),
    /// Describe a commit as "tag-N-gabc" relative to the nearest tag.
    Describe(describe::DescribeArgs),
    /// Read/write/list config values.
    Config(config_cmd::ConfigArgs),
    /// Create a tar archive of a tree-ish.
    Archive(archive::ArchiveArgs),
    /// List commit oids reachable from given starts.
    RevList(rev_list::RevListArgs),
    /// Plumbing: replace the index from a tree-ish.
    ReadTree(read_tree::ReadTreeArgs),
    /// Plumbing: low-level index manipulation.
    UpdateIndex(update_index::UpdateIndexArgs),
    /// Remove unreachable loose objects.
    Prune(prune::PruneArgs),
    /// Remove loose objects already present in a pack.
    PrunePacked(prune::PrunePackedArgs),
    /// Diff two commit ranges, matching by patch-id.
    RangeDiff(range_diff::RangeDiffArgs),
    /// Create/verify/list-heads/unbundle offline bundles.
    Bundle(bundle::BundleArgs),
    /// Search tracked file content for a pattern.
    Grep(grep::GrepArgs),
    /// Add/replace/remove RFC2822-style trailers in commit messages.
    InterpretTrailers(interpret_trailers::InterpretTrailersArgs),
    /// Build a .idx file for an existing .pack.
    IndexPack(index_pack::IndexPackArgs),
    /// Low-level fetch (delegates to fetch).
    FetchPack(fetch_pack::FetchPackArgs),
    /// Low-level push (delegates to push).
    SendPack(send_pack::SendPackArgs),
    /// Enumerate unmerged index entries.
    MergeIndex(merge_index::MergeIndexArgs),
    /// Apply a unified diff to the working tree.
    Apply(apply::ApplyArgs),
    /// Apply a series of mailbox patches.
    Am(am::AmArgs),
    /// Extract author/subject/body from a single mail message on stdin.
    Mailinfo(mailinfo::MailinfoArgs),
    /// Split an mbox into one message per file.
    Mailsplit(mailsplit::MailsplitArgs),
    /// Emit commits as mail-formatted patches.
    FormatPatch(format_patch::FormatPatchArgs),
    /// Pipe patch files to sendmail.
    SendEmail(send_email::SendEmailArgs),
    /// Emit a "please pull from X" email body.
    RequestPull(request_pull::RequestPullArgs),
    /// Spawn the configured 3-way merge tool per conflicted file.
    Mergetool(tools::MergetoolArgs),
    /// Spawn the configured diff tool per modified file.
    Difftool(tools::DifftoolArgs),
    /// Manage sparse-checkout patterns.
    SparseCheckout(sparse_checkout::SparseCheckoutArgs),
    /// Submodule porcelain.
    Submodule(submodule::SubmoduleArgs),
    /// Interactive rebase.
    RebaseInteractive(rebase_interactive::RebaseInteractiveArgs),
    /// History rewriter.
    FilterBranch(filter_branch::FilterBranchArgs),
    /// Emit a static-HTML repo viewer.
    Gitweb(gitweb::GitwebArgs),
    /// Text-mode equivalent of gitk.
    Gitk(gitweb::GitkArgs),
    /// Text-mode equivalent of git-gui.
    GitGui(gitweb::GitGuiArgs),
    /// Serve gitweb via local file output.
    Instaweb(gitweb::InstawebArgs),
    /// Subversion bridge.
    Svn(vcs_bridges::SvnArgs),
    /// Perforce bridge.
    P4(vcs_bridges::P4Args),
    /// LFS client.
    Lfs(lfs::LfsArgs),
    /// Update info/refs + objects/info/packs (for dumb-HTTP serving).
    UpdateServerInfo(server_side::UpdateServerInfoArgs),
    /// Server-side fetch endpoint.
    UploadPack(server_side::UploadPackArgs),
    /// Server-side push endpoint.
    ReceivePack(server_side::ReceivePackArgs),
    /// Server-side archive endpoint.
    UploadArchive(server_side::UploadArchiveArgs),
    /// `git://` daemon.
    Daemon(git_daemon::DaemonArgs),
    /// Long-running credential cache.
    CredentialCacheDaemon(internals::CredentialCacheDaemonArgs),
    /// File-system event watcher daemon.
    FsmonitorDaemon(internals::FsmonitorDaemonArgs),
    /// Parallel-checkout helper.
    CheckoutWorker(internals::CheckoutWorkerArgs),
    /// Submodule porcelain helper.
    SubmoduleHelper(internals::SubmoduleHelperArgs),
    /// Remote helper via external command.
    RemoteExt(internals::RemoteExtArgs),
    /// Remote helper via file descriptors.
    RemoteFd(internals::RemoteFdArgs),
    /// Re-fetch objects a filtered clone is missing.
    Backfill(recent::BackfillArgs),
    /// Emit a diagnostics report.
    Diagnose(recent::DiagnoseArgs),
    /// Print a bug-report skeleton.
    Bugreport(recent::BugreportArgs),
    /// Emit per-pair diff entries from stdin.
    DiffPairs(recent::DiffPairsArgs),
    /// Show repository history overview.
    History(recent::HistoryArgs),
    /// Print last-modifying commit per indexed path.
    LastModified(recent::LastModifiedArgs),
    /// Multi-purpose ref maintenance.
    Refs(recent::RefsArgs),
    /// Repository metadata.
    Repo(recent::RepoArgs),
    /// Replay commits onto a base.
    Replay(recent::ReplayArgs),
    /// Run a command across every configured repo.
    ForEachRepo(recent::ForEachRepoArgs),
    /// List or run client-side hooks.
    Hook(recent::HookArgs),
    /// Add, inspect, or remove notes attached to objects.
    Notes(notes::NotesArgs),
    /// Manage linked worktrees (NON_GOALS Batch I).
    Worktree(worktree::WorktreeArgs),
    /// Remove stale `*.lock` files and orphan `checkout.tmp.*/` shadow dirs
    /// left behind by a crashed earlier rustygit/git process.
    PruneLocks(prune_locks::PruneLocksArgs),
    /// Run a repo health check (stale locks, orphan shadows, HEAD
    /// resolvability, index version).
    Doctor(doctor::DoctorArgs),
    /// Print an environment bundle suitable for filing a bug report.
    /// Includes version, OS, `git --version`, `rustygit doctor` output,
    /// safe env vars, and the last 10 subcommand names from the opt-in
    /// history log. All output is passed through a secrets-redaction
    /// filter before printing.
    BugReport(bug_report::BugReportArgs),
    /// Generate a shell-completion script for the given shell. Hidden from
    /// `--help`; the release workflow invokes this to populate
    /// `/usr/share/bash-completion/completions/rustygit` and friends.
    /// See `src/cli/completions.rs` for output paths. (NON_GOALS B4.)
    #[command(hide = true)]
    Completions(completions::CompletionsArgs),
    /// Generate a troff(1) man page for `rustygit(1)` on stdout. Hidden
    /// from `--help`; the release workflow gzips the output into
    /// `/usr/share/man/man1/rustygit.1.gz`. (NON_GOALS B4.)
    #[command(hide = true)]
    Manpage(completions::ManpageArgs),
}

/// Subcommands the plan explicitly drops from rustygit's scope. Return a
/// useful, named explanation if `name` matches one — `None` for anything
/// else (clap will then parse normally and reject unknown names generically).
///
/// We intercept these BEFORE clap so the user learns *why* `rustygit gitweb`
/// fails instead of getting "unrecognized subcommand 'gitweb'".
pub fn explain_unsupported_subcommand(_name: &str) -> Option<String> {
    // All previously-rejected names are now wired as real subcommands.
    // Keeping the shape of this function so other call-sites stay
    // unchanged.
    None
}

/// Run a parsed CLI. Returns the exit code that should be propagated to the OS.
///
/// Conventions (matching git): 0 on success, 1 on "expected" failure (e.g.
/// `diff --exit-code` finding differences), 128 on fatal error, 129 on usage.
pub fn dispatch(cli: Cli) -> io::Result<i32> {
    if let Some(cwd) = &cli.global.cwd {
        std::env::set_current_dir(cwd)?;
    }
    // `--git-dir` / `--work-tree` are wired by setting the env vars the same
    // way upstream git's discovery reads them. `Repository::discover_from_cwd`
    // honors them (see A6); for the rest of the dispatch path we let any
    // child process inherit the values too.
    if let Some(p) = &cli.global.git_dir {
        // SAFETY: process is single-threaded at this point; CLI dispatch has
        // not spawned any worker threads yet.
        std::env::set_var("GIT_DIR", p);
    }
    if let Some(p) = &cli.global.work_tree {
        std::env::set_var("GIT_WORK_TREE", p);
    }
    if cli.global.no_pager {
        // Disable pagination across the whole dispatch by forcing the
        // pager-selection helpers to short-circuit. `cat` is the documented
        // sentinel and is honored by `pager::open`.
        std::env::set_var("GIT_PAGER", "cat");
    }
    if let Some(ep) = &cli.global.exec_path {
        if ep.is_empty() {
            // Bare `--exec-path`: print our exec-path equivalent and exit.
            // We don't have an installed libexec dir, but the directory of
            // the running binary is the closest analogue.
            let exe = std::env::current_exe()?;
            let dir = exe.parent().unwrap_or_else(|| std::path::Path::new("."));
            println!("{}", dir.display());
            return Ok(EXIT_OK);
        }
        std::env::set_var("GIT_EXEC_PATH", ep);
    }
    // `--bare` is meaningful to `init` only; it lives in `init::InitArgs`
    // separately. We accept it globally for argv compatibility with users
    // (and scripts) who pass `rustygit --bare init ...`; it's a no-op here.
    let _ = cli.global.bare;
    // `-c key=value` overrides: split each into (key, value) and install on
    // the process-wide layer that `Config::from_repo_dir` consults. A bare
    // `-c key` (no `=`) is treated as `key=true`, matching git.
    if !cli.global.config_overrides.is_empty() {
        let parsed: Vec<(String, String)> = cli
            .global
            .config_overrides
            .iter()
            .map(|raw| match raw.split_once('=') {
                Some((k, v)) => (k.to_string(), v.to_string()),
                None => (raw.clone(), "true".to_string()),
            })
            .collect();
        crate::config::set_cli_overrides(parsed);
    }
    match cli.command {
        Command::Init(args) => init::run(args),
        Command::HashObject(args) => hash_object::run(args),
        Command::CatFile(args) => cat_file::run(args),
        Command::LsTree(args) => ls_tree::run(args),
        Command::UpdateRef(args) => update_ref::run(args),
        Command::ShowRef(args) => show_ref::run(args),
        Command::SymbolicRef(args) => symbolic_ref::run(args),
        Command::RevParse(args) => rev_parse::run(args),
        Command::Add(args) => add::run(args),
        Command::WriteTree(args) => write_tree::run(args),
        Command::CommitTree(args) => commit_tree::run(args),
        Command::Commit(args) => commit::run(args),
        Command::Log(args) => log::run(args),
        Command::Show(args) => show::run(args),
        Command::Status(args) => status::run(args),
        Command::Rm(args) => rm::run(args),
        Command::Mv(args) => mv::run(args),
        Command::Diff(args) => diff::run(args),
        Command::DiffTree(args) => diff_tree::run(args),
        Command::DiffIndex(args) => diff_index::run(args),
        Command::DiffFiles(args) => diff_files::run(args),
        Command::Branch(args) => branch::run(args),
        Command::Checkout(args) => checkout::run(args),
        Command::Switch(args) => switch::run(args),
        Command::Restore(args) => restore::run(args),
        Command::Reset(args) => reset::run(args),
        Command::VerifyPack(args) => verify_pack::run(args),
        Command::UnpackObjects(args) => unpack_objects::run(args),
        Command::Clone(args) => clone::run(args),
        Command::PackObjects(args) => pack_objects::run(args),
        Command::Repack(args) => repack::run(args),
        Command::Gc(args) => gc::run(args),
        Command::LsRemote(args) => ls_remote::run(args),
        Command::Fetch(args) => fetch::run(args),
        Command::Pull(args) => pull::run(args),
        Command::Push(args) => push::run(args),
        Command::Merge(args) => merge::run(args),
        Command::MergeBase(args) => merge_base::run(args),
        Command::MergeTree(args) => merge_tree::run(args),
        Command::CherryPick(args) => cherry_pick::run(args),
        Command::Revert(args) => revert::run(args),
        Command::Tag(args) => tag::run(args),
        Command::Stash(args) => stash::run(args),
        Command::Rebase(args) => rebase::run(args),
        Command::Reflog(args) => reflog::run(args),
        Command::CommitGraph(args) => commit_graph::run(args),
        Command::MultiPackIndex(args) => multi_pack_index::run(args),
        Command::Blame(args) => blame::run(args),
        Command::Bisect(args) => bisect::run(args),
        Command::Fsck(args) => fsck::run(args),
        Command::Replace(args) => replace::run(args),
        Command::Rerere(args) => rerere::run(args),
        Command::VerifyCommit(args) => verify_commit::run(args),
        Command::VerifyTag(args) => verify_tag::run(args),
        Command::Mktag(args) => mktag::run(args),
        Command::Mktree(args) => mktree::run(args),
        Command::PatchId(args) => patch_id::run(args),
        Command::ShowIndex(args) => show_index::run(args),
        Command::MergeFile(args) => merge_file::run(args),
        Command::PackRefs(args) => pack_refs::run(args),
        Command::CountObjects(args) => count_objects::run(args),
        Command::CheckRefFormat(args) => check_ref_format::run(args),
        Command::Stripspace(args) => stripspace::run(args),
        Command::Var(args) => var::run(args),
        Command::Shortlog(args) => shortlog::run(args),
        Command::NameRev(args) => name_rev::run(args),
        Command::ShowBranch(args) => show_branch::run(args),
        Command::ForEachRef(args) => for_each_ref::run(args),
        Command::LsFiles(args) => ls_files::run(args),
        Command::CheckIgnore(args) => check_ignore::run(args),
        Command::CheckAttr(args) => check_attr::run(args),
        Command::Clean(args) => clean::run(args),
        Command::Describe(args) => describe::run(args),
        Command::Config(args) => config_cmd::run(args),
        Command::Archive(args) => archive::run(args),
        Command::RevList(args) => rev_list::run(args),
        Command::ReadTree(args) => read_tree::run(args),
        Command::UpdateIndex(args) => update_index::run(args),
        Command::Prune(args) => prune::run(args),
        Command::PrunePacked(args) => prune::run_prune_packed(args),
        Command::RangeDiff(args) => range_diff::run(args),
        Command::Bundle(args) => bundle::run(args),
        Command::Grep(args) => grep::run(args),
        Command::InterpretTrailers(args) => interpret_trailers::run(args),
        Command::IndexPack(args) => index_pack::run(args),
        Command::FetchPack(args) => fetch_pack::run(args),
        Command::SendPack(args) => send_pack::run(args),
        Command::MergeIndex(args) => merge_index::run(args),
        Command::Apply(args) => apply::run(args),
        Command::Am(args) => am::run(args),
        Command::Mailinfo(args) => mailinfo::run(args),
        Command::Mailsplit(args) => mailsplit::run(args),
        Command::FormatPatch(args) => format_patch::run(args),
        Command::SendEmail(args) => send_email::run(args),
        Command::RequestPull(args) => request_pull::run(args),
        Command::Mergetool(args) => tools::run_mergetool(args),
        Command::Difftool(args) => tools::run_difftool(args),
        Command::SparseCheckout(args) => sparse_checkout::run(args),
        Command::Submodule(args) => submodule::run(args),
        Command::RebaseInteractive(args) => rebase_interactive::run(args),
        Command::FilterBranch(args) => filter_branch::run(args),
        Command::Gitweb(args) => gitweb::run(args),
        Command::Gitk(args) => gitweb::run_gitk(args),
        Command::GitGui(args) => gitweb::run_git_gui(args),
        Command::Instaweb(args) => gitweb::run_instaweb(args),
        Command::Svn(args) => vcs_bridges::run_svn(args),
        Command::P4(args) => vcs_bridges::run_p4(args),
        Command::Lfs(args) => lfs::run(args),
        Command::UpdateServerInfo(args) => server_side::run_update_server_info(args),
        Command::UploadPack(args) => server_side::run_upload_pack(args),
        Command::ReceivePack(args) => server_side::run_receive_pack(args),
        Command::UploadArchive(args) => server_side::run_upload_archive(args),
        Command::Daemon(args) => git_daemon::run(args),
        Command::CredentialCacheDaemon(args) => internals::run_credential_cache_daemon(args),
        Command::FsmonitorDaemon(args) => internals::run_fsmonitor_daemon(args),
        Command::CheckoutWorker(args) => internals::run_checkout_worker(args),
        Command::SubmoduleHelper(args) => internals::run_submodule_helper(args),
        Command::RemoteExt(args) => internals::run_remote_ext(args),
        Command::RemoteFd(args) => internals::run_remote_fd(args),
        Command::Backfill(args) => recent::run_backfill(args),
        Command::Diagnose(args) => recent::run_diagnose(args),
        Command::Bugreport(args) => recent::run_bugreport(args),
        Command::DiffPairs(args) => recent::run_diff_pairs(args),
        Command::History(args) => recent::run_history(args),
        Command::LastModified(args) => recent::run_last_modified(args),
        Command::Refs(args) => recent::run_refs(args),
        Command::Repo(args) => recent::run_repo(args),
        Command::Replay(args) => recent::run_replay(args),
        Command::ForEachRepo(args) => recent::run_for_each_repo(args),
        Command::Hook(args) => recent::run_hook(args),
        Command::Notes(args) => notes::run(args),
        Command::Worktree(args) => worktree::run(args),
        Command::PruneLocks(args) => prune_locks::run(args),
        Command::Doctor(args) => doctor::run(args),
        Command::BugReport(args) => bug_report::run(args),
        Command::Completions(args) => completions::run_completions(args),
        Command::Manpage(args) => completions::run_manpage(args),
    }
}

/// Default initial branch name when neither `--initial-branch` nor
/// `init.defaultBranch` is specified. We follow git's compiled default.
pub const DEFAULT_INITIAL_BRANCH: &str = "master";

/// Helpers shared across commands. Everything here is pub(crate) so we can
/// expose just what the binary entry point needs.
pub(crate) fn parse_hash_kind(s: &str) -> Result<HashKind, String> {
    HashKind::parse(s).map_err(|e| e.to_string())
}
