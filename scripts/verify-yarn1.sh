#!/usr/bin/env bash
# The Yarn 1 harness: plain Yarn 1 run twice on fresh copies of a project,
# compared with `matchapack diff --meta` (paths, types, modes, contents,
# symlink targets, .yarn-integrity bytes, mtimes to the nanosecond, uid, gid,
# xattr names). Plain against plain must come out identical before anything
# is compared with an engine; when it does not, the run says "plain varies"
# and shows the evidence (NOTES "Yarn 1 harness"). With --engine yarn1, a
# third copy is installed by `matchapack yarn` with the same cache and
# compared with the first plain run (NOTES "Yarn 1 engine").
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
#                      ("prime") before the compared runs
#   --cache empty      each compared run starts from its own empty cache
#   --cache-group G    warm only: chgrp the filled cache to group G (one the
#                      user is in): a copy made by Node >= 22 takes the cache
#                      file's group, a clone the directory's
#   --cache-xattr      warm only: give every package file in the cache an
#                      extra extended attribute: Node's copy drops it, a
#                      clone keeps it
#   --cache-dir DIR    warm only: use this existing Yarn cache instead of a
#                      new one in the work directory (a large project's
#                      packages need not be downloaded again; the prime run
#                      still runs, so the compared runs fetch nothing). Not
#                      with --cache-group / --cache-xattr, which change it.
#   --prime-node NODE  warm only: fill the cache with this node binary (the
#                      compared runs use MATCHAPACK_YARN_NODE): Yarn's times
#                      then pass through two Node versions' conversions
#   --then DIR         after the install, copy DIR's package.json and
#                      yarn.lock into each tree and install again (an
#                      incremental install over the first tree); the
#                      comparison is of the second install
#   --before-then CMD  with --then: run CMD (sh -c) in each tree between
#                      the two installs (remove part of node_modules, ...)
#   --subdir DIR       run each install in DIR, a directory of the project
#                      (a workspace: Yarn installs at the workspace root, or
#                      with --focus that workspace); the trees compared are
#                      still the whole projects
#   --engine yarn1    also install a third copy with `matchapack yarn` and
#                      compare it with plain1; a path that also differs
#                      between plain1 and plain2 counts as "plain varies",
#                      not as the engine's
#   --engine-min-clones N  the engine run must have cloned at least N files
#                      (default 1; 0 for a case that must clone nothing)
# Environment: MATCHAPACK_YARN_NODE (node binary, default the node on PATH),
# MATCHAPACK_YARN_JS (yarn.js, default the yarn on PATH, resolved); the
# engine's test switches (MATCHAPACK_YARN_FAIL_EVERY, …) reach the engine run.
# Exit: 0 identical; 1 a difference (or a failed run); 2 usage; 3 plain varies
# (and, with --engine, the engine differs from plain1 only where plain does).
# Kept in the work directory: the trees (plain1, plain2, engine), each run's
# log, the full comparisons (diff-plain.txt, diff-engine.txt), the engine's
# log line (engine.json) and `runs`: one line per run, "<name> <start
# ns>:<end ns> <cache dir>", the --a-run / --a-cache values for comparing
# these trees again.
# Every comparison uses --normalize-roots: each tree's own path, where an
# install script wrote it into a file name (node-gyp's .deps/<absolute
# path>/...), is written <root>, so plain1, plain2 and the engine have the
# same path there (issue grimmerk/matchapack-private#48).
# Yarn always gets a scratch --cache-folder, YARN_DISABLE_SELF_UPDATE_CHECK
# (so it never writes ~/.yarnrc) and no GITHUB_TOKEN; matchapack a scratch
# store, log and config directory.
set -uo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
bin=$here/target/release/matchapack
work= cache_mode=warm cache_group= cache_xattr=0 engine= prime_node= then= min_clones=1 cache_dir= before_then= subdir=.
while [ $# -gt 0 ]; do
  case $1 in
    --work) work=${2:?--work needs a directory}; shift 2 ;;
    --cache) cache_mode=${2:?--cache needs warm or empty}; shift 2 ;;
    --cache-group) cache_group=${2:?--cache-group needs a group}; shift 2 ;;
    --cache-xattr) cache_xattr=1; shift ;;
    --cache-dir) cache_dir=${2:?--cache-dir needs a directory}; shift 2 ;;
    --prime-node) prime_node=${2:?--prime-node needs a node binary}; shift 2 ;;
    --then) then=${2:?--then needs a directory}; shift 2 ;;
    --before-then) before_then=${2:?--before-then needs a command}; shift 2 ;;
    --subdir) subdir=${2:?--subdir needs a directory}; shift 2 ;;
    --engine) engine=${2:?--engine needs a name}; shift 2 ;;
    --engine-min-clones) min_clones=${2:?--engine-min-clones needs a number}; shift 2
      case $min_clones in ''|*[!0-9]*) echo "--engine-min-clones $min_clones: a number" >&2; exit 2 ;; esac ;;
    -*) echo "unknown option $1" >&2; exit 2 ;;
    *) break ;;
  esac
