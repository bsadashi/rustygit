# Roadmap — planned work beyond v0.1.0

v0.1.0 ships the porcelain in [`COMPAT.md`](COMPAT.md) at tier T1 — every
command in that table is byte-for-byte compatible with upstream `git`.
This document enumerates everything *else* upstream git has, with each
item classified as:

- **§A — Planned, prioritized**: missing-but-promised work that should
  land in v0.2 / v0.3.
- **§B — Planned, lower priority**: porcelain/plumbing not yet shipped
  and not explicitly out of scope.
- **§C — Sub-feature gaps within shipped commands**: things rustygit
  *partially* implements; listed flag-by-flag.
- **§D — Out of scope (see [`NON_GOALS.md`](NON_GOALS.md))**: features
  rustygit will not implement.

The source of truth for the audit was the `builtin/` directory of
upstream git 2.54.0.

---

## §A — Planned, prioritized — ✅ SHIPPED in v0.2

All three landed at T1 in COMPAT.md with full oracle-comparison test
coverage. See those rows for the exact subsets shipped.

- ✅ `tag` (lightweight + annotated + `-s` signed; `-d`, `-l`, `-f`)
- ✅ `mktag` (plumbing — oid byte-matches `git mktag`)
- ✅ `verify-tag` (GPG verify; exit 0/1/128 like git)
- ✅ `revert` (single, multi-arg, range `A..B`, `--mainline N` for merges, `--continue`/`--abort`)
- ✅ `stash` (`push`/`pop`/`apply`/`list`/`show`/`drop`/`clear` with `-u`/`--include-untracked` 3-parent shape)

Deferred sub-features (now in §C): tag editor flow when `-a` given
without `-m`; stash `--keep-index`, `--patch`, `stash branch`.

---

## §B — ✅ SHIPPED in v0.3 (all 31 items)

Every porcelain and plumbing subcommand in this section now lands at T1
in COMPAT.md. See those rows for the documented subsets. `index-pack`
ships as a stub pointing at `repack` (full implementation requires
delta-aware idx writing, deferred).

(Original list moved to the §B-Shipped appendix at the bottom of this
file.)

## §B-Shipped — original backlog (kept for reference)

### Porcelain not yet implemented

| Subcommand | Why it matters | Approx. effort |
|---|---|---|
| `clean` | Remove untracked files. Pairs with `status` / `add`. | 1 session |
| `grep` | Search through history. Big quality-of-life win. | 2 sessions |
| `archive` | tar/zip from a tree. Used by release tooling. | 1 session |
| `describe` | `vX.Y.Z-N-gabc` naming from tags. Release-tagging workflow. | 1 session |
| `bundle` | Create/extract offline bundles. Needed for air-gapped transfers. | 1-2 sessions |
| `shortlog` | Group `log` by author. Changelog generator. | < 1 session |
| `range-diff` | Diff two commit ranges. PR-review tool. | 1 session |
| `show-branch` | Branch tips + shared history visualization. | < 1 session |
| `for-each-ref` | List refs with `--format=` template. Common scripting primitive. | 1 session |
| `pack-refs` | Consolidate loose refs into `packed-refs`. Maintenance. | < 1 session |
| `count-objects` | Repo size / loose-object stats. | < 1 session |
| `name-rev` | Find a symbolic name for an oid (inverse of `rev-parse`). | < 1 session |

### Plumbing not yet implemented

