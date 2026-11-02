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
# Copy-on-write copies of the source (macOS: cp -c; Linux: --reflink=auto).
if [ "$(uname)" = Darwin ]; then cpcow() { cp -c -R "$1" "$2"; }; else cpcow() { cp -a --reflink=auto "$1" "$2"; }; fi
cpcow "$src" "$work/A"
cpcow "$src" "$work/B"

marker="$work/.install-start"; touch "$marker"; sleep 1
# Baseline command for A (default `npm ci`); e.g. LOCKCLONE_VERIFY_BASELINE=install
# compares against a fresh `npm install`.
base=${LOCKCLONE_VERIFY_BASELINE:-ci}
echo "== npm $base (A)"
t0=$(python3 -c 'import time;print(time.time())')
(cd "$work/A" && npm "$base" --no-audit --no-fund > "$work/A.log" 2>&1) || { tail -30 "$work/A.log"; exit 1; }
if [ "$base" != ci ] && ! cmp -s "$src/package-lock.json" "$work/A/package-lock.json"; then
  echo "NOTE: npm $base rewrote package-lock.json"
fi
t1=$(python3 -c 'import time;print(time.time())')
echo "== lockclone install (B)"
if [ "${LOCKCLONE_VERIFY_VIA:-install}" = wrapper ]; then
  # The path a user takes: plain `npm ci` in a shell that sourced the wrapper.
  (cd "$work/B" && PATH="$(dirname "$bin"):$PATH" zsh -c ". '$here/shell/lockclone.sh'; npm ci --no-audit --no-fund" > "$work/B.log" 2>&1) || { tail -30 "$work/B.log"; exit 1; }
  grep -q '^lockclone: added' "$work/B.log" || { echo "wrapper did NOT install from the store:"; grep '^lockclone' "$work/B.log"; exit 1; }
else
  (cd "$work/B" && "$bin" install . "$@" > "$work/B.log" 2>&1) || { tail -30 "$work/B.log"; exit 1; }
fi
t2=$(python3 -c 'import time;print(time.time())')
grep '^\[lockclone\]\|^lockclone:' "$work/B.log" | grep -v '\] skip ' || true
python3 -c "print(f'npm ci: {$t1-$t0:.2f}s   lockclone: {$t2-$t1:.2f}s')" 2>/dev/null || true

echo "== diff A B"
rc=0
ign=()
for i in ${LOCKCLONE_VERIFY_IGNORE:-}; do ign+=(--ignore "$i"); done
"$bin" diff "$work/A" "$work/B" ${ign[@]+"${ign[@]}"} || rc=1
# npm ci leaves every extracted file and directory with the install time; a
# path under node_modules not newer than the marker is a stale mtime. Install
# scripts can legitimately leave old mtimes (bcrypt unpacks a prebuilt binary
# with its archive time), so only paths stale in B but NOT in A fail.
echo "== mtime (paths under node_modules not newer than install start; symlinks excluded)"
for t in A B; do
  (cd "$work/$t" && find node_modules ! -type l ! -newer "$marker" | LC_ALL=C sort) > "$work/$t.stale"
  echo "$t: $(wc -l < "$work/$t.stale" | tr -d ' ')"
done
extra=$(LC_ALL=C comm -13 "$work/A.stale" "$work/B.stale" | wc -l | tr -d ' ')
echo "stale in B only: $extra"
if [ "$extra" -ne 0 ] && [ "${LOCKCLONE_VERIFY_MTIME:-1}" = 1 ]; then
  LC_ALL=C comm -13 "$work/A.stale" "$work/B.stale" | head -5; rc=1
fi
echo "== npm ls --all (A vs B)"
ea=0; (cd "$work/A" && npm ls --all > "$work/A.ls" 2>&1) || ea=$?
eb=0; (cd "$work/B" && npm ls --all > "$work/B.ls" 2>&1) || eb=$?
sed -e "s#/$(basename "$work")/A#/<root>#g" -e 's#/[^ ]*_logs/[^ ]*#<log>#' "$work/A.ls" > "$work/A.ls.n"
sed -e "s#/$(basename "$work")/B#/<root>#g" -e 's#/[^ ]*_logs/[^ ]*#<log>#' "$work/B.ls" > "$work/B.ls.n"
if [ $ea -eq $eb ] && cmp -s "$work/A.ls.n" "$work/B.ls.n"; then
  echo "npm ls: same result as npm ci (exit $ea)"
else
  echo "npm ls: DIFFERS (A exit $ea, B exit $eb)"; diff "$work/A.ls.n" "$work/B.ls.n" | head -20; rc=1
fi
exit $rc
