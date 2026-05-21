# Testing Strategy for rustygit

This document is the authoritative spec for how we test rustygit. Every layer
below is a *contract* — when a layer's tests are green, callers can rely on
the guarantees that layer claims. The load-bearing principle is **oracle
comparison**: every behavior must be verified against the system `git` binary
on the same input. Where the two disagree, rustygit is wrong by definition
(this is a port, not an alternative).

## 0. Why this document exists

We have ~720 tests today (as of M15) and they cover a lot. But the strategy
has been ad-hoc per-milestone. This document:

1. Names the test layers explicitly so we know what each one covers.
2. Defines the oracle-comparison pattern that every command must follow.
3. Enumerates per-command coverage so we can see at a glance what's tested
   and what's not.
4. Lists the gaps that exist today (so they're tracked, not hidden).

If you add a new command or change one, you update this document.

---

## 1. Test layers (what we have, what each covers)

| Layer | Location | Speed | Cardinality (today) | Purpose |
|---|---|---|---|---|
| **L1: Unit** | `src/**/*.rs` `#[cfg(test)] mod tests` | < 5s total | ~602 | Pure logic; parsers; sort orders; algorithm correctness. No I/O beyond `tempfile::TempDir`. |
| **L2: Compat** | `tests/m<N>_compat.rs` | < 30s total | ~100+ | Oracle comparison: drive identical inputs through `rustygit` and `git`, byte-diff outputs and on-disk state. Network-gated tests skip if no internet. |
| **L3: Self-host** | `tests/self_host.rs` (TODO #1) | < 60s | 0 today | rustygit manages its own source tree end-to-end (init → add → commit → push → clone → merge → reset). |
| **L4: Real-world stress** | scripts under `scripts/stress/` (TODO #2) | minutes | 0 today | Pick public repos with known properties (deep history, criss-cross merges, large blobs, many refs) and exercise every read path. |
| **L5: Fuzz / property** | `tests/fuzz/` (TODO #3) | minutes | 0 today | Random inputs to parsers (pack format, idx, commit-graph, midx, refs, index) — never panic, return clean errors. |

Today's strength: L1 + L2. The pyramid is unit-heavy at the bottom and
compat-heavy at the next layer. The gaps are L3 (self-host) and L4
(real-world stress, beyond the ad-hoc one I ran after M8). L5 fuzz has been
deferred.

---

## 2. The oracle-comparison pattern

Every command's behavior must be verifiable as one of:

### 2a. **Stdout byte-identity**
`rustygit <cmd> <args>` stdout BYTE-EQUAL to `git <cmd> <args>` for the same
repo state and environment. Examples: `log`, `diff`, `cat-file`, `ls-tree`,
`show-ref`, `verify-pack -v`, `merge-base`.

```rust
let our = rustygit(&["log", "--oneline"], &repo).stdout;
let theirs = git(&["log", "--oneline"], &repo).stdout;
assert_eq!(our, theirs);
```

### 2b. **On-disk byte-identity**
After `rustygit <cmd>`, the resulting `.git` directory's content for the
files we BOTH write must byte-match. Files git writes that we don't (e.g.
hook samples) are documented as acceptable divergence.

```rust
let our_dir = run_rustygit_init(&tmp1);
let git_dir = run_git_init(&tmp2);
for (path, content) in snapshot_dir(&our_dir) {
    if !path.starts_with("hooks/") {
        assert_eq!(content, read_from(&git_dir, &path));
    }
}
```

### 2c. **Object oid equality**
A commit, tree, or blob produced by rustygit must have the EXACT SAME SHA-1
(or SHA-256) as the corresponding one produced by git for identical inputs.
This is the strongest possible compatibility guarantee — same bytes, same hash.

```rust
let our_oid = rustygit(&["commit", "-m", "x"], &our_repo);
let git_oid = git(&["commit", "-m", "x"], &git_repo);
assert_eq!(our_oid, git_oid);  // only works with fixed author/date env
```

### 2d. **Cross-binary readability**
rustygit-written artifacts must be readable by git, AND git-written artifacts
must be readable by rustygit. Both directions.

```rust
// rustygit → git
rustygit(&["hash-object", "-w", "--stdin"], ..., bytes);
assert!(git(&["cat-file", "-p", &our_oid], ...).success());

// git → rustygit
git(&["commit", "-m", "x"], ...);
assert!(rustygit(&["log", "--oneline"], ...).success());
```

### 2e. **Exit code equivalence**
Match git's exit codes: 0 success, 1 expected failure (e.g. `diff
--exit-code` finding differences), 128 fatal, 129 usage.

```rust
let r = rustygit(&["cat-file", "-e", &absent_oid], &repo);
assert_eq!(r.status.code(), Some(1));
```

### 2f. **Format validation by git**
Anything we write to a binary format must be accepted by git's own verifier.
The strongest gate: `git fsck --full` must exit 0 after any rustygit
operation that touches the object store.

```rust
rustygit(&["commit", "-m", "x"], &repo);
assert!(git(&["fsck", "--full"], &repo).status.success());
```

For specific formats:
- packs → `git verify-pack -v`
- commit-graph → `git commit-graph verify`
- multi-pack-index → `git multi-pack-index verify`

---

## 3. Per-command coverage matrix

Each command needs every applicable test type below. The matrix shows what we
test today; cells with **gap** mark intentional or accidental holes.

Legend: ✓ = present; **gap** = missing; n/a = doesn't apply.

| Command         | 2a stdout | 2b disk | 2c oid | 2d round-trip | 2e exit | 2f fsck |
|-----------------|:--:|:--:|:--:|:--:|:--:|:--:|
| `init`          | ✓ | ✓ | n/a | n/a | ✓ | ✓ |
| `hash-object`   | ✓ | ✓ | ✓ | ✓ | **gap** | ✓ |
| `cat-file`      | ✓ | n/a | n/a | ✓ | ✓ | n/a |
| `ls-tree`       | ✓ | n/a | n/a | ✓ | n/a | n/a |
| `update-ref`    | n/a | ✓ | n/a | ✓ | **gap** | n/a |
| `show-ref`      | ✓ | n/a | n/a | ✓ | n/a | n/a |
| `symbolic-ref`  | ✓ | ✓ | n/a | ✓ | **gap** | n/a |
| `rev-parse`     | ✓ | n/a | n/a | n/a | ✓ | n/a |
| `add`           | n/a | ✓ | n/a | ✓ | ✓ | ✓ |
| `write-tree`    | ✓ | n/a | ✓ | ✓ | n/a | n/a |
| `commit-tree`   | ✓ | n/a | ✓ | ✓ | n/a | ✓ |
| `commit`        | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `log`           | ✓ | n/a | n/a | n/a | ✓ | n/a |
| `status`        | ✓ (porcelain) | n/a | n/a | n/a | ✓ | n/a |
| `rm`            | n/a | ✓ | n/a | n/a | ✓ | ✓ |
| `mv`            | n/a | ✓ | n/a | n/a | ✓ | ✓ |
| `diff`          | ✓ | n/a | n/a | n/a | ✓ | n/a |
| `diff-tree`     | ✓ | n/a | n/a | n/a | ✓ | n/a |
| `diff-index`    | **gap** | n/a | n/a | n/a | **gap** | n/a |
| `diff-files`    | **gap** | n/a | n/a | n/a | **gap** | n/a |
| `branch`        | ✓ | ✓ | n/a | ✓ | ✓ | n/a |
| `checkout`      | ✓ | ✓ | n/a | n/a | ✓ | ✓ |
| `switch`        | ✓ | ✓ | n/a | n/a | ✓ | ✓ |
| `restore`       | n/a | ✓ | n/a | n/a | ✓ | ✓ |
| `reset`         | ✓ | ✓ | n/a | n/a | ✓ | ✓ |
| `verify-pack`   | ✓ | n/a | n/a | ✓ | ✓ | n/a |
| `unpack-objects`| ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `clone` (local) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `clone` (https) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `clone` (ssh)   | **gap** | **gap** | **gap** | **gap** | **gap** | **gap** |
| `pack-objects`  | ✓ | n/a | n/a | ✓ | n/a | ✓ |
| `repack`        | n/a | ✓ | n/a | ✓ | n/a | ✓ |
| `gc`            | n/a | ✓ | n/a | n/a | n/a | ✓ |
| `ls-remote`     | ✓ | n/a | n/a | n/a | ✓ | n/a |
| `fetch`         | **gap** | **gap** | **gap** | **gap** | ✓ | **gap** |
| `pull` (stub)   | n/a | n/a | n/a | n/a | ✓ | n/a |
| `push` (local)  | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `push` (https)  | **gap** | **gap** | **gap** | **gap** | **gap** | **gap** |
| `push` (ssh)    | **gap** | **gap** | **gap** | **gap** | **gap** | **gap** |
| `merge`         | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `merge-base`    | ✓ | n/a | n/a | n/a | ✓ | n/a |
| `merge-tree`    | ✓ | n/a | ✓ | ✓ | n/a | ✓ |
| `cherry-pick`   | ✓ | ✓ | **gap** | n/a | ✓ | ✓ |
| `rebase`        | ✓ | ✓ | n/a | n/a | ✓ | ✓ |
| `reflog`        | ✓ | n/a | n/a | n/a | ✓ | n/a |
| `commit-graph`  | n/a | ✓ | n/a | ✓ | ✓ | ✓ |
| `multi-pack-index` | n/a | ✓ | n/a | ✓ | ✓ | ✓ |
| `blame` (M16)   | ⏳ | n/a | n/a | n/a | ⏳ | n/a |
| `fsck` (M16)    | ⏳ | n/a | n/a | n/a | ⏳ | n/a |
| `bisect` (M16)  | ⏳ | ✓ | n/a | n/a | ⏳ | n/a |

⏳ = M16 in flight, will be filled by the new agent tests.

---

## 4. Concrete test catalog

For commands where the test count is currently low, the canonical scenarios
that MUST be covered:

### `init`
- Default (`init .`) byte-matches git's layout (modulo hooks/)
- `--bare`
- `--initial-branch=main`
- `--object-format=sha256`
- `init` in a directory that doesn't exist (creates it)
- `init` in an existing repo (reinitialize message)

### `commit` / `commit-tree`
- First commit (root, no parents)
- Linear follow-up commit
- Merge commit (two parents)
- Commit with empty message → error (or `--allow-empty-message`)
- Commit with non-ASCII author name (UTF-8 in identity)
- Commit with `GIT_AUTHOR_DATE` honored
- Commit with no `user.name`/`user.email` → error
- Commit-tree with explicit `-p` parents (octopus, 3+)

### `diff`
- Workdir vs HEAD (no flag)
- `diff --cached` (index vs HEAD)
- `diff <oid>` (workdir vs oid)
- `diff <a> <b>` (two-tree)
- Pure addition / pure deletion / modify / mode change / type change
- Binary file (NUL in first 8000 bytes)
- File without trailing newline
- Multiple hunks per file
- Context size `-U <n>`

### `merge`
- Fast-forward
- Already-up-to-date
- Clean 3-way (disjoint changes)
- Content conflict (workdir has markers, MERGE_HEAD recorded, exit 1)
- Modify/delete conflict
- Add/add same content (clean)
- Add/add different content (conflict)
- Mode-change conflict
- `--ff-only` refuses non-FF
- Octopus merge (3+ parents) — currently NOT supported; document as gap

### `push` (local)
- First push (creates ref)
- Fast-forward update
- Non-FF refusal
- `--force` override
- Delete `:<ref>`
- Multiple refs in one command
- Round-trip: push → clone the bare → log matches source

### `clone` (https)
- Real github.com clone byte-fsck-matches
- `--no-checkout`
- Refuses non-empty destination
- Auth retry (TODO — needs credential helper integration test)

### `rebase`
- Empty rebase (up to date)
- Fast-forward rebase
- Clean replay of N commits
- Conflict mid-rebase → state saved, exit 1
- `--continue` after resolving
- `--abort` restores HEAD
- Empty commit during rebase skipped
- `--onto <newbase>`

### `cherry-pick`
- Clean apply
- Conflict → CHERRY_PICK_HEAD + markers
- `--abort` restores HEAD
- Multi-commit (TODO — currently single only)

---

## 5. The compat-test harness pattern

Every `tests/m<N>_compat.rs` file follows this pattern. It's worth
standardizing because it's the single most repeated piece of code in the
test suite.

```rust
mod common;

use std::path::Path;
use assert_cmd::Command as AssertCmd;
use common::{git, has_system_git};
use tempfile::TempDir;

/// Run `rustygit <args>` with deterministic env (author/committer/date fixed).
fn rustygit(args: &[&str], cwd: &Path) -> std::process::Output {
    AssertCmd::cargo_bin("rustygit")
        .unwrap()
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")     // fixes oids
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")  // ditto
        .output()
        .unwrap()
}

#[test]
fn behavior_byte_matches_git() {
    if !has_system_git() { return; }
    let tmp = TempDir::new().unwrap();
    // Set up identical state in tmp.
    // Drive identical operation in lockstep.
    // diff outputs.
    let ours = rustygit(&["the-command"], tmp.path());
    let theirs = git(&["the-command"], tmp.path());
    assert_eq!(ours.stdout, theirs.stdout);
}
```

**Key invariant**: fixed `GIT_AUTHOR_DATE` and `GIT_COMMITTER_DATE` are
mandatory for oid-equality tests. Without them, every commit has a different
timestamp and oids will diverge.

---

## 6. Gaps to address (prioritized)

Numbered for tracking — each becomes a follow-up task:

### A. Self-host test suite (high value, low effort)
Build a single `tests/self_host.rs` that does:
1. `rustygit init` a fresh temp repo
2. `rustygit add` rustygit's own `src/` + `tests/` + `Cargo.toml`
3. `rustygit commit -m "self-host"`
4. `rustygit clone <self> /tmp/clone1`
5. `rustygit branch feature` in clone1, make a commit, push to a bare repo
6. `rustygit clone <bare> /tmp/clone2`
7. `rustygit log` on clone2 sees the commit
8. `rustygit merge`/`rebase` between branches
9. `git fsck --full` on every output

This single test exercises every porcelain command end-to-end as an
integration smoke. ~150 lines.

### B. Workflow byte-diff test (high value, medium effort)
A `tests/workflow_compat.rs` that drives the same sequence of 30 operations
through both rustygit and git side-by-side, asserting:
- Each command's stdout matches
- Each command's exit code matches
- After each command, the resulting `.git` snapshot (refs + index) matches

This is the strongest single test we could write. ~500 lines.

### C. Coverage matrix gaps (per the table in §3)
- `diff-index` / `diff-files`: add explicit stdout-byte-match tests
- `hash-object` / `update-ref` / `symbolic-ref`: add exit-code tests
- `cherry-pick`: add oid-equality test (with fixed date env)
- `fetch` (network): add round-trip test against `octocat/Hello-World`
- `push` (https): add a github push test gated on a deploy key being
  available (or skip in CI)
- `clone` / `push` (ssh): add tests gated on `SSH_TEST_URL` env var

### D. Fuzz tests for binary parsers (medium value, medium effort)
Use `cargo fuzz` or hand-rolled random-input loops for:
- `Index::parse` — random bytes never panic, return clean error
- `PackFile::open` — corrupted pack files
- `IdxFile::open` — corrupted idx
- `CommitGraph::open` — corrupted graph
- `MultiPackIndex::open` — corrupted midx
- `Commit::parse` — malformed commit object
- `Tree::parse` — malformed tree

Run for ~5 minutes each in CI nightly job; failures saved to `tests/fuzz_corpus/`.

### E. Performance regression tests (low priority)
Tag-and-time a small benchmark suite using `cargo bench` or
`criterion`. Track:
- `init` (should be < 50ms)
- `commit` of 1000 files (should be < 1s)
- `clone` of `octocat/Hello-World` (should be < 5s)
- `log` walk of 1000 commits (< 100ms)

Failure mode: regression > 2x on any benchmark.

### F. Cross-platform CI matrix (high priority once we have a release branch)
Run the whole suite on:
- macOS (current dev platform)
- Linux x86_64
- Linux aarch64 (Apple Silicon servers, Raspberry Pi)
- Windows (best-effort; many edge cases around case-insensitive FS and
  filemode probing)

The status output (`status_compat.rs`) will surface Windows-specific
divergences first.

---

## 7. Running the test suite

### Local development
```bash
# Fast unit-only loop while iterating
cargo test --lib

# Full sweep (debug)
cargo test

# Full sweep (release — catches optimization-mode issues)
cargo test --release

# Single milestone
cargo test --test m13_compat

# With output (don't suppress println!)
cargo test -- --nocapture

# Single test by name
cargo test merge_clean_non_ff_creates_merge_commit -- --exact
```

### Environment requirements
- `git --version >= 2.40` (we use `--object-format=sha256` and other modern flags)
- For network tests: HTTPS access to `github.com` (or the test sees a connection
  error and skips cleanly)
- For SSH tests: `SSH_TEST_URL` env var set to a clonable SSH URL

### Skipping vs failing
Network and system-dependent tests SKIP cleanly when prerequisites aren't
available; they don't false-fail. Pattern:
```rust
if !has_system_git() { return; }   // not an assertion; clean skip
```

### Determinism
- Fixed env vars for author/committer name/email/date in every compat test
- Tests run in isolated `TempDir`s (no shared state)
- `cargo test` runs in parallel by default; every test must be parallel-safe
- Random tests use seeded PRNGs (`seed = 42` convention)

---

## 8. CI

We don't have CI yet. When we do, the suggested matrix:

| Job | What | Frequency |
|---|---|---|
| `lint` | `cargo clippy --all-targets -- -D warnings` | every PR |
| `test-debug` | `cargo test` | every PR |
| `test-release` | `cargo test --release` | every PR |
| `test-network` | network-gated tests with retry-on-flake | every PR |
| `test-ssh` | SSH tests with deploy key from CI secrets | every PR |
| `self-host` | self-host workflow test | every PR |
| `fuzz` | fuzz tests, 5 min each parser | nightly |
| `benchmark` | benchmark suite, regression detection | nightly |
| `windows` | Windows-only test subset | weekly |

---

## 9. The "complete" bar

rustygit is "1.0 ready" when:

- [ ] L1 unit tests: every module has tests for its public API. Current.
- [ ] L2 compat tests: every cell in §3's matrix is ✓ (no gaps). Current with gaps documented.
- [ ] L3 self-host: the §6.A test exists and passes. **Missing.**
- [ ] L4 stress: clone + walk a 1000+ commit repo (e.g. tinygrad, ~80 commits;
      or git itself with a full clone), every read works, output matches git.
      **Partial; ran manually after M8 but not in the test suite.**
- [ ] L5 fuzz: §6.D parsers fuzz-clean for 1 hour each. **Missing.**
- [ ] Cross-platform: Linux ✓ and Windows best-effort. **Untested off macOS.**
- [ ] CI matrix: every PR runs §8's jobs. **None today.**

The path from M16-complete to 1.0-ready is the work in §6. Estimated: 1-2
dedicated sessions for §6.A + §6.B; another 1 for fuzz; CI setup is a few
hours.
