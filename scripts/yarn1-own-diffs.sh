#!/usr/bin/env bash
# The engine's own differences, for verify-yarn1.sh:
#
#   yarn1-own-diffs.sh <plain1-vs-plain2 classes> <plain1-vs-engine classes>
#
# Both files hold `matchapack diff --meta --classes` output (`<class>\t<path>`
# per line). Prints each line of the second that is not in the first: a
# difference between plain1 and the engine counts as plain Yarn's own
# variation only when the two plain runs differ on the same path IN THE SAME
# CLASS (a path whose mtime varies between plain runs does not excuse a
# content difference on it). Exit 0 always; empty output: none of its own.
set -euo pipefail
LC_ALL=C comm -13 <(LC_ALL=C sort -u "${1:?plain classes}") <(LC_ALL=C sort -u "${2:?engine classes}")