done
src=${1:?usage: verify-yarn1.sh [options] <project-dir> [yarn install args...]}; shift
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 2; }
[ -f "$src/package.json" ] && [ -f "$src/yarn.lock" ] || { echo "$src: needs package.json and yarn.lock" >&2; exit 2; }
[ -e "$src/node_modules" ] && { echo "$src has a node_modules: give a clean copy" >&2; exit 2; }
case $cache_mode in
  warm) [ -z "$cache_dir" ] || { [ -z "$cache_group" ] && [ $cache_xattr = 0 ]; } || { echo "--cache-dir is not changed: no --cache-group / --cache-xattr" >&2; exit 2; } ;;
  empty) [ -z "$cache_group" ] && [ $cache_xattr = 0 ] && [ -z "$prime_node" ] && [ -z "$cache_dir" ] || { echo "--cache-group / --cache-xattr / --prime-node / --cache-dir need --cache warm" >&2; exit 2; } ;;
  *) echo "--cache $cache_mode: warm or empty" >&2; exit 2 ;;
esac
case $engine in
  ''|yarn1) ;;
  *) echo "--engine $engine: the Yarn 1 engine is yarn1" >&2; exit 2 ;;
esac
[ -z "$then" ] || { [ -f "$then/package.json" ] && [ -f "$then/yarn.lock" ]; } || { echo "--then $then: needs package.json and yarn.lock" >&2; exit 2; }
[ -z "$before_then" ] || [ -n "$then" ] || { echo "--before-then needs --then" >&2; exit 2; }
case $subdir in /*|..|../*|*/..|*/../*) echo "--subdir $subdir: a relative path inside the project" >&2; exit 2 ;; esac
[ -d "$src/$subdir" ] || { echo "--subdir $subdir: no such directory in $src" >&2; exit 2; }
node=${MATCHAPACK_YARN_NODE:-$(command -v node)}
yarnjs=${MATCHAPACK_YARN_JS:-}
if [ -z "$yarnjs" ]; then
  y=$(command -v yarn) || { echo "no yarn on PATH (or set MATCHAPACK_YARN_JS)" >&2; exit 2; }
  yarnjs=$(python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$y")
fi
[ -x "$node" ] && [ -f "$yarnjs" ] || { echo "node ($node) or yarn.js ($yarnjs) not found" >&2; exit 2; }
# Absolute before anything changes directory (each run is started from its
# own tree).
if [ -n "$prime_node" ]; then
  case $prime_node in */*) ;; *) prime_node=$(command -v "$prime_node") || { echo "--prime-node: no such command" >&2; exit 2; } ;; esac
  [ -x "$prime_node" ] || { echo "--prime-node $prime_node not found" >&2; exit 2; }
  prime_node=$(cd "$(dirname "$prime_node")" && pwd -P)/$(basename "$prime_node")
