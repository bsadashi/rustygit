# GO-LIVE — production sign-off for rustygit v0.1.0

This file is the production-readiness gate. It enumerates the checks that
must hold before a tag is cut, records the evidence from the last
sign-off run, and lists the Phase 2 work that follows after the first
release.

Companion docs: [`COMPAT.md`](COMPAT.md) (subcommand tier table),
[`PROOF.md`](PROOF.md) (the byte-equality evidence),
[`POLISH.md`](POLISH.md) (deferred-from-milestones items),
[`NON_GOALS.md`](NON_GOALS.md) (out-of-scope features, with their
sub-status), [`TESTING.md`](TESTING.md) (test-layer contract).

---

## Phase 1 — release gates (must hold to cut a tag)

Run from a clean checkout. Every gate is a CI job in
`.github/workflows/ci.yml`; this section is what a release engineer reruns
locally before pushing the tag.

| # | Gate                                  | Command                                                       | Pass criterion                          |
|---|---------------------------------------|---------------------------------------------------------------|-----------------------------------------|
| 1 | Release build                         | `cargo build --release`                                       | exit 0, no warnings                     |
| 2 | Formatting                            | `cargo fmt --all -- --check`                                  | exit 0, zero diffs                      |
| 3 | Lint                                  | `RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets` | exit 0                                |
| 4 | Tests (debug)                         | `cargo test --workspace`                                      | all green                               |
| 5 | Tests (release)                       | `cargo test --workspace --release`                            | all green                               |
| 6 | Supply chain — license / bans / sources | `cargo deny check`                                          | exit 0                                  |
| 7 | Supply chain — advisories             | `cargo audit`                                                 | zero vulnerabilities                    |
| 8 | MSRV                                  | `rustup run 1.85 cargo check --workspace --all-targets`       | exit 0 with a fresh lockfile            |

## Sign-off run — 2026-05-15

| Gate                  | Result                                            |
|-----------------------|---------------------------------------------------|
| 1. Release build      | ok — `Finished release profile in 47.62s`         |
| 2. `cargo fmt`        | ok — exit 0                                       |
| 3. clippy `-D warnings` | ok — exit 0                                     |
| 4. tests (debug)      | **941 passed, 0 failed, 0 ignored** across 31 binaries |
| 5. tests (release)    | **941 passed, 0 failed, 0 ignored** across 31 binaries |
| 6. `cargo deny check` | ok — advisories ok, bans ok, licenses ok, sources ok |
| 7. `cargo audit`      | ok — 0 vulnerabilities across 152 dependencies    |
| 8. MSRV (1.85)        | ok — `Finished dev profile in 11.64s`             |

### Drift caught and fixed during this sign-off

- **Clippy warned at 1.95**: 41 errors across the workspace (the CI gate
  is `-D warnings`, so all 41 would have failed CI). The fixes are
  recorded in the working tree. Two were real-bug-shaped:
    - `src/midx.rs` — a redundant `if name_a <= name_b { 0u32 } else { 0u32 }`
      in a test, collapsed to a single constant with the rationale
      preserved as a comment.
    - `src/diff/mod.rs` and `src/pathspec.rs` — `if`-chains where two
      arms had identical bodies, folded with `||`.
- **MSRV reality drift**: the CI MSRV job was pinned to **Rust 1.75**,
  but a transitive dep (idna_adapter via url via ureq) now requires
  `edition2024`, which is only stable in Rust 1.85+. Cargo.toml's
  `rust-version` and the CI matrix were bumped from `1.75` → `1.85`.
- **License allowlist gap**: `cargo deny` rejected webpki-roots's
  `CDLA-Permissive-2.0` license. Added to `deny.toml` with rationale.

---

## Phase 2 — release pipeline (after the tag is cut)

The Phase 1 gates verify *the code is ready*. Phase 2 is the post-tag
release machinery, all wired in `.github/workflows/release.yml`:

1. **Tag push** triggers the workflow on `v[0-9]+.[0-9]+.[0-9]+(-.*)?`.
2. **Binary build — `build` job** for six targets:
    - `x86_64-unknown-linux-musl` (cross, ubuntu-latest)
    - `aarch64-unknown-linux-musl` (cross, ubuntu-latest)
    - `x86_64-apple-darwin` (native, macos-13)
    - `aarch64-apple-darwin` (native, macos-14)
    - `x86_64-pc-windows-msvc` (native, windows-latest)
    - `aarch64-pc-windows-msvc` (native, windows-latest)
3. **Universal macOS — `macos-universal` job** combines the two macOS
   slices with `lipo -create`, producing
   `rustygit-vX.Y.Z-universal-apple-darwin`. Verified with `lipo -info`.
