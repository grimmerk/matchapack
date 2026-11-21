#!/usr/bin/env bash
# Where nothing can be cloned, npm extracts every package itself: the
# install succeeds, matchapack says so, and the tree is npm ci's.
#   handover-test.sh <work-dir> <store> [<fill-dir>]
# <work-dir> holds the projects; put it where a clone from <store> cannot be
# made: a filesystem without reflinks (the runner's ext4, a container's
# overlayfs) or another filesystem than the store's (EXDEV). With
# <fill-dir> (a directory on the store's filesystem, which must clone), the
# store is filled first by a matchapack install there, so the checks run
# against a warm store (an existing store on another disk than the project).
# The project is fixtures/small (registry tarballs). npm's cache is the
# caller's: set npm_config_cache.
# Checks, each a PASS or FAIL line; exit status 1 when any failed:
#  1. `matchapack npm ci` (the shell function's path) in a project that has a
#     node_modules already (a sentinel in it), against plain `npm ci` in the
#     same project (scripts/verify.sh: the same tree, the hidden lockfile
#     byte-identical, `npm ls` the same, nothing cloned): it succeeds, says
#     "no clones from the store here, npm extracted the packages itself
#     (clone-unsupported ...)", and the sentinel went as it went with plain
#     npm ci (npm itself replaced node_modules).
#  2. The same project with its lockfile out of sync: npm stops before it
#     touches node_modules, matchapack exits with npm's own status, and the
#     sentinel is still there (matchapack removed nothing before npm ran).
# Used by PR CI (ci.yml, the linux job) and scripts/linux-test.sh.
set -uo pipefail
work=${1:?usage: handover-test.sh <work-dir> <store> [<fill-dir>]}; store=${2:?usage: handover-test.sh <work-dir> <store> [<fill-dir>]}; fill=${3:-}
here=$(cd "$(dirname "$0")/.." && pwd); bin=$here/target/release/matchapack
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 2; }
[ -n "${npm_config_cache:-}" ] || { echo "set npm_config_cache (this script never uses the user's own npm cache)" >&2; exit 2; }
mkdir -p "$work" "$store"; work=$(cd "$work" && pwd -P); store=$(cd "$store" && pwd -P)
export MATCHAPACK_STORE=$store MATCHAPACK_LOG_DIR=$work/logs MATCHAPACK_PATHS=$work MATCHAPACK_NPM_VERSIONS=any MATCHAPACK_NPM_FINGERPRINT=any
unset MATCHAPACK_ENGINE MATCHAPACK
fstype() { stat -f -c %T "$1" 2>/dev/null || echo "?"; }
echo "projects: $work ($(fstype "$work")); store: $store ($(fstype "$store")); npm $(npm -v)"
failures=0
pass() { echo "PASS $1"; }
fail() { echo "FAIL $1"; failures=$((failures+1)); }
project() { # dir
  rm -rf "${1:?}"; mkdir -p "$1/node_modules/sentinel"
  cp "$here/fixtures/small/package.json" "$here/fixtures/small/package-lock.json" "$1/"
  echo keep > "$1/node_modules/sentinel/keep"
}

if [ -n "$fill" ]; then
  mkdir -p "$fill"; fill=$(cd "$fill" && pwd -P)
  f=$fill/fill; project "$f"
  "$bin" install "$f" -- --no-audit --no-fund > "$work/fill.log" 2>&1; rc=$?
  n=$(grep -Eo 'inject: cloned [0-9]+' "$work/fill.log" | grep -Eo '[0-9]+$')
  if [ $rc = 0 ] && [ "${n:-0}" -gt 0 ]; then
    pass "store filled on $(fstype "$fill"): $n packages cloned there"
  else
    fail "filling the store on $fill (status $rc, cloned ${n:-none})"; tail -20 "$work/fill.log" | sed 's/^/    /'
  fi
  rm -rf "${f:?}"
fi

# 1. The hand-over, against plain npm ci.
src=$work/src; project "$src"
printf 'ci\0' > "$work/args"
out=$(MATCHAPACK_VERIFY_ARGS_FILE=$work/args MATCHAPACK_VERIFY_CLONES=none MATCHAPACK_VERIFY_BYTES=1 \
  "$here/scripts/verify.sh" "$src" "$work/v" 2>&1); rc=$?
grep -E 'RESULT|no clones from the store|^matchapack: added|npm ls:|^npm ci:|^cpu|byte-identical|stale in B|expected no clones' <<<"$out" | sed 's/^/    /'
said=no; grep -q 'no clones from the store here, npm extracted the packages itself (clone-unsupported' "$work/v/B.log" && said=yes
a=gone; [ -e "$work/v/A/node_modules/sentinel" ] && a=kept
b=gone; [ -e "$work/v/B/node_modules/sentinel" ] && b=kept
if [ $rc = 0 ] && [ $said = yes ] && [ $a = $b ]; then
  pass "npm extracted every package, said so; the tree is npm ci's; the old node_modules ($b) went as with plain npm ci ($a); files: $(find "$work/v/B/node_modules" -type f | wc -l | tr -d ' ')"
else
  fail "hand-over: verify.sh status $rc, said so: $said, sentinel with plain npm ci: $a, with matchapack: $b"
  grep -v '^\[matchapack\] skip ' "$work/v/B.log" | tail -20 | sed 's/^/    /'
fi

# 2. npm stops before it touches node_modules: so does matchapack.
for t in plain mp; do
  project "$work/sync-$t"
  python3 -I - "$work/sync-$t/package.json" <<'EOF'
import json, sys
p = sys.argv[1]
d = json.load(open(p))
d["dependencies"]["left-pad"] = "1.3.0"
json.dump(d, open(p, "w"), indent=2)
EOF
done
(cd "$work/sync-plain" && npm ci --no-audit --no-fund > "$work/sync-plain.log" 2>&1); want=$?
(cd "$work/sync-mp" && "$bin" npm ci --no-audit --no-fund > "$work/sync-mp.log" 2>&1); rc=$?
errs=$(grep -Ec 'npm (error|ERR!) code EUSAGE' "$work/sync-mp.log")
if [ $want != 0 ] && [ $rc = $want ] && [ "$errs" = 1 ] && [ -f "$work/sync-mp/node_modules/sentinel/keep" ] && [ -f "$work/sync-plain/node_modules/sentinel/keep" ]; then
  pass "lockfile out of sync: npm's own error and status ($rc), the old node_modules untouched"
else
  fail "out of sync: status $rc (plain npm ci: $want), npm's error printed $errs times, sentinel: $(ls "$work/sync-mp/node_modules" 2>&1 | tr '\n' ' ')"
  tail -10 "$work/sync-mp.log" | sed 's/^/    /'
fi

echo "handover checks failed: $failures"
[ "$failures" -eq 0 ]