fi
if [ -n "$cache_dir" ]; then
  mkdir -p "$cache_dir" && cache_dir=$(cd "$cache_dir" && pwd -P) || { echo "--cache-dir $cache_dir: cannot use it" >&2; exit 2; }
fi
yver=$("$node" "$yarnjs" --version 2>/dev/null) || { echo "$yarnjs does not run" >&2; exit 2; }
case $yver in 1.*) ;; *) echo "yarn $yver is not Yarn 1" >&2; exit 2 ;; esac
[ -n "$work" ] || work=$(mktemp -d "${TMPDIR:-/tmp}/verify-yarn1.XXXXXX")
mkdir -p "$work"; work=$(cd "$work" && pwd -P)
# The cleanup below deletes run directories inside $work, and each run copies
# $src into $work: the two must not overlap.
src_real=$(cd "$src" && pwd -P)
case "$work/" in "$src_real/"*) echo "--work $work is inside the source $src_real" >&2; exit 2 ;; esac
case "$src_real/" in "$work/"*) echo "the source $src_real is inside --work $work" >&2; exit 2 ;; esac
# Nor may a cache given with --cache-dir: the cleanup would delete it, or
# delete inside it.
if [ -n "$cache_dir" ]; then
  case "$cache_dir/" in "$work/"*) echo "--cache-dir $cache_dir is inside --work $work" >&2; exit 2 ;; esac
  case "$work/" in "$cache_dir/"*) echo "--work $work is inside --cache-dir $cache_dir" >&2; exit 2 ;; esac
fi
rm -rf "${work:?}/prime" "${work:?}/plain1" "${work:?}/plain2" "${work:?}/engine" "${work:?}/cache" "${work:?}/cache-1" "${work:?}/cache-2" \
  "${work:?}/cache-e" "${work:?}/runs" "${work:?}/mp" "${work:?}/engine.json" "${work:?}/diff-engine.txt"
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
# install <name> <cache> <node> <how> [args]: one install in $work/<name>;
# <how> is plain or engine. Exit status: the install's.
install() {
  local name=$1 cache=$2 n=$3 how=$4
  shift 4
  if [ "$how" = plain ]; then
    (cd "$work/$name/$subdir" && env -u GITHUB_TOKEN YARN_DISABLE_SELF_UPDATE_CHECK=true \
      "$n" "$yarnjs" install --frozen-lockfile --non-interactive --cache-folder "$cache" "$@") >> "$work/$name.log" 2>&1
  else
    # The engine runs the node on PATH and the yarn.js given; a decline
    # exits 3 (MATCHAPACK_NO_FALLBACK) instead of running plain Yarn.
    (cd "$work/$name/$subdir" && env -u GITHUB_TOKEN -u MATCHAPACK_ENGINE -u MATCHAPACK_PATHS -u MATCHAPACK \
      YARN_DISABLE_SELF_UPDATE_CHECK=true PATH="$(dirname "$n"):$PATH" MATCHAPACK_YARN_JS="$yarnjs" \
      MATCHAPACK_STORE="$work/mp/store" MATCHAPACK_LOG_DIR="$work/mp/log" MATCHAPACK_CONFIG_DIR="$work/mp/config" MATCHAPACK_NO_FALLBACK=1 \
      "$bin" yarn install --frozen-lockfile --non-interactive --cache-folder "$cache" "$@") >> "$work/$name.log" 2>&1
  fi
}
# yarn_run <name> <cache> <node> <how> [args]: copy the project to
# $work/<name>, install (twice with --then); sets run_<name>=START:END.
yarn_run() {
  local name=$1 cache=$2 n=$3 how=$4 s e rc
  shift 4
  # A partial copy could still install and compare as identical: stop.
  cpp "$src" "$work/$name" || { echo "copying $src to $work/$name failed"; exit 1; }
  : > "$work/$name.log"
  s=$(now_ns)
  install "$name" "$cache" "$n" "$how" "$@"; rc=$?
  if [ $rc = 0 ] && [ -n "$then" ]; then
    cp -p "$then/package.json" "$then/yarn.lock" "$work/$name/" \
      && { [ -z "$before_then" ] || (cd "$work/$name" && sh -c "$before_then") >> "$work/$name.log" 2>&1; } \
      && install "$name" "$cache" "$n" "$how" "$@"; rc=$?
  fi
  e=$(now_ns)
  if [ $rc != 0 ]; then echo "$how yarn failed in $name (exit $rc):"; tail -15 "$work/$name.log"; exit 1; fi
  printf -v "run_$name" '%s:%s' "$s" "$e"
  echo "$name $s:$e $cache" >> "$work/runs"
}

