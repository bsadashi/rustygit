# Polish & Tech Debt

Items deferred from earlier milestones, surfaced by the cross-cutting validation
pass after M8. None block forward progress; each is scoped small enough to fix
in a single focused session.

Ordered by impact-per-hour: cheapest wins first.

---

## 1. `cargo clippy --fix` sweep — ✅ DONE

**Status**: Done. Warnings dropped from **139 → 33** after auto-fix.
Remaining 33 are all cosmetic doc-list indentation in module-level
comments (clippy::doc_lazy_continuation / doc_list_overindented). No real
bugs surfaced. The `status_staged_then_modified_shows_MM` snake-case
violation and an octal-escape readability nit in `cache_tree.rs` were
fixed manually. Auto-fix touched ~27 files; 766 tests still passing in both
debug and release. The clippy floor is now low enough that real future
warnings will be visible against the noise.

---

## 2. `--abbrev-commit` short-oid for `log --oneline` — ✅ DONE

**Status**: Done. `LogArgs` now exposes `--abbrev=<N>` and `--abbrev-commit`;
`--oneline` implies both. Default abbrev width is 7 chars (matching git's
`core.abbrev = 7`). The `Merge:` line in the medium format also abbreviates.
Verified `rustygit log --oneline` byte-matches `git log --oneline`, same
for `--abbrev=4`/`10`/`14`. New test `log_abbrev_width_matches_git` in
`tests/m3_compat.rs`; existing tests in `m3_compat.rs` and `m15_compat.rs`
that asserted the buggy full-width behavior were updated to expect the
correct git-compatible output.

**Deferred sub-item**: dynamic-bump of abbrev width on collision (git's
`core.abbrev` auto-grow logic for very large repos). Static 7-default is
sufficient for everything we test.

---

## 3. `git status` human-readable form — ✅ DONE

**Status**: Done. `rustygit status` (no flag) now emits the verbose human
form that `git status` does. `--porcelain` and `-s`/`--short` continue to
emit porcelain v1. New `Human` formatter in `src/worktree/status.rs`
handles: "On branch <name>", unborn-branch "No commits yet", detached
HEAD "HEAD detached at <short>", upstream-tracking stub ("Your branch is
up to date with 'origin/<name>'." when `refs/remotes/origin/<name>` has
the same oid as HEAD), the three section bodies ("Changes to be
committed:" / "Unmerged paths:" / "Changes not staged for commit:" /
"Untracked files:"), and all four footer variants ("working tree clean"
/ "nothing added to commit" / "no changes added to commit" / unborn
"nothing to commit (create/copy files…)").

Four new compat tests in `tests/m4_compat.rs`
(`status_human_form_byte_matches_git_*`) assert byte-equal output against
system `git status` for empty repo, clean tree, unstaged-mod, and
untracked-file scenarios. 773 tests passing (was 769, +4 new).

The 9 prior tests that compared `rustygit status` to `git status
--porcelain` were updated to pass `--porcelain` explicitly so they
continue to test the porcelain format. Self-host smoke test and m6 tests
that asserted clean-tree status was empty also switched to `--porcelain`.

**Known divergence**: writing a raw oid into `.git/HEAD` without going
through `git checkout --detach` produces git's "Not currently on any
branch." message while rustygit always says "HEAD detached at <short>".
This is an unusual code path; the common `git checkout <oid>` flow does
emit the latter wording in upstream git too.

**Deferred sub-item**: real ahead/behind counts in the upstream-tracking
line require fetching divergence info. We stub it as up-to-date or
diverged (without counts) for now; counts ship when we have networking
parity.

---

## 4. Shallow-clone awareness in revparse / log — ✅ DONE

**Status**: Done. `Repository` now reads `.git/shallow` at open time into
a `HashSet<ObjectId>` and exposes `is_shallow_boundary(oid)`. The log
walker checks this after printing each commit; when the current oid is on
the boundary, the loop breaks gracefully instead of trying to read a
parent that's missing from the odb. Verified live against our
`~/Git_Repos/git/.git/` (a `--depth=1` clone): `rustygit log` walks
exactly 1 commit and exits 0; previously it crashed with `object not
found`. Test `log_on_synthetic_shallow_clone_stops_at_boundary` in
`tests/m3_compat.rs` builds a controlled shallow scenario and asserts the
log stops at exactly 1 commit.

**Revparse side**: kept as-is. If you try to `rev-parse HEAD^` past a
shallow boundary, you get a clear `object not found: <parent-oid>` error
(which is exactly what git itself does in the same scenario). The major
user-visible fix was `log`, which is now correct.

---

## 5. Per-directory `.gitignore` files in status — ✅ DONE

