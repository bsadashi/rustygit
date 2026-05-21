# Non-Goals — Now Goals

The 23 features the original plan ruled out of M0–M16 scope, surfaced here so
we can take them on one at a time. Same shape as POLISH.md: ordered by
impact-per-hour, cheapest wins first, each item scoped to a single focused
session unless explicitly flagged as "multi-session."

The grouping comes directly from the plan's "Explicit non-goals" bullet list
(see `~/.claude/plans/i-know-it-is-sprightly-pinwheel.md`).

---

## Batch A — Cheap rejections (drop-or-stub) — ✅ DONE

**Status**: Done. Two surfaces got fixed in one round.

**CLI side** — new `cli::explain_unsupported_subcommand(name)` intercepts
the 10 drop-entirely subcommand names BEFORE clap parses, printing a
named, paragraph-level explanation that links to the right upstream
alternative:

- `gitweb` → "Perl/CGI web frontend; install git-gitweb"
- `gitk` → "Tk-based history viewer; try 'tig' / 'lazygit'"
- `git-gui` (and `gui`) → "Tk-based commit GUI; try 'lazygit' / 'gitui'"
- `git-svn` (and `svn`) → "Perl SVN bridge; use upstream"
- `git-p4` (and `p4`) → "Python Perforce bridge; use upstream"
- `git-instaweb` (and `instaweb`) → "wraps gitweb; use upstream"
- `request-pull` → "mailing-list helper; use a forge pull request"
- `mergetool` and `difftool` → "invoke vimdiff/meld/kdiff3 directly"
- `filter-branch` → "deprecated; use git-filter-repo instead"

All exit with code 128. Typo'd names (e.g. `gitwev`) fall through to
clap's standard "unrecognized subcommand" error — verified.

**Transport side** — `transport::connect_upload_pack` gained
`classify_unsupported()` to map `git://`, `ftp://`, `ftps://`, and
`rsync://` to a new `TransportError::UnsupportedTransport { url, reason }`
variant with named, actionable text. Schemes are URL-spec
case-insensitive (`GIT://` → same rejection as `git://`).
`TransportError::NotV2` got expanded to explicitly name protocol v0/v1
and suggest upgrading the server, so users see *why* their old server
won't work rather than a cryptic "didn't advertise v2".

**Tests added**: `tests/non_goals_drops.rs` (3 tests covering all 10 drop
names, alias forms, and the typo-falls-through case) + 5 unit tests in
`src/transport/mod.rs::tests` (named-rejection wording for each scheme
plus the NotV2 v0/v1 wording assertion). 823 passing (was 815, +8 new).

---

## Batch B — Read-only format support — ✅ DONE (acceptance), optimization deferred

**Status**: "Don't break on it" verified for all three formats. Actually
*using* the optimizations to speed up queries is deferred — splitting
each item in two preserves the original commitment ("read-only support")
while honestly labelling what's left to land.

**12a. `.bitmap` reachability files** — already silently ignored by pack
discovery (filename-suffix filter selects `.pack` only). A repo produced
by `git repack -d -a --write-bitmap-index` opens, resolves, walks, and
cat-files correctly. Verified by `pack_bitmap_does_not_break_reads`.

**12b. Use bitmaps to speed up `rev-list --count` / reachability queries**
— **deferred**. The bitmap format
(`~/Git_Repos/git/Documentation/technical/bitmap-format.txt`) is
non-trivial to parse and saves wall time, not correctness. File for a
post-1.0 optimization pass.

**13a. Commit-graph BIDX/BDAT (Bloom) chunks** — already silently ignored
by `CommitGraph::open` (line 476: "Other chunks (GDA2, BIDX, BDAT,
BASE...) are ignored for read"). Verified by
`commit_graph_with_bloom_chunks_still_reads`.

**13b. Use Bloom filters to filter pathspecs in `log -- <path>`** —
**deferred**. The filter is a per-commit changed-paths probe that lets
the log walker skip commits cheaply; it's a perf-only feature.

**14a. Multi-pack-index BTMP chunks** — already silently ignored by
`MidxFile::open` (line 446: "unknown chunk — ignore for forward-compat").
The sibling `.bitmap` file is similarly invisible. Verified by
`multi_pack_bitmap_does_not_break_reads`.

