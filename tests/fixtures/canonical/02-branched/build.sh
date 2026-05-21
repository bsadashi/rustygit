#!/usr/bin/env bash
# Canonical fixture 02-branched: main + an unmerged `feature` branch.
#
# Both branches share an initial commit. Each has a follow-up commit
# of its own. Exercises branch listing, multi-ref show-ref output, and
# rev-parse against a non-default branch name.
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

# Base.
printf 'shared\n' > shared.txt
git add shared.txt
commit_at "initial commit" "2024-01-01T00:00:00+0000"

# Main advances.
printf 'main change\n' >> shared.txt
git add shared.txt
commit_at "main change" "2024-01-02T00:00:00+0000"

# Branch off the base, advance separately.
git checkout -q -b feature HEAD~1
printf 'feature change\n' > feature.txt
git add feature.txt
commit_at "feature change" "2024-01-03T00:00:00+0000"

# Leave HEAD on main: makes default `rev-parse HEAD` semantically
# unambiguous for the golden files.
git checkout -q main
