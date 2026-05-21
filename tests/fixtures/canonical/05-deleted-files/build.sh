#!/usr/bin/env bash
# Canonical fixture 05-deleted-files: a file added then deleted.
#
# Useful for testing log --follow-style flows, diff across deletions,
# and ls-tree on the final HEAD tree (which should NOT contain the
# deleted file).
set -euo pipefail

if [[ $# -lt 1 ]]; then
    echo "usage: $0 <target-dir>" >&2
    exit 2
fi
TARGET="$1"

rm -rf "$TARGET"
mkdir -p "$TARGET"
cd "$TARGET"

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

# Commit 1: keeper.
printf 'keeper\n' > keeper.txt
git add keeper.txt
commit_at "add keeper" "2024-01-01T00:00:00+0000"

# Commit 2: add the temporary file.
printf 'temp\n' > temp.txt
git add temp.txt
commit_at "add temp" "2024-01-02T00:00:00+0000"

# Commit 3: delete the temporary file.
git rm -q temp.txt
commit_at "remove temp" "2024-01-03T00:00:00+0000"
