#!/usr/bin/env bash
# The engine's own differences, for verify-yarn1.sh:
#
#   yarn1-own-diffs.sh <plain1-vs-plain2 classes> <plain1-vs-engine classes>
#
# Both files hold `matchapack diff --meta --normalize-roots --classes` output
# (`<class>\t<path>` per line; each tree's own root written `<root>`, so a
# file an install script named after its tree's path is the same path in
# all three trees). Prints each line of the second that is not in the first: a
# difference between plain1 and the engine counts as plain Yarn's own
# variation only when the two plain runs differ on the same path IN THE SAME
# CLASS (a path whose mtime varies between plain runs does not excuse a
# content difference on it). Exit 0: the lines (none: no difference of its
# own); exit 2: a file could not be read or sorted, and nothing is decided.
set -uo pipefail
plain=${1:?plain classes} engine=${2:?engine classes}
tmp=$(mktemp -d "${TMPDIR:-/tmp}/yarn1-own.XXXXXX") || exit 2
trap 'rm -f "$tmp/p" "$tmp/e"; rmdir "$tmp"' EXIT
# Sorted into files, each status checked: inside a process substitution a
# failed sort would reach comm as an empty list, i.e. "nothing of its own".
LC_ALL=C sort -u -- "$plain" > "$tmp/p" || { echo "yarn1-own-diffs: cannot read $plain" >&2; exit 2; }
LC_ALL=C sort -u -- "$engine" > "$tmp/e" || { echo "yarn1-own-diffs: cannot read $engine" >&2; exit 2; }
LC_ALL=C comm -13 "$tmp/p" "$tmp/e" || exit 2