if [ $cache_mode = warm ]; then
  c1=${cache_dir:-$work/cache}; c2=$c1 ce=$c1
  yarn_run prime "$c1" "${prime_node:-$node}" plain "$@"
  if [ -n "$cache_group" ]; then
    chgrp -R "$cache_group" "$c1" || { echo "chgrp $cache_group failed (is the user in that group?)"; exit 2; }
    echo "cache group: $cache_group"
  fi
  if [ $cache_xattr = 1 ]; then
    # Every package file: which ones the engine clones (not among Yarn's
    # first copies, which Node makes) is not known in advance.
    find "$c1" -type f -path '*/node_modules/*' | LC_ALL=C sort > "$work/xattr-files"
    f=$(head -1 "$work/xattr-files")
    [ -n "$f" ] || { echo "no package file in the cache to tag"; exit 2; }
    # One process for all (macOS's xattr tool starts a Python per file).
    python3 -c '
import ctypes, os, sys
libc = ctypes.CDLL(None, use_errno=True)
for p in open(sys.argv[1]).read().splitlines():
    if hasattr(os, "setxattr"):
        os.setxattr(p, "user.matchapack.verify", b"1")
    elif libc.setxattr(p.encode(), b"user.matchapack.verify", b"1", 1, 0, 0) != 0:
        sys.exit("setxattr %s: %s" % (p, os.strerror(ctypes.get_errno())))
' "$work/xattr-files" || { echo "cannot set the xattr on the cache files"; exit 2; }
    echo "cache xattr: user.matchapack.verify on $(wc -l < "$work/xattr-files" | tr -d ' ') files, first ${f#"$c1"/}"
  fi
else
  c1=$work/cache-1 c2=$work/cache-2 ce=$work/cache-e
fi
yarn_run plain1 "$c1" "$node" plain "$@"
yarn_run plain2 "$c2" "$node" plain "$@"
[ -n "$engine" ] && yarn_run engine "$ce" "$node" engine "$@"

"$bin" diff --meta --normalize-roots "$work/plain1" "$work/plain2" --a-run "$run_plain1" --b-run "$run_plain2" \
  --a-cache "$c1" --b-cache "$c2" > "$work/diff-plain.txt"
rc=$?
grep -v '^    ' "$work/diff-plain.txt" | grep -v '^  written during\|^  from each run' || true
if [ $rc != 0 ] && [ $rc != 1 ]; then echo "matchapack diff failed (exit $rc)"; exit 1; fi
plain_rc=$rc
if [ $rc = 0 ]; then
  echo "plain vs plain: IDENTICAL"
else
  echo "plain vs plain: PLAIN VARIES (two plain Yarn runs differ; full list: $work/diff-plain.txt)"
  grep '^    ' "$work/diff-plain.txt" | head -20
fi
[ -n "$engine" ] || { [ $plain_rc = 0 ] && exit 0; exit 3; }

