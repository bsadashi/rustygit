#!/usr/bin/env bash
# Canonical fixture 04-tagged: linear history with both kinds of tags.
#
# Includes a lightweight tag (just a ref) and an annotated tag (its
# own object). Exercises show-ref / for-each-ref tag output.
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

printf 'one\n' > a.txt
git add a.txt
commit_at "first" "2024-01-01T00:00:00+0000"

# Lightweight tag.
git tag v0.1.0-light

printf 'two\n' > b.txt
git add b.txt
commit_at "second" "2024-01-02T00:00:00+0000"

# Annotated tag (its own object).
GIT_AUTHOR_DATE="2024-01-02T01:00:00+0000" \
GIT_COMMITTER_DATE="2024-01-02T01:00:00+0000" \
    git tag -a -m "release 0.2.0" v0.2.0