**14b. Use the midx bitmap to accelerate cross-pack queries** —
**deferred** for the same perf-only reason.

**Bonus**: `split_commit_graph_chain_falls_back_gracefully` verifies that
when git writes a split commit-graph chain to
`.git/objects/info/commit-graphs/`, rustygit doesn't choke — it just
walks the commit DAG directly (no chain reader yet). Using split chains
is a separate deferred optimization.

**Tests added** (5 new in `tests/non_goals_format_compat.rs`,
oracle-driven via real `git repack --write-bitmap-index` /
`commit-graph write --changed-paths` / `multi-pack-index write --bitmap`):
- `pack_bitmap_does_not_break_reads`
- `commit_graph_with_bloom_chunks_still_reads`
- `multi_pack_bitmap_does_not_break_reads`
- `split_commit_graph_chain_falls_back_gracefully`
- `repo_with_all_optimizations_applied_still_reads` (combines all three +
  `git fsck --full` confirms no damage). 828 passing (was 823, +5).

---

## Batch C — i18n posture — ✅ DONE

**Status**: Done. New module `src/i18n.rs` (~200 LoC + 10 tests) codifies
the policy and provides forward-compatible hooks.

**Policy** (unchanged from the plan): English-only through 1.0. No
`LC_ALL`/`LANG`/`LC_MESSAGES`-driven translation, no `gettext`/`.po`
files, no locale-aware sorting/casing/number formatting. Byte-wise
everywhere.

**Hooks added**:
- `tr("…")` — identity translation hook. Mirrors upstream git's `_("…")`
  macro (we can't use bare `_` because Rust reserves it in expression
  position). `const fn` so it works in `const X: &str = tr("…")`. When
  the policy changes post-1.0, this is the single swap point.
- `q_(singular, plural, n)` — English pluralization (singular iff n==1).
  Mirrors git's `Q_("commit", "commits", n)`.
- `is_ascii_only_locale()` — checks `LC_ALL` / `LC_MESSAGES` / `LANG` in
  that order. Returns true if the chosen value's base name is `C` or
  `POSIX` (case-insensitive, strips `.encoding` / `@modifier` suffixes).
  Today's strings are ASCII anyway, so this is future-proofing rather
  than a behavior switch.
- `asciify(s)` — replace the small set of fancy English punctuation
  (em-dash, en-dash, ellipsis, curly quotes) with ASCII equivalents.
  Returns `Cow::Borrowed` (no allocation) when input has none of those
  characters; does NOT touch legitimate Unicode in paths/names/messages.

**User-facing strings ASCII-ified** (3 sites):
- `src/cli/write_tree.rs::WriteTreeError::EmptyIndex` — em-dash → `--`
- `src/transport/mod.rs::TransportError::NotV2` — em-dash → `--`
- `src/cli/mod.rs::explain_unsupported_subcommand("mergetool"/"difftool")` —
  Unicode ellipsis → `...`

**Tests** (10 in `i18n::tests`):
- `tr_is_identity` / `tr_const_works_in_const_context`
- `q_returns_singular_when_n_is_one` / `q_returns_plural_for_zero_and_many`
- `asciify_passes_ascii_through_unchanged` (Cow::Borrowed regression)
- `asciify_replaces_em_dash` / `asciify_replaces_ellipsis` /
  `asciify_replaces_curly_quotes`
- `asciify_preserves_legitimate_unicode` (paths with `é`, `Ω` left alone)
- `is_ascii_only_locale_handles_c_with_encoding` (smoke test)

838 passing (was 828, +10).

---

## Batch D — Bundle URI / packfile URI clone optimizations — ✅ DONE

**Status**: Done. The existing fetch builder never opted in to
`packfile-uris` or `bundle-uri`, so behavior was already correct — the
work here was to **lock that in** with doc comments and tests so a
future change can't silently start opting in.

**What "polite decline" means** in concrete terms:

- **`packfile-uris`** is a fetch-time capability. Our request body
  (`build_fetch_request` in `src/transport/protocol_v2.rs`) sends only
  `thin-pack`, `ofs-delta`, `no-progress`, then `want <oid>` lines and
  `done`. No `packfile-uris=…` capability is sent, so the server
  responds with a normal in-protocol packfile stream.
