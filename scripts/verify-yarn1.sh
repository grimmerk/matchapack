#!/usr/bin/env bash
# The Yarn 1 harness: plain Yarn 1 run twice on fresh copies of a project,
# compared with `matchapack diff --meta` (paths, types, modes, contents,
# symlink targets, .yarn-integrity bytes, mtimes to the nanosecond, uid, gid,
# xattr names). Plain against plain must come out identical before anything
# is compared with an engine; when it does not, the run says "plain varies"
# and shows the evidence (NOTES "Yarn 1 harness").
#
#   verify-yarn1.sh [options] <project-dir> [yarn install args...]
#
# <project-dir> holds package.json + yarn.lock (and workspace manifests), no
# node_modules; it is copied, never changed. Default yarn args:
# --frozen-lockfile --non-interactive; the ones given are added.
# Options:
#   --work DIR         where the copies, caches and logs go (default: a new
#                      temporary directory)
#   --cache warm       (default) one scratch cache, filled by a first run
#                      ("prime") before the two compared runs
#   --cache empty      each compared run starts from its own empty cache
#   --cache-group G    warm only: chgrp the filled cache to group G (one the
#                      user is in): a copy made by Node >= 22 takes the cache
#                      file's group, a clone the directory's
#   --cache-xattr      warm only: give one cache file an extra extended
#                      attribute: Node's copy drops it, a clone keeps it
#   --engine NAME      compare an engine run as well (no engine exists yet)
# Environment: MATCHAPACK_YARN_NODE (node binary, default the node on PATH),
# MATCHAPACK_YARN_JS (yarn.js, default the yarn on PATH, resolved).
# Exit: 0 identical; 1 a difference (or a failed run); 2 usage; 3 plain varies.
# Kept in the work directory: the trees (plain1, plain2), each run's log, the
# full comparison (diff-plain.txt) and `runs`: one line per run, "<name>
# <start ns>:<end ns> <cache dir>", the --a-run / --a-cache values for
# comparing these trees again.
# Yarn always gets a scratch --cache-folder, YARN_DISABLE_SELF_UPDATE_CHECK
# (so it never writes ~/.yarnrc) and no GITHUB_TOKEN.
set -uo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
bin=$here/target/release/matchapack
work= cache_mode=warm cache_group= cache_xattr=0 engine=
while [ $# -gt 0 ]; do
  case $1 in
    --work) work=${2:?--work needs a directory}; shift 2 ;;
    --cache) cache_mode=${2:?--cache needs warm or empty}; shift 2 ;;
    --cache-group) cache_group=${2:?--cache-group needs a group}; shift 2 ;;
    --cache-xattr) cache_xattr=1; shift ;;
    --engine) engine=${2:?--engine needs a name}; shift 2 ;;
    -*) echo "unknown option $1" >&2; exit 2 ;;
    *) break ;;
  esac
done
src=${1:?usage: verify-yarn1.sh [options] <project-dir> [yarn install args...]}; shift
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 2; }
[ -f "$src/package.json" ] && [ -f "$src/yarn.lock" ] || { echo "$src: needs package.json and yarn.lock" >&2; exit 2; }
[ -e "$src/node_modules" ] && { echo "$src has a node_modules: give a clean copy" >&2; exit 2; }
case $cache_mode in
  warm) ;;
  empty) [ -z "$cache_group" ] && [ $cache_xattr = 0 ] || { echo "--cache-group / --cache-xattr need --cache warm" >&2; exit 2; } ;;
  *) echo "--cache $cache_mode: warm or empty" >&2; exit 2 ;;
esac
if [ -n "$engine" ]; then
  # PR 2 (grimmerk/matchapack-private, Yarn 1 engine "A+") adds the engine
  # here: run_engine in a third copy with the same cache, then
  # `matchapack diff --meta plain1 engine`, where a path that also differs
  # between plain1 and plain2 counts as "plain varies", not as the engine's.
  echo "--engine $engine: the Yarn 1 engine is not built yet; this harness compares plain runs only" >&2
  exit 2