**Status**: Done. Refactored `walk_untracked` from iterative-stack to
recursive `walk_dir`/`walk_dir_inner`. Each directory entry checks for
`<dir>/.gitignore`, pushes onto `IgnoreStack` if present, recurses, then
pops on return (`pushed_here` counter for correctness on errors). Added
`IgnoreStack::pop_layer()` + `depth()` helpers.

Verified with `tests/m4_compat.rs::status_respects_nested_gitignore_negation`:
root `*.log` excludes, but `sub/.gitignore`'s `!important.log` correctly
re-includes the file. 768 tests passing.

**Known follow-up (separate)**: `git status` collapses an all-untracked
subdirectory to a single `sub/` line; rustygit lists each file underneath.
This is a display optimization in git's wt-status.c, NOT a gitignore
correctness issue. Filed for a future polish pass.

---

## 6. Hand-rolled `Lru` should swap to LRU semantics — ✅ DONE

**Status**: Done. The `Lru::get` impl at `src/pack/store.rs:53-58` already
bumps `entry.1 = age` to the current counter, so eviction picks the
least-recently-accessed entry. Test `lru_evicts_least_recently_used` at
`src/pack/store.rs:253` verifies the semantics: insert 1/2/3, touch 1,
insert 4 → 2 is evicted (not 1).

This was either always correct or fixed by an agent during M7. POLISH.md
description was stale.

---

## (Originally numbered 7 below; renumbered to keep ordering)

## 7. `git add -p` interactive hunk staging — ✅ DONE

**Status**: Done. `rustygit add -p` (also `--patch`) now walks each hunk of
the worktree-vs-index diff for tracked files and prompts the user. New
module `src/add_patch.rs` exposes the pure data layer (`Hunk`, `HunkLine`,
`parse_hunks_from_diff`, `split_hunk`, `apply_hunks_to_base`,
`format_hunk`) and `src/cli/add_patch.rs` runs the prompt loop. The
hunk parser round-trips the output of `crate::xdiff::unified_diff` —
header, body, `\ No newline at end of file` markers, function-context
trailers — into a structured model we can transform.

**Subset shipped (POLISH.md commitment was minimum y/n/q/a/d/?/s plus
optional e)**:
- `y` — stage this hunk
- `n` — skip this hunk
- `q` — quit; skip remaining hunks
- `a` — stage this and all later hunks in this file
- `d` — skip this and all later hunks in this file
- `s` — split the hunk into smaller hunks at context-line boundaries
- `?` — print help text

**Not implemented (documented in help text as "not yet implemented")**:
- `e` — manually edit the hunk (would spawn `$EDITOR`)
- `g` — go to numbered hunk
- `j`/`J` — leave undecided and continue
- `/` — regex hunk search

For deleted files, a simplified `[y,n,q,a,d,?]?` prompt stages the
deletion or skips it.

**Architecture**: the CLI orchestrator uses the existing
`diff::flatten_index` + `diff::flatten_workdir_against_index` pair (which
already hashes worktree blobs into the odb) to enumerate candidates,
parses the per-file unified diff into `Vec<Hunk>`, walks them
interactively, and finally applies the chosen subset to the base content
to build the new blob. The worktree file is intentionally NOT touched —
unchosen changes remain in the working tree, matching git's semantics.

**Test coverage**: 16 unit tests in `src/add_patch.rs` covering parsing
edge cases (multi-hunk, no-newline marker, pure addition, pure deletion,
function-context trailer), split mechanics, and applier correctness; 1
unit test in `cli/add_patch.rs` for the empty-repo no-op path; 11
integration tests in `tests/add_patch_compat.rs` driving the binary via
`assert_cmd`'s `write_stdin` for every prompt char and a porcelain
round-trip against system git for the "yes to all" path.

**Test count**: 815 passing (was 773; +42 — 28 of which are directly new
add_patch tests).

**Known limitations**: `e/g/j/J/'/'` flagged as not-yet-implemented; the
prompt loop uses cooked-mode line-buffered input (one Enter per choice)
rather than single-char raw mode — matches git's `set inputrc -- vi` /
no-readline fallback path, and is what real git ships with when
`color.ui = never` or `EDITOR` isn't a TTY.

---

## 8. Reftable backend for refs — ✅ DONE

**Status**: Done. New `ReftableStore` selectable via `extensions.refStorage =
reftable` in `.git/config`. `Repository::open` reads the key (new helper
`read_ref_storage_format` plus the `RefStorageFormat` enum) and routes to
either the existing loose+packed `CompositeRefStore` or the new reftable
backend pointed at `.git/reftable/`. Loose+packed remains the default.

Files added under `src/refs/reftable/`:
* `format.rs` (~370 LoC) — varint encode/decode (reftable spec §3.5), 24-byte
  v1 header, 68-byte v1 footer with CRC-32, block-header struct, zlib
  inflate/deflate wrappers. 8 unit tests including a known CRC-32 vector and
  a real git-produced header sample.