- **`bundle-uri`** is a SEPARATE command (`command=bundle-uri`),
  advertised alongside `fetch`/`ls-refs` in the v2 capability list. We
  never send `command=bundle-uri`, so even if a server advertises it
  we get a normal clone.
- **`filter`** (partial clone) is also intentionally not opted into
  here — that's NON_GOALS.md item out-of-spec (partial clone). Without
  the `filter ...` capability, the server returns all reachable objects.

**Defensive parsing** — server-side cap ads that LIST `bundle-uri` or
`packfile-uris=<allowed-hashes>` parse cleanly; the names land in
`CapabilityAdvertisement::commands` for inspection but the fetch
builder ignores them. Confirmed by
`parse_v2_advertisement_with_bundle_and_packfile_uris`.

**Server-emitted `packfile-uris` section** in a fetch response is also
tolerated: `parse_fetch_response` already skips any section that isn't
`packfile`, so a misconfigured server doesn't break the clone. Locked
in by `fetch_response_with_packfile_uris_section_is_tolerated`.

**Tests added** (3 in `transport::protocol_v2::tests`):
- `parse_v2_advertisement_with_bundle_and_packfile_uris` — both names
  parse + land in `supports("bundle-uri")` / `supports("packfile-uris")`.
- `fetch_request_does_not_opt_into_packfile_uris` — byte-level regression:
  our fetch body contains `thin-pack`/`ofs-delta`/`no-progress` and does
  NOT contain `packfile-uris`, `filter `, or `bundle-uri`.
- `fetch_response_with_packfile_uris_section_is_tolerated` — server
  emitting an unsolicited `packfile-uris` section in the response still
  yields the underlying packfile bytes.

841 passing (was 838, +3).

---

## Batch E — Niche command stubs — ✅ DONE

**Status**: Done. Three distinct shapes shipped, matching the underlying
feasibility of each.