| Subcommand | Why it matters | Approx. effort |
|---|---|---|
| `config` (CLI subcommand) | `git config user.email …`. We *read* config; the CLI surface to write isn't exposed. | 1 session |
| `ls-files` | List cached/staged files. Heavily used in scripts. | 1 session |
| `rev-list` | Standalone commit walker with `--all`, `--reverse`, `--count`, `--objects`. | 1-2 sessions |
| `read-tree` | Populate the index from a tree. Used by `merge`, `checkout` plumbing. | 1 session |
| `update-index` | Full `--add`, `--remove`, `--cacheinfo` flag surface. Partial today via `add`. | 1 session |
| `mktree` | Build a tree from `ls-tree`-format stdin. | < 1 session |
| `merge-file` | Three-way file merge CLI. Library at `src/merge/file.rs` already implements this; only the wrapper is missing. | < 1 session |
| `merge-index` | Run a merge driver across index entries. | 1 session |
| `index-pack` | Build `.idx` from a streamed `.pack`. (Today's `pack-objects` produces pairs.) | 1-2 sessions |
| `show-index` | Dump `.idx` contents. | < 1 session |
| `patch-id` | Stable patch identity (cherry-pick equivalence detection). | < 1 session |
| `check-attr` | Attribute lookup for paths. Useful even though smudge/clean filters are NON_GOAL. | 1 session |
| `check-ignore` | Explain which `.gitignore` rule matches a path. | < 1 session |
| `check-ref-format` | Validate ref names. | < 1 session |
| `var` | Print internal git variables (`GIT_EDITOR`, `GIT_AUTHOR_IDENT`). | < 1 session |
| `stripspace` | Clean whitespace per git's commit-message rules. | < 1 session |
| `interpret-trailers` | Read/edit commit-message trailers. | 1 session |
| `prune` / `prune-packed` | Orphan / loose-object cleanup. Today's `gc` handles part. | 1 session |
| `fetch-pack` / `send-pack` | Lower-level than `fetch`/`push`. Scripted CI flows sometimes need them. | 1 session |

---

## §C — ✅ SHIPPED in v0.3 (all 12 items)

- ✅ `commit` editor flow (`-e`/no-`m`/`-F`)
- ✅ `log` `-p`/`--grep`/`--author`/`--committer`
- ✅ `branch --contains` / `--no-contains` / `--merged` / `--no-merged`
- ✅ `cat-file --batch` / `--batch-check` / `--batch-all-objects`
- ✅ `reflog expire` / `delete` / `exists`
- ✅ `worktree lock` / `unlock` / `move` / `repair`
- ✅ `notes merge` (union / ours / theirs strategies)
- ✅ `add -p` complete prompts (`e`/`g`/`j`/`J`/`/`)
- ✅ `replace` mutating forms (`--delete`, `--graft`, positional create)
- ✅ `rerere` real implementation (status/diff/forget/gc/clear/remaining)
- ✅ SSH signing (`gpg.format = ssh` via `ssh-keygen -Y sign/verify`)
- ✅ ANSI color (`color.ui`) infrastructure module

Auto-record/auto-replay of rerere through merge/cherry-pick/rebase
remains deferred to a future plumbing pass.

## §C-Original — sub-feature gaps within shipped commands (kept for reference)

| Command | What's missing |
|---|---|
| `commit` | Editor flow (`git commit` with no `-m` opens `$EDITOR`). Today `-m`/`-F` only. |
| `log` | `-p` (patch), `--graph`, `--author=`, `--grep=`, `--follow`, `-S`/`-G` pickaxe. |
| `branch` | `--contains`, `--merged`, `--no-merged`. |
| `cat-file` | `--batch`, `--batch-check`, `--batch-all-objects`. (Today: `-t`/`-s`/`-p`/`-e` only.) |
| `reflog` | Only `show`; missing `expire`, `delete`, `exists`. |
| `worktree` | `lock`, `unlock`, `move`, `repair`. |
| `notes` | `merge` (three-way merge of notes refs). |
| `rebase` | Non-interactive only — `-i` is intentionally NON_GOAL. |
| `add -p` | `e/g/j/J/'/'` prompts are not-yet-implemented stubs. |
| `replace` | Only `--list`; mutating forms reject. |
| `rerere` | Pure stub — prints "not implemented" and exits 128. |
| Signing | GPG done; SSH (`gpg.format = ssh`) and X.509 (gpgsm) deferred. |
| Reftable | Single ref block per table; no compaction; no obj blocks; v2/SHA-256 plumbing in place but untested. |
| Pack queries | `.bitmap` and BIDX/BDAT chunks are *read* but not *used* for query speed. |
| Output | No ANSI color anywhere; locale ignored (English-only). |
| Hooks | Client-side only; `am`/`apply` hooks not wired (no `am`). |

---

## §D — Out of scope (won't implement)

These are documented in detail in [`NON_GOALS.md`](NON_GOALS.md). Listed
here as a single index so this file is the one-stop "what's the
landscape" answer.

**Workflow features explicitly NOT in rustygit's scope**
- Mail/patch workflow: `am`, `apply`, `mailinfo`, `mailsplit`, `format-patch`, `send-email`
- Sparse-checkout (cone and non-cone)
- Submodule porcelain (`submodule add/update/foreach`)
- LFS (depends on attribute filters, which are NON_GOAL)
- Attribute filters (smudge / clean / textconv)
- Partial clone / promisor remotes (`--filter=blob:none`)
- Interactive rebase (`-i`, `--rebase-merges`, `--autosquash`, `--exec`)
- `filter-branch` (deprecated upstream; use `git-filter-repo`)

**GUI / Perl / shell helpers** — rustygit is a binary-only port
- `gitweb`, `gitk`, `git-gui`, `git-instaweb`
- `git-svn`, `git-p4`
- `request-pull`, `mergetool`, `difftool`

**Server-side** — rustygit is client-only
- `receive-pack`, `upload-pack`, `upload-archive`, `update-server-info`

**Wire-protocol corners**
- Protocol v0/v1 client mode (we speak v2 for fetch, v1 only for push)
- `git://`, `ftp://`, `rsync://` transports
- Bundle URI / packfile URI clone optimizations (politely declined)
- Reachability bitmaps for query speedup (read-only acceptance only)

**Internal helpers** — private to git's implementation, not user-facing
- `checkout--worker`, `credential-cache--daemon`, `fsmonitor--daemon`
- `submodule--helper`, `remote-ext`, `remote-fd`

**Recent upstream additions with no clear user demand yet**
- `backfill`, `diagnose`, `bugreport`, `diff-pairs`, `history`,
  `last-modified`, `refs`, `repo`, `replay`, `for-each-repo`, `hook`

**Aliases**
- `annotate` (= `blame`), `init-db` (= `init`)

**Platform / i18n**
- Windows-specific path normalization (best-effort builds; no porcelain test coverage)
- i18n / gettext message catalogs (English-only through 1.0)

---

## How this document is maintained

- When something in §A or §B ships, move it to COMPAT.md and delete the
  row here.
- When a §C sub-feature ships, mark the parent command's row in
  COMPAT.md and delete the entry here.
- §D is stable. If a §D item gets reclassified (e.g. a user-demand
  case for `am`), update NON_GOALS.md first, then move the entry here.
