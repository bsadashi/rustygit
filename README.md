# rustygit

A from-scratch Rust reimplementation of git's core. Object store, refs,
index, packfiles, plumbing **and** porcelain — all single-binary, no shell
scripts, no Perl, no Tcl/Tk.

## Status

**v0.1.0 — production candidate for the documented scope.** 930+ tests
passing; byte-for-byte oracle compatibility against upstream git on every
on-disk format (loose objects, packfiles, refs, reftable, index v2,
commit-graph, midx, reflog). See [`GO-LIVE.md`](GO-LIVE.md) for the
production sign-off.

## Install

```sh
# from source (today)
cargo install --path .

# crates.io / Homebrew / .deb — see GO-LIVE.md Phase 2 release pipeline
```

## What rustygit covers vs. what it doesn't

See [`COMPAT.md`](COMPAT.md) for the subcommand-by-subcommand compatibility
tier table. Short version:

* **Tier 1 — byte-for-byte match with `git`**: every plumbing command,
  `log`, `diff`, `status`, `show`, `branch`, `commit`, `checkout`,
  `switch`, `restore`, `reset`, `rev-parse`, `cat-file`, `ls-tree`,
  `merge-base`, `notes`, `worktree`, signing, hooks.
* **Tier 2 — semantic match, format may differ**: error messages, color
  output (rustygit uniformly does not colorize today).
* **Tier 3 — rustygit-specific**: `doctor`, `prune-locks`.
* **Out of scope — must use upstream `git`**: submodules, sparse-checkout,
  smudge/clean filters (and therefore LFS), interactive rebase,
  partial clone (`--filter=blob:none`), Windows polish.

## License

Dual-licensed under either Apache-2.0 or MIT at your option.
