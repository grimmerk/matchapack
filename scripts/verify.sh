#!/usr/bin/env bash
# Verification harness.
#   verify.sh <source-project-dir> <work-dir> [matchapack install args...]
# Copies <source> (which must hold package.json + package-lock.json and any
# workspace sources, but no node_modules) to <work>/A and <work>/B, runs
# `npm ci` in A and `matchapack install` in B, then:
#   1. matchapack diff A B  (paths, types, modes, content hashes, symlink targets,
#      hidden lockfile as JSON; --normalize-roots: each tree's own path, where an
#      install script wrote it into a file name, is written <root>)
#   2. npm ls --all in B must exit 0
# B is the inject engine (npm's own `npm ci` with the extraction replaced by
# clones; the only engine since PR C2). MATCHAPACK_VERIFY_CLONES=some fails
# the run when B cloned nothing (the hook did not fire: the trees are then
# identical without testing anything), =none when it cloned anything.
# MATCHAPACK_VERIFY_META=1 also compares mtimes outside the two runs, uid,
# gid and xattr names (diff --meta).
# MATCHAPACK_VERIFY_ARGS_FILE=<file> (scripts/verify-sample.sh, #77 part
# 1b): the npm command to run instead of npm ci, its arguments in the file,
# each ended by a NUL byte (`ci`, `install`, `left-pad@1.3.0`, …; NUL, so an
# argument may hold a line break): A (and A2) run plain `npm <args>`,
# B `matchapack npm <args>` (the shell function's path), both with
# --no-audit --no-fund; package.json, package-lock.json and
# npm-shrinkwrap.json must then also be byte-identical between A and B.
# MATCHAPACK_VERIFY_NPM_DAMAGE (tests only): a command (sh -c) run in B
# after its install, to show that a damaged tree is reported.
# The store is the caller's (MATCHAPACK_STORE, else the user's own:
# verify-sample.sh samples real projects against the real store on purpose).
# B's install records its copy under <store>/projects/ for `store gc`; the
# copy is temporary, so on exit every record pointing into <work-dir> is
# removed, and no other.
set -euo pipefail
src=$1; work=$2; shift 2
here=$(cd "$(dirname "$0")/.." && pwd)
bin=$here/target/release/matchapack
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 2; }
rm -rf "${work:?}/A" "${work:?}/A2" "${work:?}/B"
mkdir -p "$work"
# The records of this run's copies: <store>/projects/<32 hex>.json, each
# holding the canonical path of the project it was written for
# (src/store.rs, record_project). Only a record whose path is inside this
# work directory goes.
store=${MATCHAPACK_STORE:-$HOME/.cache/matchapack/store}
work_real=$(cd "$work" && pwd -P)
forget_copies() {
  python3 -I - "$store/projects" "$work_real" <<'EOF' || echo "could not remove this run's project records from $store/projects" >&2
import json, os, re, sys
d, w = sys.argv[1], sys.argv[2].rstrip("/") + "/"
try:
    names = os.listdir(d)
except FileNotFoundError:
    sys.exit(0)
for n in names:
    if not re.fullmatch(r"[0-9a-f]{32}\.json", n):
        continue
    p = os.path.join(d, n)
    try:
        path = json.load(open(p)).get("path")
    except (OSError, ValueError, AttributeError):
        continue
    if isinstance(path, str) and path.startswith(w):
        os.remove(p)
EOF
}
trap forget_copies EXIT
# Copy-on-write copies of the source (macOS: cp -c; Linux: --reflink=auto).
if [ "$(uname)" = Darwin ]; then cpcow() { cp -c -R -p "$1" "$2"; }; else cpcow() { cp -a --reflink=auto "$1" "$2"; }; fi
cpcow "$src" "$work/A"
cpcow "$src" "$work/B"

