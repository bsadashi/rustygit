# rustygit ↔ git compatibility

This file enumerates every rustygit subcommand at one of three tiers,
plus the things rustygit explicitly does not implement.

## Tiers

* **T1 — byte-for-byte match with upstream `git`.** Stdout, exit codes,
  on-disk effects all match. Verified by a comparison test against the
  system `git` binary.
* **T2 — semantically equivalent, format may differ.** Same effect, output
  wording / color / locale may diverge.
* **T3 — rustygit-specific.** No corresponding upstream command, or
  upstream's exists but with very different semantics.
* **OUT — out of scope** (NON_GOALS.md). Use upstream `git`.

## Porcelain

| Subcommand   | Tier | Notes |
|--------------|:----:|-------|
| `init`       | T1   | Argv + `.git/` layout match. |
| `add`        | T1   | Includes `add -p` (interactive hunk staging). |
| `commit`     | T1   | `-m`, `--allow-empty`, `-S`, `--no-gpg-sign`, `--no-verify`. Editor flow deferred. |
| `commit-tree`| T1   | Plumbing — byte-identical commit serialization. |
| `status`     | T1   | Both `--porcelain` and the human form. |
| `log`        | T1   | `--oneline`, `--abbrev`, `--abbrev-commit`, `-n`. Filters (`-p`, `--graph`, `--author=`, `--grep=`) are deferred. |
| `show`       | T1   | Commits (header + diff vs first parent), root commits, tags (with depth cap), trees, blobs. |
| `diff`       | T1   | All 4 flavors (workdir-vs-index / workdir-vs-rev / cached / rev-vs-rev). `--exit-code`, `--quiet`. |
| `diff-tree`  | T1   | Plumbing two-tree diff. |
| `diff-index` | T1   | Plumbing tree-vs-index. |
| `diff-files` | T1   | Plumbing index-vs-workdir. |
| `branch`     | T1   | List / create / delete / rename. `--contains`, `--merged` deferred. |
| `checkout`   | T1   | Branch + path forms. Transactional workdir mutations (POLISH #9). |
| `switch`     | T1   | `-c`, `-C`, `--detach`. |
| `restore`    | T1   | `--source`, `--staged`, `--worktree`, pathspecs. |
| `reset`      | T1   | `--soft`, `--mixed`, `--hard`, paths form. |
| `merge`      | T1   | FF + true merge commit; conflict reporting. Strategy = ort-like only. |
| `merge-base` | T1   | All forms. |
| `merge-tree` | T1   | Plumbing three-way tree merge. |
| `merge-file` | T1   | Three-way file merge CLI; `--ours`/`--theirs`/`--union`. |
| `merge-index`| T1   | Enumerate unmerged index entries (driver-agnostic). |
| `mktree`     | T1   | Build a tree from ls-tree-format stdin. |
| `patch-id`   | T1   | Stable patch identity from unified diff on stdin. |
| `show-index` | T1   | Dump v2 .idx contents. |
| `pack-refs`  | T1   | Consolidate loose refs into packed-refs. |
| `count-objects` | T1 | `-v` verbose form; loose + in-pack + garbage stats. |
| `shortlog`   | T1   | Group log by author; `-n`/`-s`/`-e`. |
| `name-rev`   | T1   | Inverse of rev-parse; `--name-only`, `--tags`, `--stdin`. |
| `show-branch`| T1   | Branch tip viewer with reachability bitmask. |
| `for-each-ref`| T1  | `--format=` with %(refname/objectname/objecttype/HEAD). |
| `check-ref-format`| T1 | Validate ref names per git's rules. |
| `var`        | T1   | `GIT_AUTHOR_IDENT`/`GIT_EDITOR`/`GIT_PAGER`/`GIT_DEFAULT_BRANCH`. |
| `stripspace` | T1   | Whitespace cleanup; `-c` strip-comments, `-s` comment-lines. |
| `clean`      | T1   | `-f`/`-n`/`-d` untracked-file removal. |
| `describe`   | T1   | `--tags`/`--always`/`--abbrev`/`--dirty`. |
| `archive`    | T1   | tar archive of a tree-ish; `--prefix`. |
| `check-ignore`| T1  | Print paths that would be gitignored. |
| `check-attr` | T1   | Effective gitattributes per path. |
| `config`     | T1   | `--get`/`--set`/`--unset`/`--list`/`--add`, `--local`/`--global`. |
| `ls-files`   | T1   | `-s`/`-c`/`-o`/`-m`/`-i`/`-z`. |
| `rev-list`   | T1   | `--all`/`--reverse`/`--count`/`--max-count`/`A..B`/`^<rev>`. |
| `read-tree`  | T1   | Replace the index from a tree; `-u`/`--reset`. |
| `update-index` | T1 | `--add`/`--remove`/`--cacheinfo`/`--refresh`/skip-worktree. |
| `prune`      | T1   | Remove unreachable loose objects. |
| `prune-packed` | T1 | Remove already-packed loose objects. |
| `range-diff` | T1   | Diff two commit ranges, matched by patch-id. |
| `bundle`     | T1   | `create`/`verify`/`list-heads`/`unbundle`. |
| `grep`       | T1   | `-n`/`-i`/`-F`/`-E`/`--cached`/`-l`/`-c` + pathspec. |
| `interpret-trailers` | T1 | RFC2822 trailer manipulation; if-exists/if-missing. |
| `index-pack` | T3   | Stub: directs users to `repack` for now. |
| `fetch-pack` | T1   | Lower-level fetch (delegates to `fetch`). |
| `send-pack`  | T1   | Lower-level push (delegates to `push`). |
| `cherry-pick`| T1   | Single + range. Conflict halt mid-sequence. |
| `rebase`     | T1   | Non-interactive only. `-i` is OUT. |
| `revert`     | T1   | Single, multi-arg, **range (`A..B`)**, **`--mainline N`** for merges. `REVERT_HEAD` + `--continue`/`--abort`. |
| `tag`        | T1   | Lightweight, annotated, **signed (`-s`)**, `-d`, `-l <pattern>`, `-f`. Editor flow when `-a` given without `-m` deferred. |
| `mktag`      | T1   | Plumbing: validates a tag body from stdin and writes a tag object; oid byte-matches `git mktag`. |
| `verify-tag` | T1   | GPG verify via `signing.rs`. Exit 0 / 1 / 128 like git. |
| `stash`      | T1   | `push`/`pop`/`apply`/`list`/`show`/`drop`/`clear`, **`-u`/`--include-untracked` (3-parent shape)**. `--keep-index`, `--patch`, `stash branch` deferred. |
| `rev-parse`  | T1   | `HEAD`, `HEAD^`, `HEAD~N`, oids, abbrev oids, ref names. |
| `cat-file`   | T1   | `-t`, `-s`, `-p`, `-e`. `--batch` deferred. |
| `ls-tree`    | T1   | Recursive + non-recursive. |
| `ls-remote`  | T1   | Smart protocol v2 over HTTPS. |
| `clone`      | T1   | HTTPS (smart v2) + local. |
| `fetch`      | T1   | HTTPS smart v2. |
| `pull`       | T1   | Fetch + merge composite. |
| `push`       | T1   | HTTPS smart v2 with credentials. |
| `update-ref` | T1   | Transactional with `--create-reflog`. |
| `show-ref`   | T1   | All filters. |
| `symbolic-ref`| T1  | Read + write. |
| `hash-object`| T1   | `-w`, `--stdin`. |
| `write-tree` | T1   | Plumbing — byte-identical to git. |
| `reflog`     | T1   | `show` only. Other subforms deferred. |
| `commit-graph`| T1  | `write`, `verify`. |
| `multi-pack-index`| T1 | `write`, `verify`. |
| `pack-objects`| T1  | Reads oid list on stdin. |
| `repack`     | T1   | Consolidates packs. |
| `unpack-objects`| T1| Explodes a pack into the loose store. |
| `verify-pack`| T1   | |
| `verify-commit`| T1 | GPG signature verification via `gpg`. |
| `gc`         | T1   | Loose prune + repack. |
| `fsck`       | T1   | Object-graph integrity. |
| `bisect`     | T1   | `start`, `good`, `bad`, `reset`. |
| `blame`      | T1   | Line-level authorship. |
| `notes`      | T1   | `add`/`show`/`append`/`copy`/`remove`/`edit`/`list`/`prune`. `merge` is OUT. |
| `worktree`   | T1   | `add`/`list`/`remove`/`prune`. `lock`/`unlock`/`move`/`repair` deferred. |
| `replace`    | T1   | `--list` only; mutating forms reject (NON_GOALS Batch E). |
| `rm`         | T1   | |
| `mv`         | T1   | |
| `rerere`     | T3   | Stub — prints "not implemented" (NON_GOALS Batch E). |
| `prune-locks`| T3   | rustygit-specific. Cleans `*.lock` and `checkout.tmp.*/` orphans. |
| `doctor`     | T3   | rustygit-specific. Repo health-check. |

## Top-level flags

| Flag | Tier | Notes |
|------|:----:|-------|
| `-C <PATH>` | T1 | Change working directory before running. |
| `-c <KEY=VALUE>` | T1 | Config override layer; matches git's "before subcommand only" rule. |

## Output divergences (known, documented)

* **Color output**: rustygit does not emit ANSI color codes today. `git`
  honors `color.ui` / `--color`. Tier 2 across the board for color.
* **Date/locale**: rustygit emits ASCII English dates (e.g.
  `Mon Jan 1 00:00:00 2026 +0000`). `LC_ALL` is ignored
  (NON_GOALS Batch C).
* **Submodule typechange entries** in `status --porcelain`: rustygit lists
  them; git silently skips (we treat submodules as out-of-scope and could
  in theory skip too — flagged for a future polish pass).

## Out of scope — use upstream `git`

These features are explicitly **not** in rustygit's roadmap (see
`NON_GOALS.md`). Running an inbound command that requires one will
either error clearly or fall back to git's default behavior:

