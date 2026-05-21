# Homebrew tap setup (NON_GOALS B5)

This is the one-time operational runbook for hooking `release.yml`'s
already-generated Homebrew formula into a distribution channel users can
install from. No source code change is required for this step — just
GitHub repo creation, a deploy token, and a workflow secret.

After running this, end-users install via:

```bash
brew tap bsadashi/rustygit
brew install rustygit
```

## Why a separate repo?

Homebrew taps are GitHub repositories matching the pattern
`<github-user>/homebrew-<tap-name>`. The formula file lives at
`Formula/rustygit.rb` inside that repo. We CANNOT publish from
`rustygit` directly — the formula has to live in a repo named
`homebrew-rustygit` so `brew tap bsadashi/rustygit` (which expands to
`github.com/bsadashi/homebrew-rustygit`) resolves correctly.

The alternative is the **homebrew-core** PR path. Don't take that until
we have meaningful adoption — their reviewer queue is slow and they
expect a track record of releases first.

## Step 1 — Create the tap repo

On GitHub, create a new public repository named `homebrew-rustygit`
under the `bsadashi` account (or whichever maintainer owns this).
Initialize with a README and Apache-2.0 license.

Clone locally and bootstrap with a minimal `Formula/` directory:

```bash
gh repo create bsadashi/homebrew-rustygit --public --description "Homebrew tap for rustygit (Rust port of git)" --license=apache-2.0
git clone https://github.com/bsadashi/homebrew-rustygit.git
cd homebrew-rustygit
mkdir Formula
# Placeholder so the next `release.yml` run has a directory to write into.
touch Formula/.gitkeep
git add Formula/.gitkeep
git commit -m "scaffold: Formula directory for rustygit"
git push origin main
```

## Step 2 — Generate a deploy token

The release workflow needs WRITE access to push the formula to the tap
repo. Two options:

### Option A — Fine-grained GitHub PAT (recommended)

1. Visit https://github.com/settings/personal-access-tokens/new.
2. Resource owner: `bsadashi`.
3. Repository access: "Only select repositories" → pick
   `bsadashi/homebrew-rustygit`.
4. Permissions: under "Repository permissions":
   - **Contents**: Read and write.
   - **Metadata**: Read-only (default; required).
5. Expiration: 1 year. Calendar this for renewal.
6. Generate, copy the token (`github_pat_…`).

### Option B — Deploy key (more setup, longer-lived)

1. Generate an ed25519 keypair locally:
   `ssh-keygen -t ed25519 -f ./homebrew-rustygit-deploy -N ""`.
2. Add the **public** key (`*.pub`) to
   `https://github.com/bsadashi/homebrew-rustygit/settings/keys/new`
   with write access.
3. Use the **private** key in the workflow (read from secret as
   `SSH_PRIVATE_KEY`, set up via `webfactory/ssh-agent@v0.9.0`).

Option A is simpler and what `release.yml`'s `homebrew-formula` step
expects (it currently uses `GITHUB_TOKEN` for cross-repo writes which
DOESN'T work — needs the PAT). See Step 4 for the workflow tweak.

## Step 3 — Store the token as a workflow secret

In `bsadashi/rustygit`'s repository settings:

1. Go to **Settings → Secrets and variables → Actions → New repository secret**.
2. Name: `HOMEBREW_TAP_TOKEN`.
3. Value: the PAT from Step 2 (or paste the ed25519 private key if you
   chose Option B).
4. Save.

## Step 4 — Wire the workflow step

Open `.github/workflows/release.yml`. Find the `homebrew-formula` job
(it should already exist per the existing `release.yml`). The push step
needs to use `HOMEBREW_TAP_TOKEN` and target the tap repo:

```yaml
- name: Push formula to homebrew tap
  env:
    HOMEBREW_TAP_TOKEN: ${{ secrets.HOMEBREW_TAP_TOKEN }}
    VERSION: ${{ github.ref_name }}    # the v0.1.0 tag
  run: |
    set -euo pipefail
    git clone "https://x-access-token:${HOMEBREW_TAP_TOKEN}@github.com/bsadashi/homebrew-rustygit.git" tap
    cp dist/rustygit.rb tap/Formula/rustygit.rb
    cd tap
    git config user.name "rustygit-release-bot"
    git config user.email "release-bot@bsadashi.dev"
    git add Formula/rustygit.rb
    git diff --cached --quiet || git commit -m "rustygit ${VERSION}"
    git push origin main
```

**Security note:** the token is read from `secrets.*` (server-side
substitution) into an env var, NOT interpolated into the shell command
string directly. This avoids the workflow-injection class of vulns.
Don't `echo` the token in any debug step.

## Step 5 — Smoke test the install path

After the next tagged release publishes (e.g. `v0.1.0-beta.1`), verify
on a clean macOS box:

```bash
brew tap bsadashi/rustygit
brew install rustygit
rustygit --version
which rustygit  # should resolve to /opt/homebrew/bin/rustygit on Apple Silicon

# Verify the shell completion + manpage got installed:
brew test rustygit  # exercises the formula's `test do` block
man rustygit | head -5
```

## Optional: `--install-as-git` alias

Per the user-confirmed launch posture (both binaries shipped; opt-in
drop-in), provide a post-install hook that prints how to set up the
`git` alias:

```ruby
# In Formula/rustygit.rb, inside the install block:
def caveats
  <<~EOS
    rustygit installs as `rustygit`. If you want it to take over the
    name `git` on your shell, add to your shell rc:
        alias git=rustygit

    Or, for a system-wide symlink (requires shell rehash):
        sudo ln -sfn #{HOMEBREW_PREFIX}/bin/rustygit #{HOMEBREW_PREFIX}/bin/git
  EOS
end
```

This is a NO-OP at install time — users see the caveat once when they
install. Add to the generated formula template; `release.yml`'s
`homebrew-formula` job (which builds the formula string) should
include this block.

## Status

This is a Phase-2 GA prerequisite — not a code change, so it doesn't
gate the v0.1.0-beta.1 tag. We can tag and release via direct GitHub
Releases first, then add the tap any time. Recommended timing: after
the first 14-day public-beta window when we know the formula generation
works end-to-end.