4. **Linux packages**:
    - **`deb` job** runs `cargo-deb --no-build` against the prebuilt
      musl binary for each Linux arch, producing
      `rustygit-vX.Y.Z-amd64.deb` and `rustygit-vX.Y.Z-arm64.deb`.
      Metadata in `[package.metadata.deb]` ships the binary at
      `/usr/bin/rustygit` plus README and COMPAT docs.
    - **`rpm` job** runs `cargo-generate-rpm` for `x86_64` and
      `aarch64`, producing `rustygit-vX.Y.Z-{x86_64,aarch64}.rpm`.
      `auto-req = "no"` since the musl-static binary has no shared
      library deps.
5. **Homebrew — `homebrew-formula` job** writes `Formula/rustygit.rb`
   with version + SHA-256 for each macOS slice, switching by
   `on_arm`/`on_intel`. Emitted as a release artifact so a
   `homebrew-tap` repo can pull it (or it can be PR'd into
   `homebrew-core`).
6. **Signing — every job**: cosign keyless (Fulcio + Rekor) over every
   `.bin` / `.exe` / `.deb` / `.rpm` / universal binary. `.sig` + `.pem`
   land next to each artifact. Transparency log lookups by digest are
   the verification story.
7. **SBOM**: `anchore/sbom-action` emits one CycloneDX JSON SBOM per
   build target.
8. **GitHub Release — `publish` job**: auto-generated notes + every
   binary + `.exe` + `.deb` + `.rpm` + universal mac + `rustygit.rb` +
   `.sha256` + `.sig` + `.pem` + `.sbom.json`.
9. **crates.io publish**: `cargo publish --no-verify` after the version
   guard that asserts the tag's numeric component matches
   `Cargo.toml`'s `version`.

### Still-manual / out of Phase 2

- **Homebrew tap repository / formula PR**: the workflow *generates*
  the formula but does not push it to a separate tap repo. Either (a)
  fork a `homebrew-tap` repo and add a follow-on job that pushes
  `rustygit.rb` to its `Formula/` dir via a deploy token, or (b) open
  the homebrew-core PR by hand on first release.
- **macOS notarization / codesigning** with an Apple Developer cert.
  We sign with cosign (transparency-log) only, so Gatekeeper will
  still warn on first run of the macOS binaries. Adding this requires
  Apple Developer ID secrets in the repo (`APPLE_ID`, `APPLE_PASSWORD`,
  `APPLE_TEAM_ID`, `APPLE_CERT_P12`, `APPLE_CERT_PWD`) and an extra
  `xcrun notarytool submit` step after the lipo job.
- **Windows code signing** with an Authenticode certificate. Same
  shape — cosign-only today; signtool integration needs a real cert.
- **APT/YUM repo hosting**: `.deb` and `.rpm` ship as standalone files
  on the GitHub Release. Distributing them through a proper APT/YUM
  repo (so users can `apt install rustygit` / `dnf install rustygit`
  from a configured source) is a separate hosting story (Cloudsmith,
  packagecloud.io, or self-hosted).
- **scoop / winget / chocolatey** (Windows package managers): same
  story as Homebrew — manifest generation could be added once a
  stable release exists.

---

## What this sign-off does NOT certify

To set expectations correctly for first-week users:

- **Scope**: only the subcommands listed at tier T1 in
  [`COMPAT.md`](COMPAT.md). Features under "Out of scope" in
  [`NON_GOALS.md`](NON_GOALS.md) are *intentionally absent*; users who
  need them should fall back to upstream `git`.
- **Platforms**: macOS and Linux at full porcelain test coverage. The
  Windows artifacts ship as **best-effort** in Phase 2 — the binary
  builds for `x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc`,
  and library unit tests run on Windows x64 in CI, but the porcelain
  integration tests (which exec shell scripts and assume a POSIX
  filesystem) are not run on Windows. Path-normalization,
  case-insensitive-FS, and reflog-rename-rules edge cases are not in
  the Windows test matrix.
- **Performance**: not benchmarked against upstream git in this
  release. Performance regression tests are a Phase 3 item
  ([`TESTING.md`](TESTING.md) §6E).
- **Concurrency**: no concurrent-process safety claims beyond what
  `lockfile.rs` provides for ref / index updates. Two rustygit
  processes writing to the same repo simultaneously have not been
  fuzzed.

---

## How to re-run this gate

```sh
# Phase 1 — full gate
cargo build --release
cargo fmt --all -- --check
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets
cargo test --workspace
cargo test --workspace --release
cargo deny check
cargo audit
# MSRV — needs a fresh lockfile because Cargo.lock v4 isn't readable by 1.85's cargo
mv Cargo.lock Cargo.lock.stash && rustup run 1.85 cargo check --workspace --all-targets && mv Cargo.lock.stash Cargo.lock
```

If any gate fails, the tag does not go out — fix forward, re-run the
whole gate, then push the tag.

---

## Sign-off

| Field    | Value                                  |
|----------|----------------------------------------|
| Version  | v0.1.0                                 |
| Date     | 2026-05-15                             |
| Tests    | 941 / 941 passing (debug + release)    |
| Targets  | macOS arm64 (local), Linux x86_64/arm64 + macOS x86_64 (CI matrix) |
| Approved | rustygit maintainers                   |

Cleared for production tag.