marker="$work/.install-start"; touch "$marker"; sleep 1
# Baseline command for A (default `npm ci`); e.g. MATCHAPACK_VERIFY_BASELINE=install
# compares against a fresh `npm install`.
base=${MATCHAPACK_VERIFY_BASELINE:-ci}
# Harness runs log to the work dir unless the caller chose a log dir.
export MATCHAPACK_LOG_DIR=${MATCHAPACK_LOG_DIR:-$work/logs}
# The harness is how a new npm version gets verified, so B installs on any npm
# unless the caller narrows it (`matchapack install` applies the version gate
# and the hook the pacote fingerprint gate; verify-npm.sh says whether this
# npm's fingerprint is listed).
export MATCHAPACK_NPM_VERSIONS=${MATCHAPACK_NPM_VERSIONS:-any}
export MATCHAPACK_NPM_FINGERPRINT=${MATCHAPACK_NPM_FINGERPRINT:-any}
# The copies live under the work dir: keep the user's trial scope file
# (~/.config/matchapack/paths) from deciding a wrapper run.
export MATCHAPACK_PATHS=${MATCHAPACK_PATHS:-$(cd "$work" && pwd -P)}
# The command A, A2 and B run: npm's `$base`, or the words of
# MATCHAPACK_VERIFY_ARGS_FILE.
npm_args=("$base")
if [ -n "${MATCHAPACK_VERIFY_ARGS_FILE:-}" ]; then
  npm_args=()
  while IFS= read -r -d '' a; do npm_args+=("$a"); done < "$MATCHAPACK_VERIFY_ARGS_FILE"
  [ ${#npm_args[@]} -gt 0 ] || { echo "MATCHAPACK_VERIFY_ARGS_FILE holds no arguments" >&2; exit 2; }
fi
echo "== npm ${npm_args[*]} (A)"
# The words as run, one JSON array (verify-sample.sh checks them against the
# logged run's: the boundaries between words included).
python3 -c 'import json,sys; print("npm words:", json.dumps(sys.argv[1:]))' "${npm_args[@]}"
t0=$(python3 -c 'import time;print(time.time_ns())')
(cd "$work/A" && npm "${npm_args[@]}" --no-audit --no-fund > "$work/A.log" 2>&1) || { tail -30 "$work/A.log"; exit 1; }
if [ "$base" != ci ] && ! cmp -s "$src/package-lock.json" "$work/A/package-lock.json"; then
  echo "NOTE: npm $base rewrote package-lock.json"
fi
t1=$(python3 -c 'import time;print(time.time_ns())')
echo "== matchapack install (B)"
if [ -n "${MATCHAPACK_VERIFY_ARGS_FILE:-}" ]; then
  # `matchapack npm`, as the shell function runs it.
  (cd "$work/B" && "$bin" npm "${npm_args[@]}" --no-audit --no-fund > "$work/B.log" 2>&1) || { tail -30 "$work/B.log"; exit 1; }
elif [ "${MATCHAPACK_VERIFY_VIA:-install}" = wrapper ]; then
  # The path a user takes: plain `npm ci` in a shell that sourced the wrapper.
  # Not anchored: a script may leave the line unterminated (husky prints
  # ".git can't be found" without a newline).
  (cd "$work/B" && PATH="$(dirname "$bin"):$PATH" zsh -c ". '$here/shell/matchapack.sh'; npm ci --no-audit --no-fund" > "$work/B.log" 2>&1) || { tail -30 "$work/B.log"; exit 1; }
  grep -q 'matchapack: added' "$work/B.log" || { echo "wrapper did NOT install from the store:"; grep '^matchapack' "$work/B.log"; exit 1; }
else
  # matchapack runs npm's own `npm ci`: give it A's flags.
  (cd "$work/B" && "$bin" install . "$@" -- --no-audit --no-fund > "$work/B.log" 2>&1) || { tail -30 "$work/B.log"; exit 1; }
fi
t2=$(python3 -c 'import time;print(time.time_ns())')
grep '^\[matchapack\]\|^matchapack:' "$work/B.log" | grep -v '\] skip ' || true
python3 -c "print(f'npm ${npm_args[0]}: {($t1-$t0)/1e9:.2f}s   matchapack: {($t2-$t1)/1e9:.2f}s')" 2>/dev/null || true
if [ -n "${MATCHAPACK_VERIFY_NPM_DAMAGE:-}" ]; then
  (cd "$work/B" && sh -c "$MATCHAPACK_VERIFY_NPM_DAMAGE") || { echo "MATCHAPACK_VERIFY_NPM_DAMAGE failed"; exit 1; }
  echo "B damaged on purpose (MATCHAPACK_VERIFY_NPM_DAMAGE)"
fi

rc=0
if [ -n "${MATCHAPACK_VERIFY_CLONES:-}" ]; then
  # `matchapack install` prints "inject: cloned N", the shell function
  # "matchapack: added N packages".
  cloned=$(grep -Eo 'inject: cloned [0-9]+|matchapack: added [0-9]+|matchapack: [0-9]+ packages? cloned' "$work/B.log" | grep -Eo '[0-9]+' | head -1 || true)
  case "$MATCHAPACK_VERIFY_CLONES:${cloned:-missing}" in
    some:missing|some:0) echo "the inject hook cloned nothing (MATCHAPACK_VERIFY_CLONES=some)"; rc=1 ;;
    none:0) ;;
    none:*) echo "expected no clones, got ${cloned} (MATCHAPACK_VERIFY_CLONES=none)"; rc=1 ;;
  esac