**17. `rerere`** — pure stub. New module `src/cli/rerere.rs` declares the
subcommand with all six upstream forms (`status`, `diff`, `forget <paths>`,
`gc`, `clear`, `remaining`) so users get clap argument validation for free.
Every form prints the same "not implemented; the conflict-resolution
database is its own subsystem (~1500 LoC in upstream git's `rerere.c`);
deferred to post-1.0" message and exits 128.

**18. `replace`** — partial implementation. `--list` actually works (and
byte-matches `git replace --list`); mutating operations (`--delete`,
`--edit`, `--graft`, positional create) print "not implemented" and exit
128. The list path narrows iteration to `refs/replace/` via the
`RefStore::iter(Some("refs/replace/"))` prefix hint and prints each ref's
stripped suffix (the original-oid form upstream uses). Glob patterns
(`<prefix>*`, `<prefix>????`) filter via the existing
`crate::wildmatch::wildmatch` engine. Symbolic refs in
`refs/replace/<oid>` are skipped with a warning (pathological but
technically allowed).

**19. `filter-branch`** — already shipped in Batch A as part of
`explain_unsupported_subcommand`. Points users at `git-filter-repo` as
the modern replacement (upstream git itself deprecates `filter-branch`).

**Tests added** (8 in `tests/non_goals_stubs.rs`):
- `rerere_bare_exits_128_with_named_message` — `rustygit rerere` alone
- `rerere_subcommands_all_exit_128` — all 5 sub-forms
- `rerere_forget_with_paths_exits_128` — with positional args
- `replace_list_on_empty_repo_prints_nothing` — clean empty case
- `replace_list_reads_git_written_refs` — oracle: write replacement
  with `git replace <a> <b>`, verify rustygit lists it byte-for-byte vs.
  `git replace --list`
- `replace_list_with_pattern_filters` — `<prefix>*` glob filtering
- `replace_mutating_flags_exit_128` — `--delete`/`--edit`/`--graft` all
  reject
- `replace_with_positional_oids_no_list_exits_128` — `replace <a> <b>`
  (create form) rejects

Plus 3 unit tests in `cli::replace::tests` for the glob matcher (`*`,
`?`, literal). 853 passing (was 841, +12).

---

## Batch F — Signing — ✅ DONE (GPG; SSH/X.509 deferred)

**Status**: Done for the GPG format, which is what `commit.gpgsign=true`
defaults to. SSH and X.509 signing share the same `gpgsig` trailer format
and the same porcelain entry point (`-S` flag); a future commit can
slot them in by extending [`signing::Signer`].

**New module `src/signing.rs`** (~300 LoC + 4 unit tests):
- `Signer` trait — `sign(payload) -> Signature` and
  `verify(payload, signature) -> VerifyOutcome`. Trait-based so commit
  porcelain accepts mocks for testing.
- `GpgSigner` — production impl. Spawns `gpg --detach-sign --armor --batch
  --pinentry-mode loopback [--local-user <key>]` and pipes the payload over
  stdin. Verify parses `[GNUPG:] GOODSIG` / `BADSIG` / `NO_PUBKEY` /
  `VALIDSIG` status-fd lines to produce a structured `VerifyOutcome`.
- `MockSigner` (in `signing::testing`, intentionally NOT cfg-test-gated
  so integration tests can reach it) — fixed signature + verify outcome,
  records every payload it was asked to sign.

**Config wired**:
- `gpg.program` — gpg binary to invoke. Defaults `"gpg"`.
- `user.signingkey` — passed via `--local-user`. Optional.
- `commit.gpgsign=true` — enables sign-by-default in `cli::commit::run`.

**Commit porcelain** — new `-S [<keyid>]` and `--no-gpg-sign` flags
mirror upstream git. `--no-gpg-sign` always wins; explicit `-S` second;
`commit.gpgsign=true` config as the silent default. The signing happens
on the UNSIGNED commit body (no `gpgsig` header), and the signature is
folded into a `gpgsig` continuation-line header before the final
sha-and-store — matching `git commit -S`'s byte layout, so the resulting
commit oid is identical to what git would have produced.

**New porcelain `verify-commit`** — reads the commit, strips `gpgsig`,
re-serializes the unsigned body, calls `signer.verify`. Exit code 0 on
good, 1 on bad/unknown-key, 128 on missing/unparseable commit. Argument
parity with `git verify-commit` (multiple commits, `-v`).

**Tests** (5 in `tests/non_goals_signing.rs`):
- `create_commit_with_mock_signer_folds_signature_into_gpgsig` — pure
  in-memory check that (a) the signer sees the body WITHOUT `gpgsig`,
  (b) stripping `gpgsig` from the stored commit recovers the exact bytes
  the signer signed.
- `rustygit_signed_commit_verifies_with_git` — gpg-gated. Generate a
  passphraseless RSA key in a disposable GNUPGHOME, `rustygit commit -S`,
  then assert `git verify-commit HEAD` succeeds and outputs `Good
  signature`.
- `rustygit_verifies_git_signed_commit` — the reverse oracle.
  `git commit -S` produces the commit; `rustygit verify-commit HEAD`
  must emit `GOODSIG`.
- `verify_commit_on_unsigned_commit_fails_with_128` — unsigned commit
  → exit 128 + "no signature" stderr.
- `no_gpg_sign_overrides_config_default` — `commit.gpgsign=true` plus
  `--no-gpg-sign` → unsigned commit (verify-commit returns 128).

Plus 4 unit tests in `signing::tests` covering `MockSigner` recording,
`GpgSigner::from_config` defaults, and `from_config` honoring
`gpg.program`/`user.signingkey`. 862 passing (was 853, +9 new).

**`tempfile` was promoted from dev-dependency to regular dependency**
because `GpgSigner::verify` needs a scratch directory for the detached
signature + payload pair. (gpg's detached-verify CLI doesn't accept both
on stdin.)

**Deferred sub-items**:
- SSH signing (`gpg.format = ssh`, `user.signingkey = ~/.ssh/id_ed25519.pub`)
  — slot in as a second `Signer` impl reading from `ssh-keygen -Y sign`.
- X.509 signing (`gpg.format = x509` via gpgsm) — third `Signer` impl.
- Signed-tag (`git tag -s`) — needs the same wiring on the tag porcelain.
  Today tag-writing isn't implemented as a CLI; will pair with that.

---

## Batch G — Hooks framework — ✅ DONE (client hooks; server hooks out of scope)

**Status**: Done. A real client-side hooks dispatcher (`src/hooks.rs`) ships
the full client-hook set with correct argv / stdin / env wiring, honors
`core.hooksPath`, and aborts the parent op on a non-zero exit from a
blocking hook.

**New module `src/hooks.rs`** (~250 LoC + 13 unit tests):
- `HookRunner` — built once per command via `HookRunner::from_repo(&repo)`.
  Resolves the hooks directory (`core.hooksPath` from config, falling back
  to `<gitdir>/hooks/`) and caches the env-var passthrough list.
- `HookOutcome` — `Ran { exit_code }`, `NotPresent` (missing OR not
  executable — both treated as success no-op), `Skipped { reason }` (hooks
  dir itself unusable). `aborts_parent()` returns true iff a hook ran with
  a non-zero exit.
- POSIX exec-bit detection via `std::os::unix::fs::PermissionsExt`. On
  non-Unix any regular file counts as executable so the module still
  compiles.
- Env vars set on every hook: `GIT_DIR`, `GIT_WORK_TREE`. Passed through
  when present in the parent env: `GIT_INDEX_FILE`, `GIT_EDITOR`, `EDITOR`,
  and the `GIT_AUTHOR_*` / `GIT_COMMITTER_*` family.
- stdout/stderr are captured then forwarded after `wait_with_output()` so
  the user sees the hook's diagnostics live. (We can't `Stdio::inherit()`
  under cargo's test harness — sharing the harness's pipe FDs across many
  parallel test hooks deadlocks when buffers fill.)

