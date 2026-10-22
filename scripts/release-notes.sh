#!/usr/bin/env bash
# Prints the notes of release VERSION: its section of CHANGELOG.md, under
# `## [VERSION]`, without the heading, up to the next release's. Fails if
# there is none, or it is empty, so that no release is published without
# saying what changed: `scripts/release-notes.sh 0.15.0`.

set -euo pipefail
cd "$(dirname "$0")/.."

version=${1:?usage: scripts/release-notes.sh VERSION}
notes=$(awk -v heading="## [$version]" '
    index($0, heading) == 1 { found = 1; next }
    found && /^## \[/ { exit }
    found { print }
' CHANGELOG.md)
# Without the blank lines at either end.
notes=$(printf '%s\n' "$notes" | sed -e '/./,$!d')
if [ -z "$notes" ]; then
    echo "CHANGELOG.md has no notes under ## [$version]" >&2
    exit 1
fi
printf '%s\n' "$notes"
