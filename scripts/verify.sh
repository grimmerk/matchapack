#!/usr/bin/env bash
# Verification harness.
#   verify.sh <source-project-dir> <work-dir> [thinclone install args...]
# Copies <source> (which must hold package.json + package-lock.json and any
# workspace sources, but no node_modules) to <work>/A and <work>/B, runs
# `npm ci` in A and `thinclone install` in B, then:
#   1. thinclone diff A B  (paths, types, modes, content hashes, symlink targets,
#      hidden lockfile as JSON)
#   2. npm ls --all in B must exit 0
# The engine in B is thinclone's own switch: THINCLONE_ENGINE=inject runs the
# inject engine (npm's own `npm ci` with the extraction replaced by clones).
# With it, THINCLONE_VERIFY_CLONES=some fails the run when B cloned nothing
# (the hook did not fire: the trees are then identical without testing
# anything), =none when it cloned anything.
set -euo pipefail
src=$1; work=$2; shift 2
here=$(cd "$(dirname "$0")/.." && pwd)
bin=$here/target/release/thinclone
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 2; }
rm -rf "${work:?}/A" "${work:?}/A2" "${work:?}/B"
mkdir -p "$work"
# Copy-on-write copies of the source (macOS: cp -c; Linux: --reflink=auto).
if [ "$(uname)" = Darwin ]; then cpcow() { cp -c -R "$1" "$2"; }; else cpcow() { cp -a --reflink=auto "$1" "$2"; }; fi
cpcow "$src" "$work/A"
cpcow "$src" "$work/B"

marker="$work/.install-start"; touch "$marker"; sleep 1
# Baseline command for A (default `npm ci`); e.g. THINCLONE_VERIFY_BASELINE=install
# compares against a fresh `npm install`.
base=${THINCLONE_VERIFY_BASELINE:-ci}
# Harness runs log to the work dir unless the caller chose a log dir.
export THINCLONE_LOG_DIR=${THINCLONE_LOG_DIR:-$work/logs}
# The harness is how a new npm version gets verified, so B installs on any npm
# unless the caller narrows it (`thinclone install` applies the version gate).
export THINCLONE_NPM_VERSIONS=${THINCLONE_NPM_VERSIONS:-any}
# The copies live under the work dir: keep the user's trial scope file
# (~/.config/thinclone/paths) from deciding a wrapper run.
export THINCLONE_PATHS=${THINCLONE_PATHS:-$(cd "$work" && pwd -P)}
echo "== npm $base (A)"
t0=$(python3 -c 'import time;print(time.time())')
(cd "$work/A" && npm "$base" --no-audit --no-fund > "$work/A.log" 2>&1) || { tail -30 "$work/A.log"; exit 1; }
if [ "$base" != ci ] && ! cmp -s "$src/package-lock.json" "$work/A/package-lock.json"; then
  echo "NOTE: npm $base rewrote package-lock.json"
fi
t1=$(python3 -c 'import time;print(time.time())')
engine=${THINCLONE_ENGINE:-classic}
echo "== thinclone install (B, $engine engine)"
if [ "${THINCLONE_VERIFY_VIA:-install}" = wrapper ]; then
  # The path a user takes: plain `npm ci` in a shell that sourced the wrapper.
  # Not anchored: a script may leave the line unterminated (husky prints
  # ".git can't be found" without a newline).
  (cd "$work/B" && PATH="$(dirname "$bin"):$PATH" zsh -c ". '$here/shell/thinclone.sh'; npm ci --no-audit --no-fund" > "$work/B.log" 2>&1) || { tail -30 "$work/B.log"; exit 1; }
  grep -q 'thinclone: added' "$work/B.log" || { echo "wrapper did NOT install from the store:"; grep '^thinclone' "$work/B.log"; exit 1; }
else
  # The inject engine runs npm's own `npm ci`: give it A's flags (the classic
  # engine never runs the audit).
  npmflags=(); [ "$engine" = inject ] && npmflags=(-- --no-audit --no-fund)
  (cd "$work/B" && "$bin" install . "$@" ${npmflags[@]+"${npmflags[@]}"} > "$work/B.log" 2>&1) || { tail -30 "$work/B.log"; exit 1; }
fi
t2=$(python3 -c 'import time;print(time.time())')
grep '^\[thinclone\]\|^thinclone:' "$work/B.log" | grep -v '\] skip ' || true
python3 -c "print(f'npm ci: {$t1-$t0:.2f}s   thinclone: {$t2-$t1:.2f}s')" 2>/dev/null || true