* `reader.rs` (~510 LoC) — `TableReader` parses a single `.ref` file; ref
  blocks (spec §4.3) and log blocks (zlib-deflated, §4.5) are both decoded.
  `StackReader` opens every file in `tables.list` and merges them
  newest-wins; tombstones (`value_type=0`) are honored across the stack.
* `writer.rs` (~290 LoC) — emits a single-ref-block, optionally-log-block,
  v1 footer reftable. Restart-at-every-record (prefix compression
  intentionally disabled for simplicity); deletions written as tombstones.
* `transaction.rs` (~250 LoC) — `RefTransactionTrait` impl. On `commit()`,
  acquires `tables.list.lock`, computes the next `update_index` from the
  highest existing `max_update_index`, writes a new
  `${idx:012x}-${idx:012x}-${rand}.ref` file, and atomically appends to
  `tables.list` via the lock + rename. HEAD reflog mirroring is honored
  (matches loose backend).
* `mod.rs` (~135 LoC) — public `ReftableStore` API + module wiring.

New test file `tests/reftable_compat.rs` (6 tests, all gated on
`git init --ref-format=reftable` working — skip cleanly otherwise):
* `rustygit_reads_git_written_reftable_head` — git creates a commit on a
  reftable repo, rustygit `rev-parse HEAD` matches `git rev-parse HEAD`.
* `rustygit_show_ref_matches_git_on_reftable` — sorted `show-ref` output is
  byte-equal across backends.
* `rustygit_update_ref_writes_reftable_git_reads_it_back` — rustygit creates
  a ref, git's `show-ref` sees it. Full round trip.
* `reftable_round_trip_branch_create_and_delete` — rustygit creates, git
  deletes, rustygit no longer sees it (tombstone cascade).
* `reftable_symbolic_head_read` — symbolic-ref HEAD matches.
* `reftable_branch_listing_via_iter` — multiple branches enumerate
  correctly through the stack.

Manual sanity beyond the test file: 4-table stack with rustygit-written
deletions, `git fsck` clean on rustygit-emitted reftables, `git reflog`
correctly reads rustygit-written log entries.

**Known limitations** (deferred for a post-1.0 polish pass):
* Single ref block per table (block_size = 4 KiB, ~100 refs/table). Larger
  single transactions would need ref index blocks and multi-block layout.
* Compaction not implemented — every transaction appends a new table; the
  stack only grows. JGit-style periodic compaction belongs in a `pack-refs`
  command for reftable.
* Obj blocks omitted — by-oid reverse lookup falls back to scanning all refs.
* v2/SHA-256 type plumbing is in place but untested against a SHA-256 repo.
* Prefix compression on ref names disabled (every record is a restart point).

**Test count**: 801 → 815 (+14: 8 lib unit tests in `refs::reftable::format`,
6 integration tests in `tests/reftable_compat.rs`).

---

## 9. Workdir mutations in `unpack_trees::checkout_tree` aren't transactional — ✅ DONE

**Status**: Done. New `StagedCheckout` (`src/unpack_trees.rs`) gives
`checkout_tree` a two-phase commit. Stage phase writes every new blob to
`<gitdir>/checkout.tmp.<pid>.<nanos>/<id>`. Commit phase A renames each
existing Update/Delete target into a `shadow_orig` slot inside the same
shadow dir, then commit phase B renames each shadow_new into its target.
Any failure during commit reverses the renames in reverse order so the
workdir is restored to its pre-call state. Drop sweeps the shadow dir
best-effort.

This beats upstream git, which has the same partial-checkout caveat we
used to document — git itself can leave the workdir half-written if a
mid-checkout error fires.

**Test coverage** (3 new tests in `src/unpack_trees.rs::tests`):
* `commit_failure_rolls_back_workdir` — pre-places a directory at one of
  two new-file targets so the rename collides; asserts the *other* file's
  pre-call content is restored and the directory is preserved.
* `shadow_dir_is_cleaned_up_on_drop` — Drop cleans the shadow dir when
  no commit ran.
* `shadow_dir_is_cleaned_up_after_successful_checkout` — after a clean
  checkout, no `checkout.tmp.*` dirs leak.

920 → 923 passing.

---

## Tracking

- [x] 1. clippy --fix sweep
- [x] 2. log --oneline abbrev
- [x] 3. status human-readable form
- [x] 4. shallow-clone awareness
- [x] 5. nested .gitignore in status walk
- [x] 6. true-LRU in pack cache
- [x] 7. `add -p` interactive hunk staging
- [x] 8. reftable backend
- [x] 9. workdir transactional checkout
