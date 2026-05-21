# xtask-compat-corpus

Compatibility-corpus harness for `rustygit` (NON_GOALS C2).

This is a standalone Cargo binary that clones a curated set of public
git repositories with system `git`, then runs a fixed sequence of
read-only operations through both `rustygit` and `git`, asserting
byte-equal stdout and exit codes. Any divergence fails the run and
emits a small `.diff` snippet next to the offending repo.

The crate is intentionally **not** a member of the main rustygit
workspace: its dependencies (`toml`, `serde`) are dev-tooling and we
don't want them in the production binary's audit graph.

## Layout

```
xtask-compat-corpus/
  Cargo.toml          standalone crate; not a workspace member
  src/main.rs         the harness binary
  corpus.toml         data-driven list of repos + operations
  README.md           this file
```

## Run locally

From the repository root:

```bash
cargo build --release
cargo run --manifest-path xtask-compat-corpus/Cargo.toml -- \
    xtask-compat-corpus/corpus.toml target/corpus target/release/rustygit
```

Or from inside this crate:

```bash
cargo run -- corpus.toml ../target/corpus ../target/release/rustygit
```

All three positional args have defaults (`corpus.toml`,
`target/corpus`, `target/release/rustygit`), so the bare `cargo run`
works if you're in the right directory.

## Output

For each `repo × op` pair the harness prints `PASS` / `FAIL` plus
exit codes and divergent-byte count. A `target/corpus/<repo>/<op>.diff`
file is written for every failure containing a small unified-diff
snippet (the first six differing lines). CI uploads these as
artifacts on failure.

A non-zero exit code from the harness means at least one divergence
was observed.

## Add a new repository

Append a `[[repo]]` block to `corpus.toml`:

```toml
[[repo]]
name = "go"
url = "https://github.com/golang/go.git"
shallow = true
depth = 2000
```

`name` becomes the per-repo subdirectory under `target/corpus/`. Keep
`shallow = true` unless a specific op (e.g. `rev-list --count HEAD`)
needs the full history — large public repos clone hundreds of
megabytes and shallow keeps the cache warm and the runs fast.

## Add a new operation

Append an `[[op]]` block. The ONLY requirement is that the output be
deterministic across runs:

```toml
[[op]]
label = "diff-shortstat"
argv = ["diff", "--shortstat", "HEAD~1..HEAD"]
```

Avoid: anything that prints timestamps unformatted, paths with
locale-dependent collation, `--date=human`, or sampled (random) lists.

## Interpret a failure

A failing run prints something like:

```
[git] log-1000                FAIL (rusty=0 git=0 diff=842b)
```

That tells you exit codes matched but stdout differed by 842 bytes.
The diff snippet at `target/corpus/git/log-1000.diff` is the place
to start triaging — it shows the first lines where the two outputs
diverge.

When CI fails, those `.diff` files are uploaded as an artifact
named `compat-corpus-diffs`.

## CI workflow

`.github/workflows/compat-corpus.yml` runs this harness nightly at
06:00 UTC, plus on `workflow_dispatch`. Clones are cached on the
hash of `corpus.toml` so the slow network step is amortized.