fi
ign=()
for i in ${MATCHAPACK_VERIFY_IGNORE:-}; do ign+=(--ignore "$i"); done
# MATCHAPACK_VERIFY_NOISE=1: run npm ci a second time (A2); paths that differ
# between two npm ci runs (non-deterministic build output) are ignored in A vs B.
if [ "${MATCHAPACK_VERIFY_NOISE:-0}" = 1 ]; then
  cpcow "$src" "$work/A2"
  (cd "$work/A2" && npm "${npm_args[@]}" --no-audit --no-fund > "$work/A2.log" 2>&1) || { tail -30 "$work/A2.log"; exit 1; }
  # With --normalize-roots, as A vs B below: the ignored paths are given in
  # the form that comparison reports.
  "$bin" diff --normalize-roots "$work/A" "$work/A2" --paths > "$work/noise.txt" || true
  echo "noise (differs between two npm ci runs, ignored): $(wc -l < "$work/noise.txt" | tr -d ' ') paths"
  while IFS= read -r i; do [ -n "$i" ] && ign+=(--ignore "$i"); done < "$work/noise.txt"
fi
# Another command than npm ci writes package.json and the lockfile: the
# same bytes, whatever two plain runs did with them.
if [ -n "${MATCHAPACK_VERIFY_ARGS_FILE:-}" ]; then
  for f in package.json package-lock.json npm-shrinkwrap.json; do
    if [ -e "$work/A/$f" ] || [ -e "$work/B/$f" ]; then
      if cmp -s "$work/A/$f" "$work/B/$f"; then echo "$f byte-identical (A, B): true"; else echo "$f byte-identical (A, B): false"; rc=1; fi
    fi
  done
fi
echo "== diff A B"
"$bin" diff --normalize-roots "$work/A" "$work/B" ${ign[@]+"${ign[@]}"} > "$work/diff.txt" || rc=1
cat "$work/diff.txt"
# MATCHAPACK_VERIFY_BYTES=1: the hidden lockfile must also be byte-identical, not
# only equal as JSON (npm writes every entry in a fixed key order).
# MATCHAPACK_VERIFY_META=1: also compare each path's mtime, uid, gid and
# extended attribute names (`diff --meta`). A time written during its own
# install (every extracted path: npm ci leaves them at the install time) may
# differ between A and B; any other time must be equal (src/diff.rs,
# MetaOpts).
if [ "${MATCHAPACK_VERIFY_META:-0}" = 1 ]; then
  "$bin" diff --meta --normalize-roots --a-run "$t0:$t1" --b-run "$t1:$t2" "$work/A" "$work/B" ${ign[@]+"${ign[@]}"} > "$work/meta.txt" || rc=1
  if grep -q '^RESULT: IDENTICAL' "$work/meta.txt"; then
    echo "meta: identical ($(grep -m1 '^meta: [0-9]* paths' "$work/meta.txt" | cut -d' ' -f2) paths: mtime, uid, gid, xattr names)"
  else
    echo "meta: DIFFERENT"; sed -n '/by class/,$p' "$work/meta.txt" | head -30
  fi
fi
if [ "${MATCHAPACK_VERIFY_BYTES:-0}" = 1 ] && grep -q 'hidden lockfile byte-identical: false' "$work/diff.txt"; then
  echo "hidden lockfile is not byte-identical (MATCHAPACK_VERIFY_BYTES=1)"; rc=1
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
if [ "$extra" -ne 0 ] && [ "${MATCHAPACK_VERIFY_MTIME:-1}" = 1 ]; then
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
