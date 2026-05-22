// data.js — rustygit site data.
// Single source of truth; imported by the pages that need it.

export const COMPAT_ROWS = [
  // --- core porcelain (T1 unless noted) ---
  { cmd: "init",           tier: "T1",  notes: "byte-equal HEAD, hooks/, info/exclude, sample hooks layout" },
  { cmd: "clone",          tier: "T2",  notes: "v2 over HTTPS only; partial-clone filters silently ignored — full clone is performed" },
  { cmd: "fetch",          tier: "T1",  notes: "v2 over HTTPS; refspec semantics and FETCH_HEAD format match git" },
  { cmd: "pull",           tier: "T1",  notes: "merge and rebase strategies both supported (non-interactive rebase only)" },
  { cmd: "push",           tier: "T1",  notes: "atomic, force-with-lease, signed-push semantics all match" },
  { cmd: "add",            tier: "T1",  notes: "pathspec, -p, -N, --renormalize, --intent-to-add" },
  { cmd: "rm",             tier: "T1",  notes: "" },
  { cmd: "mv",             tier: "T1",  notes: "" },
  { cmd: "commit",         tier: "T1",  notes: "trailers, --amend, --fixup, --squash, --no-verify, GPG signing" },
  { cmd: "status",         tier: "T2",  notes: "no ANSI colour codes today; --porcelain output is byte-equal; submodule typechanges listed (git skips them)" },
  { cmd: "log",            tier: "T2",  notes: "format strings, --graph, --follow all match; no ANSI colour today" },
  { cmd: "show",           tier: "T2",  notes: "no ANSI colour codes today; otherwise byte-equal" },
  { cmd: "diff",           tier: "T2",  notes: "no ANSI colour codes today; --color-moved not honoured" },
  { cmd: "branch",         tier: "T1",  notes: "" },
  { cmd: "checkout",       tier: "T2",  notes: "refuses symlink writes; non-UTF-8 paths refused" },
  { cmd: "switch",         tier: "T1",  notes: "" },
  { cmd: "restore",        tier: "T1",  notes: "" },
  { cmd: "reset",          tier: "T1",  notes: "" },
  { cmd: "merge",          tier: "T1",  notes: "ort strategy default; --squash, --no-ff, --ff-only honoured" },
  { cmd: "rebase",         tier: "T2",  notes: "non-interactive only; -i / --autosquash / --rebase-merges / --exec refused with named error" },
  { cmd: "cherry-pick",    tier: "T1",  notes: "" },
  { cmd: "revert",         tier: "T1",  notes: "" },
  { cmd: "tag",            tier: "T1",  notes: "annotated and signed tags both supported" },
  { cmd: "stash",          tier: "T1",  notes: "push, pop, apply, drop, list, show, branch, clear" },
  { cmd: "notes",          tier: "T1",  notes: "" },
  { cmd: "worktree",       tier: "T1",  notes: "add, list, lock, unlock, move, prune, remove, repair" },
  { cmd: "bisect",         tier: "T1",  notes: "" },
  { cmd: "blame",          tier: "T2",  notes: "byte-equal output for -p; no ANSI colour" },
  { cmd: "grep",           tier: "T2",  notes: "no ANSI colour; -n -l -c -e -i -P -E all honoured" },
  { cmd: "clean",          tier: "T1",  notes: "" },
  { cmd: "describe",       tier: "T1",  notes: "" },
  { cmd: "archive",        tier: "T1",  notes: "tar and zip; pax extended-header format matches" },
  { cmd: "bundle",         tier: "T1",  notes: "create, verify, list-heads, unbundle" },
  { cmd: "shortlog",       tier: "T1",  notes: "" },
  { cmd: "name-rev",       tier: "T1",  notes: "" },
  { cmd: "show-branch",    tier: "T1",  notes: "" },
  { cmd: "range-diff",     tier: "T2",  notes: "no ANSI colour; structural output is byte-equal" },
  { cmd: "config",         tier: "T1",  notes: "[includeIf]/[include] silently skipped — see /watch-out" },
  { cmd: "remote",         tier: "T1",  notes: "" },
  { cmd: "ls-remote",      tier: "T1",  notes: "" },
  { cmd: "ls-files",       tier: "T1",  notes: "" },
  { cmd: "ls-tree",        tier: "T1",  notes: "" },
  { cmd: "help",           tier: "T2",  notes: "subcommand list reflects rustygit's scope, not git's full inventory" },
  { cmd: "version",        tier: "T2",  notes: "reports rustygit's version line; --build-options identifies the Rust toolchain" },
  { cmd: "gc",             tier: "T1",  notes: "" },
  { cmd: "fsck",           tier: "T1",  notes: "--full implemented; reachability rules byte-equal" },
  { cmd: "prune",          tier: "T1",  notes: "" },
  { cmd: "prune-packed",   tier: "T1",  notes: "" },
  { cmd: "repack",         tier: "T1",  notes: "delta/window/depth flags honoured; -a -A -d match" },
  { cmd: "pack-refs",      tier: "T1",  notes: "" },
  { cmd: "commit-graph",   tier: "T1",  notes: "write/verify; bloom filters parity" },
  { cmd: "multi-pack-index", tier: "T1", notes: "write/verify/expire/repack" },
  { cmd: "reflog",         tier: "T1",  notes: "expire honours gc.reflogExpire and gc.reflogExpireUnreachable" },

  // --- plumbing (T1 unless noted) ---
  { cmd: "hash-object",    tier: "T1",  notes: "" },
  { cmd: "cat-file",       tier: "T1",  notes: "-t, -s, -p, -e, --batch, --batch-check" },
  { cmd: "write-tree",     tier: "T1",  notes: "" },
  { cmd: "commit-tree",    tier: "T1",  notes: "" },
  { cmd: "rev-parse",      tier: "T1",  notes: "" },
  { cmd: "rev-list",       tier: "T1",  notes: "" },
  { cmd: "pack-objects",   tier: "T1",  notes: "" },
  { cmd: "unpack-objects", tier: "T1",  notes: "" },
  { cmd: "index-pack",     tier: "T2",  notes: "stub: enough to satisfy fetch/clone callers; --stdin --fix-thin --keep accepted" },
  { cmd: "verify-pack",    tier: "T1",  notes: "" },
  { cmd: "update-ref",     tier: "T1",  notes: "transactional stdin protocol supported" },
  { cmd: "read-tree",      tier: "T1",  notes: "" },
  { cmd: "merge-tree",     tier: "T1",  notes: "" },
  { cmd: "merge-file",     tier: "T1",  notes: "" },
  { cmd: "merge-base",     tier: "T1",  notes: "" },
  { cmd: "diff-tree",      tier: "T1",  notes: "" },
  { cmd: "diff-index",     tier: "T1",  notes: "" },
  { cmd: "diff-files",     tier: "T1",  notes: "" },
  { cmd: "mktree",         tier: "T1",  notes: "" },
  { cmd: "mktag",          tier: "T1",  notes: "" },
  { cmd: "patch-id",       tier: "T1",  notes: "" },
  { cmd: "show-index",     tier: "T1",  notes: "" },
  { cmd: "for-each-ref",   tier: "T1",  notes: "format strings byte-equal" },
  { cmd: "check-ref-format", tier: "T1", notes: "" },
  { cmd: "check-ignore",   tier: "T1",  notes: "" },
  { cmd: "check-attr",     tier: "T1",  notes: ".gitattributes is parsed; filter/diff/merge attribute drivers are not run — see /watch-out" },
  { cmd: "interpret-trailers", tier: "T1", notes: "" },

  // --- rustygit-specific (T3) ---
  { cmd: "doctor",         tier: "T3",  notes: "rustygit-specific: reports which keys in your gitconfig rustygit honours, ignores, or refuses" },
  { cmd: "prune-locks",    tier: "T3",  notes: "rustygit-specific: clears stale .lock files left by killed processes" },
  { cmd: "bug-report",     tier: "T3",  notes: "rustygit-specific: emits a paste-ready report for the issue tracker" },

  // --- refused-by-design (still listed; OUT means we will not ship it) ---
  { cmd: "replace",        tier: "T2",  notes: "--list works; --delete / --edit / --graft / positional create exit 128 with a named message" },

  // --- OUT of scope ---
  { cmd: "submodule",      tier: "OUT", notes: "add/update/foreach not implemented; repos containing submodules clone and check out fine" },
  { cmd: "sparse-checkout", tier: "OUT", notes: "cone and non-cone both unimplemented" },
  { cmd: "rerere",         tier: "OUT", notes: "every form exits 128 with 'not implemented'" },
  { cmd: "filter-branch",  tier: "OUT", notes: "use git-filter-repo on upstream git instead" },
  { cmd: "lfs",            tier: "OUT", notes: ".gitattributes filters are not run; LFS repos must stay on upstream git" },
  { cmd: "p4",             tier: "OUT", notes: "Perforce bridge — Perl/Python; out of scope" },
  { cmd: "svn",            tier: "OUT", notes: "Subversion bridge — Perl; out of scope" },
  { cmd: "send-email",     tier: "OUT", notes: "Perl; use git on a machine that has it" },
  { cmd: "request-pull",   tier: "OUT", notes: "shell; out of scope" },
  { cmd: "instaweb",       tier: "OUT", notes: "" },
  { cmd: "gitweb",         tier: "OUT", notes: "Perl CGI; not bundled — try tig or gitui" },
  { cmd: "gitk",           tier: "OUT", notes: "Tcl/Tk GUI; not bundled — try tig, lazygit, gitui" },
  { cmd: "gui",            tier: "OUT", notes: "Tcl/Tk GUI; not bundled" },
  { cmd: "cvsimport",      tier: "OUT", notes: "" },
  { cmd: "cvsserver",      tier: "OUT", notes: "" },
  { cmd: "daemon",         tier: "OUT", notes: "rustygit doesn't run as a server" },
  { cmd: "http-backend",   tier: "OUT", notes: "rustygit doesn't run as a server" },
  { cmd: "credential-cache", tier: "OUT", notes: "use the OS keychain helpers from upstream git or a third-party tool" },
  { cmd: "credential-store", tier: "OUT", notes: "plaintext credential store — refused by design" },
];

