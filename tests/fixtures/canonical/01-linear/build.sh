#!/usr/bin/env bash
# Canonical fixture 01-linear: a 3-commit linear history.
#
# Determinism contract (see ../README.md): all dates, names, and emails
# are pinned so the resulting object ids are byte-identical across runs
# and machines.
set -euo pipefail

if [[ $# -lt 1 ]]; then
    echo "usage: $0 <target-dir>" >&2
    exit 2
fi
TARGET="$1"

# Fresh tree.
rm -rf "$TARGET"
mkdir -p "$TARGET"
cd "$TARGET"

# Pinned identity + environment. Don't rely on the runner's gitconfig.
export GIT_AUTHOR_NAME="Fixture Author"
export GIT_AUTHOR_EMAIL="fixture@example.invalid"
export GIT_COMMITTER_NAME="Fixture Committer"
export GIT_COMMITTER_EMAIL="fixture@example.invalid"
export GIT_CONFIG_GLOBAL=/dev/null
export GIT_CONFIG_SYSTEM=/dev/null

git init -q -b main
git config commit.gpgsign false
git config tag.gpgsign false
git config core.autocrlf false
git config core.symlinks false

commit_at() {
    local msg="$1"
    local date="$2"
    GIT_AUTHOR_DATE="$date" GIT_COMMITTER_DATE="$date" \
        git commit -q --no-gpg-sign -m "$msg"
}

# Commit 1: initial README.
printf 'hello\n' > README.md
git add README.md
commit_at "initial commit" "2024-01-01T00:00:00+0000"

# Commit 2: add a second file.
printf 'lorem ipsum\n' > body.txt
git add body.txt
commit_at "add body" "2024-01-02T00:00:00+0000"

# Commit 3: modify README.
printf 'hello\nworld\n' > README.md
git add README.md
commit_at "extend README" "2024-01-03T00:00:00+0000"