**Hooks shipped (10 wired, 7 deliberately skipped)**:

| Hook | Fired by | Blocks? |
|------|----------|---------|
| `pre-commit` | `cli::commit::run` | yes |
| `prepare-commit-msg` | `cli::commit::run` (incl. `--no-verify`) | yes |
| `commit-msg` | `cli::commit::run` | yes |
| `post-commit` | `cli::commit::run` (after success) | no |
| `pre-push` | `cli::push::run` | yes |
| `pre-rebase` | `cli::rebase::run` | yes |
| `post-rewrite` | `cli::rebase::run` (after all picks done) | no |
| `pre-merge-commit` | `cli::merge::run` (merge-commit path) | yes |
| `post-merge` | `cli::merge::run` (FF + merge-commit) | no |
| `post-checkout` | `cli::checkout`, `cli::switch`, `cli::clone` | no |
| `pre-auto-gc` | `cli::gc::run` | yes |

**Skipped** (with reason):
- `pre-applypatch` / `post-applypatch` / `applypatch-msg` — `git am` is not
  implemented; no firing site. TODO comment in upstream-mapping notes.
- `sendemail-validate` — no `send-email` porcelain.
- `fsmonitor-watchman` — we don't speak fsmonitor.
- `p4-*` — Perforce bridge is out of scope (NON_GOALS Batch A).
- `pre-receive` / `update` / `post-receive` / `post-update` — server-side;
  rustygit doesn't run as a server.

**`--no-verify` flag** added to `commit` and `push`. Matches upstream: skips
`pre-commit` + `commit-msg` (but NOT `prepare-commit-msg`, per githooks(5)).