* Submodules (`submodule add/update/foreach`).
* Sparse-checkout (cone + non-cone).
* Attribute filters (smudge/clean/textconv) — and therefore Git-LFS.
* Interactive rebase (`-i`, `--rebase-merges`, `--autosquash`, `--exec`).
* Partial clone (`--filter=blob:none`, promisor remotes).
* Perl/shell helpers: `git-svn`, `git-p4`, `git-instaweb`, `gitweb`, `gitk`,
  `git-gui`, `request-pull`, `mergetool`, `difftool`.
* `filter-branch` (use `git-filter-repo` upstream).
* Protocol v0/v1 (we speak v2 only) and `git://`, `ftp://`, `rsync://`.
* `bundle-uri` / `packfile-uris` clone optimizations (we politely decline).
* Bitmap/Bloom filter *use* for query speedup (we ignore them on read,
  which is correct).
* Windows path-normalization parity (Linux + macOS only — runs on Windows,
  but the polish isn't there).

## SemVer policy

rustygit follows [SemVer](https://semver.org). For the CLI surface:

* **MAJOR**: a documented Tier-1 subcommand changes its argv, exit code,
  or stdout shape in a way that breaks scripts. Removing or renaming a
  flag. Dropping a subcommand.
* **MINOR**: a new subcommand or new flag. Newly-shipped Tier-1 oracle
  coverage for an existing subcommand.
* **PATCH**: bug fixes, perf improvements, documentation, internal
  refactors.

The `--help` snapshot tests in `tests/help_snapshot.rs` are the
machine-checkable contract. Any change to a subcommand's flag set must
update those tests **in the same PR** so reviewers see the SemVer
implication immediately.