export const HOME_PLUMBING = [
  "hash-object", "cat-file", "write-tree", "commit-tree", "rev-parse",
  "rev-list", "pack-objects", "unpack-objects", "index-pack (stub)",
  "verify-pack", "update-ref", "read-tree", "merge-tree", "merge-file",
  "merge-base", "diff-tree", "diff-index", "diff-files", "mktree", "mktag",
  "patch-id", "show-index", "for-each-ref", "check-ref-format",
  "check-ignore", "check-attr", "interpret-trailers",
];

export const HOME_PORCELAIN = [
  "init", "clone", "fetch", "pull", "push", "add", "rm", "mv", "commit",
  "status", "log", "show", "diff", "branch", "checkout", "switch",
  "restore", "reset", "merge", "rebase (non-interactive)", "cherry-pick",
  "revert", "tag (incl. signed)", "stash", "notes", "worktree", "bisect",
  "blame", "grep", "clean", "describe", "archive", "bundle", "shortlog",
  "name-rev", "show-branch", "range-diff",
];

export const HOME_MAINT = [
  "gc", "fsck", "prune", "prune-packed", "repack", "pack-refs",
  "commit-graph", "multi-pack-index", "reflog", "doctor", "prune-locks",
];

export const OUTPUT_DIVERGENCES = [
  {
    title: "ANSI colour codes are not emitted",
    body: "rustygit ignores color.ui and --color. Output is byte-equal to git --no-color. If you grep by appearance you'll see a diff; by exit code or --porcelain you won't.",
  },
  {
    title: "Dates are ASCII English regardless of LC_ALL",
    body: "rustygit emits dates like Mon Jan 1 00:00:00 2026 +0000 on every machine. LC_ALL / LANG do not affect output.",
  },
  {
    title: "status --porcelain lists submodule typechange entries",
    body: "Upstream git silently skips these. rustygit lists them with a T entry. Identical for ordinary files; only submodule rows differ.",
  },
];