# --- The engine run ---------------------------------------------------------
# Its log line: it must have run (not declined) and cloned what it should.
tail -1 "$work/mp/log/log.jsonl" > "$work/engine.json" 2>/dev/null
stats=$(python3 -c '
import json, sys
r = json.load(open(sys.argv[1]))
y = r.get("yarn1") or {}
copied = ", ".join(f"{k} {v}" for k, v in sorted((y.get("copied") or {}).items())) or "-"
cache = "%s after %s s, install %.1f s" % (y.get("cache"), y.get("cache_secs"), r.get("secs", 0))
print(r.get("outcome"), y.get("cloned", 0), y.get("cloned_bytes", 0), y.get("copied_total", 0), y.get("link_steps", 0), y.get("unseen"), copied, cache, sep="\t")
' "$work/engine.json" 2>/dev/null) || { echo "engine: no log line ($work/mp/log/log.jsonl)"; exit 1; }
IFS=$'\t' read -r outcome cloned cbytes copied steps unseen why cache <<< "$stats"
echo "engine: $outcome, cloned $cloned files ($(( cbytes / 1048576 )) MiB), copied by Node $copied ($why), link steps $steps, unseen $unseen"
echo "engine: Yarn's cache folder: $cache"
[ "$outcome" = installed ] || { echo "engine: did not install ($outcome)"; exit 1; }
if [ "$min_clones" = 0 ]; then
  [ "$cloned" = 0 ] || { echo "engine: cloned $cloned files, expected none"; exit 1; }
else
  [ "$cloned" -ge "$min_clones" ] || { echo "engine: cloned $cloned files, expected at least $min_clones"; exit 1; }
fi
[ "$min_clones" = 0 ] || [ "$unseen" = False ] || { echo "engine: Yarn linked but the hook saw no link step"; exit 1; }
[ "$(uname)" = Darwin ] && echo "engine: $(python3 "$here/scripts/shared-blocks.py" "$work/engine" "$ce")"

"$bin" diff --meta --normalize-roots "$work/plain1" "$work/engine" --a-run "$run_plain1" --b-run "$run_engine" \
  --a-cache "$c1" --b-cache "$ce" > "$work/diff-engine.txt"
rc=$?
if [ $rc != 0 ] && [ $rc != 1 ]; then echo "matchapack diff failed (exit $rc)"; exit 1; fi
if [ $rc = 0 ]; then
  echo "plain vs engine: IDENTICAL"
  [ $plain_rc = 0 ] && exit 0
  exit 3
fi
# A difference between plain1 and the engine that the two plain runs also
# have, on the same path and in the same class, is plain Yarn's own
# variation, not the engine's (scripts/yarn1-own-diffs.sh).
"$bin" diff --meta --normalize-roots "$work/plain1" "$work/plain2" --a-run "$run_plain1" --b-run "$run_plain2" --a-cache "$c1" --b-cache "$c2" --classes > "$work/classes-plain.txt" 2>/dev/null
r1=$?
"$bin" diff --meta --normalize-roots "$work/plain1" "$work/engine" --a-run "$run_plain1" --b-run "$run_engine" --a-cache "$c1" --b-cache "$ce" --classes > "$work/classes-engine.txt" 2>/dev/null
r2=$?
{ [ $r1 -le 1 ] && [ $r2 -le 1 ]; } || { echo "matchapack diff --classes failed (exit $r1, $r2)"; exit 1; }
own=$("$here/scripts/yarn1-own-diffs.sh" "$work/classes-plain.txt" "$work/classes-engine.txt") \
  || { echo "comparing the engine's differences with plain's failed (yarn1-own-diffs.sh)"; exit 1; }
if [ -z "$own" ]; then
  echo "plain vs engine: differs only where plain varies ($(wc -l < "$work/classes-engine.txt" | tr -d ' ') differences; $work/diff-engine.txt)"
  exit 3
fi
echo "plain vs engine: ENGINE DIFFERS ($(printf '%s\n' "$own" | wc -l | tr -d ' ') differences not explained by plain's variation; full list: $work/diff-engine.txt)"
grep -v '^    ' "$work/diff-engine.txt" | grep -v '^  written during\|^  from each run' || true
printf '%s\n' "$own" | head -20 | sed 's/^/    /'
exit 1
