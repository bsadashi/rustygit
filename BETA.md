# rustygit beta

## What "beta" means here

The GA contract for rustygit is byte-equality with upstream git on every
durable on-disk format: loose objects, packfiles, indexes (v2 and v3),
the multi-pack-index, refs and reftable, the commit-graph, and the
reflog. Beta means the binary is **feature-complete for the documented
scope** (M0 through M16; see `ROADMAP.md`) and every format invariant
is enforced by the test suite — but it has not yet seen more than 14
days of real-world traffic across more than a handful of repositories.
Treat it as you would any other 0.x version control tool: usable for
day-to-day work, not load-bearing for irreplaceable state without a
backup.

## Known divergences

The complete subcommand-by-subcommand tier table lives in
[`COMPAT.md`](COMPAT.md). The short risk model that matters
day-to-day:

1. **`.gitattributes` filters are not honored.** Smudge/clean drivers,
   per-pattern `text=auto`, and textconv are skipped. Repos that rely on
   them (notably LFS) will not check out correctly.
2. **`[includeIf]` and `[include]` are silently skipped** with a
   one-time warning. Inline the contents if you depend on them.
3. **Whole feature areas are deferred to a later release:** submodule
   `add`/`update`/`foreach`, partial clone (`--filter=blob:none`), LFS,
   sparse-checkout (cone-mode), and interactive rebase
   (`rebase -i` / `--autosquash`). Non-interactive `rebase` works.

## Acknowledging beta and silencing the banner

On every invocation rustygit emits a one-line banner to **stderr**
reminding you it's beta. To suppress it permanently:

```sh
rustygit config --global rustygit.beta.acknowledged true
```

To suppress it for a single command (useful in scripts and CI):

```sh
rustygit --i-know-this-is-beta status
```

The flag is stripped from argv before clap sees it, so it's transparent
to every subcommand. The banner drops automatically once the build tag
no longer contains `-beta`.

## Filing a bug

Run `rustygit bug-report` and paste the output into a new issue at
<https://github.com/bsadashi/rustygit/issues>. `bug-report` captures
the rustygit version, build target, OS, repo dimensions, and the
recent reflog tail — almost everything we need to reproduce on our end.

**Critical bugs — data loss, silent corruption, or segfault — should
go through [GitHub Security Advisories](https://github.com/bsadashi/rustygit/security/advisories/new)
instead.** Anything that puts a user's commits at risk gets a private
channel first; see `SECURITY.md` for the full disclosure policy.