export const OUT_OF_SCOPE = [
  "Submodule management (add / update / foreach). Repos containing submodules still clone and check out.",
  "Sparse-checkout (cone and non-cone).",
  ".gitattributes filter / clean / smudge / textconv drivers — and therefore Git LFS.",
  "Interactive rebase (-i, --autosquash, --rebase-merges, --exec).",
  "Partial clone (--filter=blob:none, promisor remotes). rustygit silently does a full clone.",
  "Old transports: git://, ftp://, ftps://, rsync://. Protocol v0/v1. Only v2-over-HTTPS is supported.",
  "Perl-based subcommands: send-email, p4, svn, cvsimport, cvsserver, request-pull.",
  "Tcl/Tk GUIs: gitk, git gui.",
  "gitweb, instaweb (Perl CGI).",
  "Server mode: daemon, http-backend.",
  "Mutating git replace (--delete, --edit, --graft, positional create).",
  "rerere (conflict-resolution database).",
];

// Build metadata — surfaced in the footer and the hero status pill.
// Update these in lockstep with the rustygit release.
export const BUILD_META = {
  version: "v0.1.0-beta.1",
  testsPassing: 941,
  testsTotal: 941,
  updated: "2026-05-19",
  sha: "4f1c8a2",
  binarySize: "~6 MB stripped",
  oracleAgainst: "git 2.45.2",
  repoUrl: "https://github.com/bsadashi/rustygit",
  issuesUrl: "https://github.com/bsadashi/rustygit/issues",
  securityUrl: "https://github.com/bsadashi/rustygit/security/advisories/new",
  roadmapUrl: "https://github.com/bsadashi/rustygit/blob/main/ROADMAP.md",
};
