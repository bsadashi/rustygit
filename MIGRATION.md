# Migrating from upstream git to rustygit

This is the doc to read first if you're swapping `git` for `rustygit` on
a machine you actually use. It walks the half-dozen places where the two
binaries diverge in practice and gives you a one-line escape hatch for
the cases rustygit can't yet handle.

## Why switch?

Rust safety guarantees on a tool that touches your commit history is
the headline reason. Beyond that: a single static binary (no Perl, no
shell, no Tcl/Tk), modern defaults (UTF-8 everywhere, SHA-256 ready,
reftable on by request), and a strict no-silent-failures policy in the
analysis paths. **rustygit is not necessarily faster than upstream git
yet** — that's not the contract. The contract is byte-equality with
upstream on every on-disk format and a saner Rust-native implementation
to build on.

## Step 1 — install

See [`README.md`](README.md). The short version is `cargo install
--path .` from a checkout, with packaged binaries (Homebrew, `.deb`,
`.rpm`) shipping per `GO-LIVE.md` Phase 2.

## Step 2 — identity

The day-one stumble is almost always `user.name` / `user.email` not
being read where you expected. rustygit reads, in order: system
config, `$XDG_CONFIG_HOME/git/config` (defaulting to
`~/.config/git/config`), `~/.gitconfig`, then the per-repo
`<gitdir>/config`. If you've only set identity via a tool that wrote
to one of those locations, it'll be picked up. If not:

```sh
rustygit config --global user.name "Your Name"
rustygit config --global user.email "you@example.com"
```

Run `rustygit doctor --import-config` to get a report of which keys in
your existing `~/.gitconfig` rustygit honors, ignores, or refuses.

## Step 3 — aliases work (mostly)

`[alias]` entries in `~/.gitconfig` are read and expanded before clap
sees argv, so `rustygit st` (with `alias.st = status`) routes the same
way `git st` would. **Exception:** aliases starting with `!` (shell
execution) are **refused** at expansion time, with a clear error. This
is intentional — silently running arbitrary shell from a config file
is the kind of footgun rustygit doesn't ship.

Workaround: move the shell-y alias into your shell's rc file.

```sh
# in ~/.zshrc or ~/.bashrc
rgs() { rustygit status "$@" && rustygit diff --stat "$@"; }
```

## Step 4 — things that differ

Five places rustygit doesn't currently match upstream:

* **`.gitattributes` filters.** Smudge/clean drivers, per-pattern
  `text=auto`, textconv: none honored today. Workaround: pre-process
  files manually before committing, or stay on upstream git for repos
  that rely on this.
* **`[includeIf]` / `[include]`.** Silently skipped with a one-time
  warning. Workaround: inline the referenced content into the parent
  config file.
* **Submodules.** The `gitlink` mode (160000) is preserved correctly
  in trees, so a repo with submodules clones and checks out cleanly,
  but `rustygit submodule add` / `update` / `foreach` are not
  implemented. Use upstream git for those commands.
* **Partial clone.** `--filter=blob:none` is not supported. rustygit
  will perform a full clone instead of erroring out, but you don't get
  the bandwidth savings.
* **Interactive rebase.** `rebase -i` and `--autosquash` are not
  supported. The non-interactive flow (`rustygit rebase <upstream>`)
  works.
* **LFS.** Not supported. Combined with the `.gitattributes` filter
  gap above, LFS repos must stay on upstream git.
* **Sparse-checkout** (cone-mode). Not supported.

## Step 5 — Windows users

Windows support in `v0.1.x` is **best-effort**, not load-bearing.
Concretely:

* **Symlinks.** Checkout refuses to materialize symlinks unless
  `core.symlinks = false` is set in the repo config. This is a hard
  refusal, not a silent skip.
* **Non-UTF-8 paths.** rustygit refuses non-UTF-8 path bytes with a
  clear error rather than doing a lossy conversion that would change
  the tree's identity.
* **`core.autocrlf`.** The literal values `true`, `input`, and `false`
  are honored. `.gitattributes`-driven `text=auto` is **not** honored
  (this is the same gap as Step 4, item 1).

If your daily driver is Windows and you're hitting any of these, stay
on upstream git and revisit when v0.2 lands.

## Step 6 — the escape hatch alias

For machines where you want rustygit by default but a clean fallback
to upstream git on incompatible repos, drop this into your shell rc:

```sh
# Falls back to upstream git on rustygit-incompatible repos.
alias gitsafe='if grep -q RUSTYGIT_INCOMPAT .git/.rustygit-flags 2>/dev/null; then git "$@"; else rustygit "$@"; fi'
```

rustygit doesn't yet write `.git/.rustygit-flags` itself — that's a
forward-compatibility marker for a later release that'll auto-detect
known-incompatible features. For now, if you want a per-repo opt-out:

```sh
mkdir -p .git
echo RUSTYGIT_INCOMPAT > .git/.rustygit-flags
```

and `gitsafe` will route that repo to upstream git.

## Reporting bugs

Run `rustygit bug-report`, paste the output into a new issue at
<https://github.com/bsadashi/rustygit/issues>. For anything in the
data-loss / silent-corruption / segfault class, use [GitHub Security
Advisories](https://github.com/bsadashi/rustygit/security/advisories/new)
instead so the discussion is private until a fix ships.