rc=0
if [ "$engine" = inject ] && [ -n "${THINCLONE_VERIFY_CLONES:-}" ]; then
  # `thinclone install` prints "inject: cloned N", the shell function
  # "thinclone: added N packages".
  cloned=$(grep -Eo 'inject: cloned [0-9]+|thinclone: added [0-9]+' "$work/B.log" | grep -Eo '[0-9]+$' | head -1)
  case "$THINCLONE_VERIFY_CLONES:${cloned:-missing}" in
    some:missing|some:0) echo "the inject hook cloned nothing (THINCLONE_VERIFY_CLONES=some)"; rc=1 ;;
    none:0) ;;
    none:*) echo "expected no clones, got ${cloned} (THINCLONE_VERIFY_CLONES=none)"; rc=1 ;;
  esac
fi
ign=()
for i in ${THINCLONE_VERIFY_IGNORE:-}; do ign+=(--ignore "$i"); done
# THINCLONE_VERIFY_NOISE=1: run npm ci a second time (A2); paths that differ
# between two npm ci runs (non-deterministic build output) are ignored in A vs B.
if [ "${THINCLONE_VERIFY_NOISE:-0}" = 1 ]; then
  cpcow "$src" "$work/A2"
  (cd "$work/A2" && npm "$base" --no-audit --no-fund > "$work/A2.log" 2>&1) || { tail -30 "$work/A2.log"; exit 1; }
  "$bin" diff "$work/A" "$work/A2" --paths > "$work/noise.txt" || true
  echo "noise (differs between two npm ci runs, ignored): $(wc -l < "$work/noise.txt" | tr -d ' ') paths"
  while IFS= read -r i; do [ -n "$i" ] && ign+=(--ignore "$i"); done < "$work/noise.txt"
fi
echo "== diff A B"
"$bin" diff "$work/A" "$work/B" ${ign[@]+"${ign[@]}"} > "$work/diff.txt" || rc=1
cat "$work/diff.txt"
# THINCLONE_VERIFY_BYTES=1: the hidden lockfile must also be byte-identical, not
# only equal as JSON (npm writes every entry in a fixed key order).
if [ "${THINCLONE_VERIFY_BYTES:-0}" = 1 ] && grep -q 'hidden lockfile byte-identical: false' "$work/diff.txt"; then
  echo "hidden lockfile is not byte-identical (THINCLONE_VERIFY_BYTES=1)"; rc=1
fi
# npm ci leaves every extracted file and directory with the install time; a
# path under node_modules not newer than the marker is a stale mtime. Install
# scripts can legitimately leave old mtimes (bcrypt unpacks a prebuilt binary
# with its archive time), so only paths stale in B but NOT in A fail.
echo "== mtime (paths under node_modules not newer than install start; symlinks excluded)"
for t in A B; do
  if [ -d "$work/$t/node_modules" ]; then
    (cd "$work/$t" && find node_modules ! -type l ! -newer "$marker" | LC_ALL=C sort) > "$work/$t.stale"
  else
    : > "$work/$t.stale"   # nothing installed (a project without dependencies)
  fi
  echo "$t: $(wc -l < "$work/$t.stale" | tr -d ' ')"
done
extra=$(LC_ALL=C comm -13 "$work/A.stale" "$work/B.stale" | wc -l | tr -d ' ')
echo "stale in B only: $extra"
if [ "$extra" -ne 0 ] && [ "${THINCLONE_VERIFY_MTIME:-1}" = 1 ]; then
  LC_ALL=C comm -13 "$work/A.stale" "$work/B.stale" | head -5; rc=1
fi
echo "== npm ls --all (A vs B)"
ea=0; (cd "$work/A" && npm ls --all > "$work/A.ls" 2>&1) || ea=$?
eb=0; (cd "$work/B" && npm ls --all > "$work/B.ls" 2>&1) || eb=$?
# A project without "name" is shown by its directory name (A@ / B@).
sed -e "s#/$(basename "$work")/A#/<root>#g" -e 's#/[^ ]*_logs/[^ ]*#<log>#' -e '1s#^A@#<name>@#' "$work/A.ls" > "$work/A.ls.n"
sed -e "s#/$(basename "$work")/B#/<root>#g" -e 's#/[^ ]*_logs/[^ ]*#<log>#' -e '1s#^B@#<name>@#' "$work/B.ls" > "$work/B.ls.n"
if [ $ea -eq $eb ] && cmp -s "$work/A.ls.n" "$work/B.ls.n"; then
  echo "npm ls: same result as npm ci (exit $ea)"
else
  echo "npm ls: DIFFERS (A exit $ea, B exit $eb)"; diff "$work/A.ls.n" "$work/B.ls.n" | head -20; rc=1
fi
exit $rc
