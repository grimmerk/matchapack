#!/usr/bin/env bash
# Verification harness.
#   verify.sh <source-project-dir> <work-dir> [lockclone install args...]
# Copies <source> (which must hold package.json + package-lock.json and any
# workspace sources, but no node_modules) to <work>/A and <work>/B, runs
# `npm ci` in A and `lockclone install` in B, then:
#   1. lockclone diff A B  (paths, types, modes, content hashes, symlink targets,
#      hidden lockfile as JSON)
#   2. npm ls --all in B must exit 0
set -euo pipefail
src=$1; work=$2; shift 2
here=$(cd "$(dirname "$0")/.." && pwd)
bin=$here/target/release/lockclone
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 2; }
rm -rf "${work:?}/A" "${work:?}/B"
mkdir -p "$work"
cp -c -R "$src" "$work/A"
cp -c -R "$src" "$work/B"

echo "== npm ci (A)"
t0=$(date +%s.%N 2>/dev/null || python3 -c 'import time;print(time.time())')
(cd "$work/A" && npm ci --no-audit --no-fund > "$work/A.log" 2>&1) || { tail -30 "$work/A.log"; exit 1; }
t1=$(python3 -c 'import time;print(time.time())')
echo "== lockclone install (B)"
(cd "$work/B" && "$bin" install . "$@" > "$work/B.log" 2>&1) || { tail -30 "$work/B.log"; exit 1; }
t2=$(python3 -c 'import time;print(time.time())')
grep '^\[lockclone\]' "$work/B.log" | grep -v '\] skip ' || true
python3 -c "print(f'npm ci: {$t1-$t0:.2f}s   lockclone: {$t2-$t1:.2f}s')" 2>/dev/null || true

echo "== diff A B"
rc=0
"$bin" diff "$work/A" "$work/B" || rc=1
echo "== npm ls --all (B)"
if (cd "$work/B" && npm ls --all > "$work/B.ls" 2>&1); then echo "npm ls: OK"; else echo "npm ls: FAILED"; tail -20 "$work/B.ls"; rc=1; fi
exit $rc