fi
node=${MATCHAPACK_YARN_NODE:-$(command -v node)}
yarnjs=${MATCHAPACK_YARN_JS:-}
if [ -z "$yarnjs" ]; then
  y=$(command -v yarn) || { echo "no yarn on PATH (or set MATCHAPACK_YARN_JS)" >&2; exit 2; }
  yarnjs=$(python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$y")
fi
[ -x "$node" ] && [ -f "$yarnjs" ] || { echo "node ($node) or yarn.js ($yarnjs) not found" >&2; exit 2; }
yver=$("$node" "$yarnjs" --version 2>/dev/null) || { echo "$yarnjs does not run" >&2; exit 2; }
case $yver in 1.*) ;; *) echo "yarn $yver is not Yarn 1" >&2; exit 2 ;; esac
[ -n "$work" ] || work=$(mktemp -d "${TMPDIR:-/tmp}/verify-yarn1.XXXXXX")
mkdir -p "$work"; work=$(cd "$work" && pwd -P)
# The cleanup below deletes run directories inside $work, and each run copies
# $src into $work: the two must not overlap.
src_real=$(cd "$src" && pwd -P)
case "$work/" in "$src_real/"*) echo "--work $work is inside the source $src_real" >&2; exit 2 ;; esac
case "$src_real/" in "$work/"*) echo "the source $src_real is inside --work $work" >&2; exit 2 ;; esac
rm -rf "${work:?}/prime" "${work:?}/plain1" "${work:?}/plain2" "${work:?}/cache" "${work:?}/cache-1" "${work:?}/cache-2" "${work:?}/runs"
echo "yarn $yver, node $("$node" -v), $("$bin" --version), $cache_mode cache, work $work"

# Fixtures install from the local tarball server (as in verify-npm.sh).
server=
if grep -q '127.0.0.1:48321' "$src/yarn.lock" && ! curl -sf -o /dev/null http://127.0.0.1:48321/y1-b-1.0.0.tgz; then
  python3 -m http.server 48321 --bind 127.0.0.1 --directory "$here/fixtures/optional-upward/tarballs" >/dev/null 2>&1 &
  server=$!
  for _ in $(seq 100); do curl -sf -o /dev/null http://127.0.0.1:48321/y1-b-1.0.0.tgz && break; sleep 0.1; done
fi
trap '[ -n "$server" ] && kill "$server" 2>/dev/null' EXIT

# Copies keep the source's times, modes and group (-p), so files outside
# node_modules compare equal. Not clones (cp -c): a clone takes the group of
# the directory it is made in, not the source's.
if [ "$(uname)" = Darwin ]; then cpp() { cp -p -R "$1" "$2"; }; else cpp() { cp -a "$1" "$2"; }; fi
now_ns() { python3 -c 'import time; print(time.time_ns())'; }
# yarn_run <name> <cache>: install in $work/<name>; sets run_<name>=START:END.
yarn_run() {
  local name=$1 cache=$2 s e
  shift 2
  # A partial copy could still install and compare as identical: stop.
  cpp "$src" "$work/$name" || { echo "copying $src to $work/$name failed"; exit 1; }
  s=$(now_ns)
  (cd "$work/$name" && env -u GITHUB_TOKEN YARN_DISABLE_SELF_UPDATE_CHECK=true \
    "$node" "$yarnjs" install --frozen-lockfile --non-interactive --cache-folder "$cache" "$@") > "$work/$name.log" 2>&1
  local rc=$?
  e=$(now_ns)
  if [ $rc != 0 ]; then echo "yarn failed in $name (exit $rc):"; tail -15 "$work/$name.log"; exit 1; fi
  printf -v "run_$name" '%s:%s' "$s" "$e"
  echo "$name $s:$e $cache" >> "$work/runs"
}

if [ $cache_mode = warm ]; then
  c1=$work/cache c2=$work/cache
  yarn_run prime "$c1" "$@"
  if [ -n "$cache_group" ]; then
    chgrp -R "$cache_group" "$c1" || { echo "chgrp $cache_group failed (is the user in that group?)"; exit 2; }
    echo "cache group: $cache_group"
  fi
  if [ $cache_xattr = 1 ]; then
    f=$(find "$c1" -type f -name package.json -path '*/node_modules/*' | LC_ALL=C sort | head -1)
    [ -n "$f" ] || { echo "no package file in the cache to tag"; exit 2; }
    python3 -c 'import os,sys; os.setxattr(sys.argv[1], "user.matchapack.verify", b"1")' "$f" 2>/dev/null \
      || xattr -w user.matchapack.verify 1 "$f" || { echo "cannot set an xattr on $f"; exit 2; }
    echo "cache xattr: user.matchapack.verify on ${f#"$c1"/}"
  fi
else
  c1=$work/cache-1 c2=$work/cache-2
fi
yarn_run plain1 "$c1" "$@"
yarn_run plain2 "$c2" "$@"
# The engine run goes here (see --engine above).

"$bin" diff --meta "$work/plain1" "$work/plain2" --a-run "$run_plain1" --b-run "$run_plain2" \
  --a-cache "$c1" --b-cache "$c2" > "$work/diff-plain.txt"
rc=$?
grep -v '^    ' "$work/diff-plain.txt" | grep -v '^  written during\|^  from each run' || true
if [ $rc = 0 ]; then
  echo "plain vs plain: IDENTICAL"
  exit 0
fi
if [ $rc != 1 ]; then echo "matchapack diff failed (exit $rc)"; exit 1; fi
echo "plain vs plain: PLAIN VARIES (two plain Yarn runs differ; full list: $work/diff-plain.txt)"
grep '^    ' "$work/diff-plain.txt" | head -20
exit 3
