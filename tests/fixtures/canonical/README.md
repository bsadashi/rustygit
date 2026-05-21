# Canonical fixture repos (NON_GOALS C3)

Hand-built tiny git repos used as oracles by `tests/fixtures_regression.rs`.

We **never** commit `.git` directories — they rot when git's repo
format ships a new version, or when filesystem case-folding differs
between macOS/Linux. Instead each fixture has a `build.sh` that
rebuilds the repo from scratch using the **system** `git` and writes
golden output files alongside.

## Layout

```
canonical/
  01-linear/             3-commit linear history
    build.sh             script to (re)build the repo
    golden/              expected outputs for various read-ops
      log-oneline.txt
      ls-tree-head.txt
      ...
  02-branched/           main + unmerged feature branch
  03-merged/             main with merged feature branch
  04-tagged/             annotated + lightweight tags
  05-deleted-files/      file added then removed
```

## Determinism contract

`build.sh` scripts MUST set:

- `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL`,
  `GIT_AUTHOR_DATE` (ISO 8601 with explicit `+0000`).
- `GIT_COMMITTER_NAME`, `GIT_COMMITTER_EMAIL`,
  `GIT_COMMITTER_DATE`.
- `--no-gpg-sign` on every commit (different runners have different
  default signing configs).
- `core.autocrlf=false`, `core.symlinks=false`, `commit.gpgsign=false`.

With those held constant, the resulting `.git/objects/*` are
byte-identical across runs and golden files are stable.

## Regenerating goldens

```bash
GOLDEN_REGEN=1 cargo test --test fixtures_regression
```

This re-runs every fixture, writes the observed `rustygit` output
to the `golden/` directory, then asserts. Use it after a known-good
behavior change. **Review the diff** before committing.

## Adding a new fixture

1. `mkdir tests/fixtures/canonical/06-mything/`
2. Write `build.sh` following the determinism contract above.
3. Add a `#[test] fn fixture_06_mything()` in
   `tests/fixtures_regression.rs`.
4. `GOLDEN_REGEN=1 cargo test --test fixtures_regression fixture_06`.
5. Review the goldens, commit.
