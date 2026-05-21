#!/usr/bin/env bash
# Canonical fixture 03-merged: main with a merged `feature` branch.
#
# Produces a true merge commit (--no-ff) so `log --oneline` shows the
# merge marker and ls-tree -r HEAD sees the union of both sides.
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
git config merge.ff false

commit_at() {
    local msg="$1"
    local date="$2"
    GIT_AUTHOR_DATE="$date" GIT_COMMITTER_DATE="$date" \
        git commit -q --no-gpg-sign -m "$msg"
}

printf 'base\n' > shared.txt
git add shared.txt
commit_at "initial commit" "2024-01-01T00:00:00+0000"

git checkout -q -b feature
printf 'feature work\n' > feature.txt
git add feature.txt
commit_at "feature work" "2024-01-02T00:00:00+0000"

git checkout -q main
GIT_AUTHOR_DATE="2024-01-03T00:00:00+0000" \
GIT_COMMITTER_DATE="2024-01-03T00:00:00+0000" \
    git merge -q --no-ff --no-gpg-sign -m "merge feature into main" feature