**Exit-code propagation**:
- Blocking hook non-zero → parent porcelain prints
  `rustygit: <op>: hook '<name>' returned <code>; aborting` and returns
  exit code 1 (git's convention for hook-aborted ops, not 128).
- Non-blocking hook non-zero → warning to stderr; parent op completes
  normally.

**Tests added**:
- `src/hooks.rs::tests` — 13 unit tests covering `HookRunner::from_repo`
  resolution, `core.hooksPath` precedence (absolute + relative), `Ran` /
  `NotPresent` / `Skipped` outcomes, argv/stdin/env propagation,
  `run_with_file`'s single-path shape, exec-bit detection, and the
  `aborts_parent()` / `exit_code()` helpers.
- `tests/hooks_compat.rs` — 15 integration tests using real `git init`
  repos + shell hooks: `pre-commit` success/abort/`--no-verify`,
  `commit-msg` message mutation, `post-commit` runs and tolerates
  non-zero, `prepare-commit-msg` sees the `message` source and runs even
  under `--no-verify`, `pre-push` argv + stdin format, `pre-push`
  failure aborts the push, `core.hooksPath` redirection (incl. proof
  that the default dir is NOT consulted when set), `post-checkout`
  argv `<old> <new> <is-branch>`, non-executable hooks silently
  skipped, `#!/bin/sh` shebang spawns correctly, `pre-auto-gc`
  failure aborts `gc`.

920 passing (was 862, +13 unit + +15 integration; concurrent batches
landed the rest in lib/notes/worktree).

**1-line demo**: `pre-commit` returning 1 aborts `rustygit commit` with
exit code 1 — verified by
`tests::pre_commit_failure_aborts_commit_with_exit_1`.

**Deferred sub-items**:
- Hooks for `git am` (3 hooks above) — slot in when the `am` porcelain
  lands.
- `sendemail-validate` — slot in when `send-email` lands.
- `post-index-change` (mentioned in upstream docs as future-only) — not
  yet a real hook in upstream git; nothing to wire.

---

## Batch H — Notes — ✅ DONE (porcelain + library; `merge` deferred)

**Status**: Done. The eight core porcelain forms ship (`list`, `add`,
`show`, `append`, `copy`, `remove`, `edit`, `prune`) and the on-disk shape
matches upstream git byte-for-byte, so:

- A note written by `rustygit notes add` is readable by `git notes show`.
- A note written by `git notes add` is readable by `rustygit notes show`.
- Crossing the 256-note fanout boundary produces a 2/38 tree that
  `git notes list` walks the same way `rustygit notes list` does.

**New module `src/notes.rs`** (~480 LoC + 8 unit tests):
- `NotesTree` — in-memory representation of one notes commit's tree, with
  all fanout layers collapsed into a flat `HashMap<ObjectId, ObjectId>`
  (target oid → note-blob oid).
- `NotesTree::open(&repo, &ref_name)` — read the current notes commit
  (or start empty if the ref doesn't exist) and recurse through every
  level of `<hex-pair>/<hex-pair>/…` subtrees to collapse the fanout.
- `NotesTree::get` / `set` / `remove` / `iter` / `read_note` / `prune`
  — in-memory mutation API.
- `NotesTree::commit(&repo, message, signer)` — rebuilds the canonical
  fanout tree (depth 0 / 1 / 2 by entry count, mirroring git's heuristic
  thresholds at 256 and 65 536), creates a notes commit on top of the
  previous tip, and transactionally updates the ref with a reflog entry.
  Honors `commit.gpgsign` via an optional `&dyn Signer`.
- `resolve_notes_ref` — precedence chain `--ref` flag > `GIT_NOTES_REF`
  env > `core.notesRef` config > `"refs/notes/commits"`. Short forms
  like `reviewers` expand to `refs/notes/reviewers`.
- `pick_editor` / `edit_text` — `$GIT_EDITOR` → `core.editor` → `$VISUAL`
  → `$EDITOR` → `vi` fallback chain for `notes edit`. Editor exit != 0
  aborts without writing.

**Fanout depths supported**: 0 (flat), 1 (2/38 for sha1, 2/62 for sha256),
2 (2/2/36 for sha1). Reads also tolerate deeper layers via the recursive
`collect_tree`, so a repo whose notes tree was built by upstream git at
any depth opens cleanly.

**New CLI module `src/cli/notes.rs`** (~370 LoC):

| Subcommand | Behavior |
|------------|----------|
| `notes [list] [<obj>]` | Print `<note-oid> <obj-oid>` lines, or the note oid for one object. |
| `notes add [-f] [-m <msg>] [-F <file>] [<obj>]` | Add (or `-f` overwrite) a note. Refuses empty notes unless `--allow-empty`. |
| `notes show [<obj>]` | Stream the note bytes to stdout. |
| `notes append [-m <msg>] [-F <file>] [<obj>]` | Concatenate to the existing note with a blank-line separator. |
| `notes copy [-f] <from> [<to>]` | Copy a note's blob oid from one object to another. |
| `notes remove [--ignore-missing] [<obj>…]` | Delete notes (defaults to HEAD). |
| `notes edit [<obj>]` | Spawn `$EDITOR` on the current note; empty edit removes. |
| `notes prune [-n] [-v]` | Drop notes whose target oid is no longer in the odb. |

`--ref <name>` is a global flag on every subcommand. Reflog messages
mirror upstream wording (`Notes added by 'git notes add'`,
`Notes removed by 'git notes remove'`, `Notes added by 'git notes copy'`,
etc.).

**Deferred sub-items**:
- `notes merge` — strategy-driven three-way merge of notes refs has its
  own merge driver in upstream git (`merge-strategies`, conflict files
  under `NOTES_MERGE_WORKTREE/`). Slot in when users ask.
- `notes get-ref` — trivial one-liner; slot in if needed.
- `--for-rewrite` / `notes.rewrite.<cmd>` — auto-copy notes during rebase /
  amend. Needs the rebase porcelain hook; deferred.

**Tests added**:
- `src/notes.rs::tests` — 8 unit tests covering fanout-depth threshold,
  flat + fanout tree collection, fanout-tree write-then-read round trip,
  depth-1 layout shape at the 300-note threshold, and `resolve_notes_ref`
  precedence (default, short form, full ref).
- `tests/notes_compat.rs` — 10 integration tests (every test gated on
  `has_system_git()` since the oracle path uses real `git`):
  - `notes_add_then_show_round_trip`
  - `notes_append_with_blank_line_separator`
  - `notes_remove_makes_show_fail`
  - `notes_copy_carries_note_across_objects`
  - `notes_list_prints_pairs`
  - `oracle_rustygit_then_git_show` — rustygit writes, git reads.
  - `oracle_git_then_rustygit_show` — git writes, rustygit reads.
  - `notes_ref_targets_alternate_namespace` — `--ref` round-trip, both
    rustygit↔git for `refs/notes/reviewers`.
  - `notes_fanout_interop_with_git` — 300 notes via rustygit, then
    `git notes list` and `rustygit notes list` produce the same target
    set; spot-checked bodies match.
  - `notes_prune_removes_dangling_entries` — delete a loose object file
    from disk, run `notes prune`, the corresponding entry disappears.

920 passing (was 902 with concurrent batches, +18 new: 8 unit + 10 integration).

**1-line oracle demo**: `rustygit notes add -m "x" HEAD` then `git notes
show HEAD` returns `x` — verified by `oracle_rustygit_then_git_show`.

---

## Batch I — Worktrees — ✅ DONE (core subcommands; lock/unlock/move/repair deferred)

**Status**: Done. The four user-facing core subcommands ship — `add`,
`list`, `remove`, `prune` — and the on-disk layout matches upstream git
byte-for-byte, so:

- `cd <linked-worktree>; git log` / `git status` works on a
  rustygit-created secondary worktree.
- `rustygit worktree list` enumerates secondary worktrees that upstream
  git wrote with `git worktree add`.
- `git worktree list` from inside the main repo shows rustygit-created
  worktrees too.
- `git fsck --full` on the main repo after `rustygit worktree add` is
  clean.

**Architectural changes** to `src/repo.rs`:

- New `commondir: PathBuf` field on `Repository`. Equals `gitdir` for a
  single-worktree (non-linked) repo. For a linked worktree, points at
  the main `.git/`.
- `Repository::discover` now follows the `.git`-FILE pointer
  (`gitdir: <abs-path>\n`) — the linked-worktree convention. The
  per-worktree gitdir is `<main>/.git/worktrees/<name>/`.
- `Repository::open` reads the `<gitdir>/commondir` marker if present
  and resolves `<gitdir>/gitdir` (back-pointer to the linked `.git`
  file) to find the real workdir.
- Shared resources (`objects/`, `refs/`, `config`, `packed-refs`,
  `shallow`, `hooks/`) live under `commondir`. Per-worktree resources
  (`HEAD`, `index`, `logs/HEAD`) live under `gitdir`.
- New public accessors: `commondir()`, `is_linked_worktree()`.
- Existing `gitdir()`, `objects_dir()`, `refs_dir()`, `config_path()`
  accessors now return paths grounded in the correct dir (commondir for
  shared, gitdir for per-worktree). All callsites that previously
  called these continue to work — they were already asking for the
  right semantic concept.

**New module `src/cli/worktree.rs`** (~500 LoC + 2 unit tests). Subcommands:

- **`add <path> [<commit-ish>] [-b <branch>] [--detach]`** — creates the
  admin dir at `<main>/.git/worktrees/<sanitized-basename>/` with HEAD
  / commondir / gitdir-back-pointer files, writes the linked
  worktree's `.git` FILE, then calls `checkout_tree` on a fresh
  Repository opened at the admin dir (which inherits commondir from
  the main repo, so objects/refs resolve correctly).
- **`list [--porcelain]`** — prints the main worktree + each linked
  worktree's path / short oid / branch. Porcelain form matches
  upstream's `worktree <path>\nHEAD <oid>\n[branch|detached]\n\n`
  layout.
- **`remove <path>`** — finds the admin dir whose `gitdir` back-pointer
  matches `<path>`, deletes both the workdir and the admin dir.
- **`prune [--dry-run]`** — walks `<main>/.git/worktrees/` looking for
  admin entries whose back-pointer targets a missing directory, prints
  the removal reason, optionally deletes.

**Deferred sub-items**:
- `worktree lock` / `worktree unlock` — admin file `locked` would
  protect against prune; not yet emitted/read.
- `worktree move` — atomic rename of admin entry + on-disk dir.
- `worktree repair` — re-link admin entry to a moved worktree.

**Tests** (9 in `tests/non_goals_worktree.rs`):
- `worktree_list_on_main_only_repo_prints_main`
- `worktree_add_with_b_creates_branch_and_checks_out` — full
  bidirectional oracle: rustygit creates the layout, git recognizes it
  in `worktree list`, `git status` inside the linked tree is clean.
- `rustygit_list_after_git_worktree_add` — reverse oracle: git creates
  the layout, rustygit `worktree list` enumerates it.
- `worktree_remove_drops_admin_and_worktree`
- `worktree_prune_drops_orphaned_admin_entries`
- `worktree_add_refuses_existing_path` — `<path>` must not exist.
- `worktree_list_porcelain_format` — `--porcelain` shape check.
- `rustygit_in_linked_worktree_reads_correct_head` — operating IN a
  linked worktree: rustygit's `rev-parse HEAD` + `status --porcelain`
  resolve through the per-worktree HEAD and the shared object store.
- `cross_oracle_fsck_after_rustygit_add` — `git fsck --full` clean.

Plus 3 unit tests in `cli::worktree::tests` for the basename sanitizer
and the relative-path helper.

---

## Out-of-spec — multi-session, deferred decision

These were in the plan's non-goals but are big enough that we should agree
explicitly before scheduling sessions:

- **Sparse-checkout (non-cone)** — pattern-driven worktree filter. Needs
  attribute-filter scaffolding to share. ~1-2 sessions.
- **Attribute filters (smudge/clean/textconv)** — external program hooks
  during checkout/add. ~1-2 sessions; enables LFS.
- **Submodule porcelain** (`submodule add/update/foreach`) — nested-repo
  work. ~2-3 sessions.
- **Interactive rebase** (`-i`, `--rebase-merges`, `--autosquash`, `--exec`) —
  scripted editor-driven flow + todo-file editing. ~2-3 sessions.
- **Partial clone / promisor remotes** (`--filter=blob:none`) — deferred
  fetch on missing objects. ~3-4 sessions.
- **LFS** — separate transfer protocol on top of attribute filters. ~4-5
  sessions.
- **Windows-specific path normalization** — best-effort exists; pushing to
  parity is its own task. ~1-2 sessions. CI doesn't even include Windows
  today (Linux + macOS only).

---

## Tracking

- [x] A. Cheap rejections (gitweb/gitk/git-gui/git-svn/git-p4/git-instaweb/
       request-pull/mergetools + dumb-HTTP + git:// + protocol v0/v1)
- [x] B. Read-only format support (bitmaps + Bloom + midx bitmaps) —
       acceptance done, optimization use deferred
- [x] C. i18n posture
- [x] D. Bundle URI / packfile URI decline
- [x] E. Stub commands (rerere + replace + filter-branch)
- [x] F. Signing (GPG) — SSH/X.509 deferred
- [x] G. Hooks framework — client hooks shipped; server-side + am/send-email
       hooks deferred
- [x] H. Notes — porcelain + library; `merge` deferred
- [x] I. Worktrees — lock/unlock/move/repair deferred
- [ ] (Out-of-spec: sparse-checkout, attr filters, submodules, rebase -i,
       partial clone, LFS, Windows — scheduled separately)
