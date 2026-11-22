#!/usr/bin/env bash
# The Yarn 1 harness: plain Yarn 1 run twice on fresh copies of a project,
# compared with `matchapack diff --meta` (paths, types, modes, contents,
# symlink targets, .yarn-integrity bytes, mtimes to the nanosecond, uid, gid,
# xattr names). Plain against plain must come out identical before anything
# is compared with an engine; when it does not, the run says "plain varies"
# and shows the evidence (TESTING.md, "The Yarn 1 harness"). With --engine yarn1, a
# third copy is installed by `matchapack yarn` with the same cache and
# compared with the first plain run.
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
#                      (default 1; 0 for a case that must clone nothing;
#                      `any`: no count required, as for a real project
#                      whose files may all be among Yarn's first copies)
#   --args-as-given    the installs get the yarn args given, --non-interactive
#                      and the cache folder only: no --frozen-lockfile
#                      (scripts/verify-sample.sh --yarn replays a logged
#                      run with that run's own flags)
#   --command 'WORDS'  after the install, run `yarn WORDS` in every tree:
#                      plain Yarn in the prime run, in plain1 and plain2;
#                      `matchapack yarn WORDS` in the engine tree
#                      (docs/architecture.md, "Yarn commands"). The install before it is plain
#                      Yarn in every tree, so a difference is the command's.
#                      WORDS are split at spaces; --non-interactive is added
#                      after them (no --frozen-lockfile), and the run's cache
#                      is given as YARN_CACHE_FOLDER, not --cache-folder: a
#                      workspace command passes its child only the words
#                      after the name, and a case may give cache options of
#                      its own in WORDS. The command's exit status must
#                      be the same in every run (it may fail: then the
#                      engine's log line must say yarn_failed with that
#                      status); the comparison covers the install and the
#                      command. Prints whether each package.json and
#                      yarn.lock is byte-identical between plain1 and the
#                      engine, and which of them the command changed. Not
#                      with --then.
#   --command-setup engine  with --command: the engine tree's install is
#                      the engine's too
#   --command-before 'WORDS'  with --command: plain `yarn WORDS` in every tree
#                      after the install and before the command (a tree an
#                      earlier command left, which a fresh install from its
#                      lockfile would lay out otherwise)
#   --command-fresh    with --command: no install before the command; it
#                      runs on the copy as it is, with no node_modules (a
#                      command that started in a fresh clone, replayed by
#                      scripts/verify-sample.sh --yarn)
#   --command-tty      with --command: run it in a pseudo-terminal that
#                      takes every update `yarn upgrade-interactive` offers
#                      (scripts/yarn1-tty.py), without --non-interactive
# Environment: MATCHAPACK_YARN_NODE (node binary, default the node on PATH),
# MATCHAPACK_YARN_JS (yarn.js, default the yarn on PATH, resolved); the
# engine's test switches (MATCHAPACK_YARN_FAIL_EVERY, …) reach the engine run.
# MATCHAPACK_VERIFY_YARN_DAMAGE (tests only): a command (sh -c) run in the
# engine tree after its run, to show that a damaged tree is reported.
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
command= command_setup=plain command_tty=0 command_before= frozen=(--frozen-lockfile) command_fresh=0
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
    --command) command=${2:?--command needs the words of a yarn command}; shift 2 ;;
    --command-setup) command_setup=${2:?--command-setup needs plain or engine}; shift 2
      case $command_setup in plain|engine) ;; *) echo "--command-setup $command_setup: plain or engine" >&2; exit 2 ;; esac ;;
    --command-tty) command_tty=1; shift ;;
    --command-fresh) command_fresh=1; shift ;;
    --command-before) command_before=${2:?--command-before needs the words of a yarn command}; shift 2 ;;
    --engine-min-clones) min_clones=${2:?--engine-min-clones needs a number}; shift 2
      case $min_clones in any) ;; ''|*[!0-9]*) echo "--engine-min-clones $min_clones: a number or any" >&2; exit 2 ;; esac ;;
    --args-as-given) frozen=(); shift ;;
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
[ -z "$command" ] || [ -z "$then" ] || { echo "--command is not with --then" >&2; exit 2; }
{ [ "$command_setup" = plain ] && [ $command_tty = 0 ] && [ -z "$command_before" ] && [ $command_fresh = 0 ]; } || [ -n "$command" ] \
  || { echo "--command-setup / --command-tty / --command-before / --command-fresh need --command" >&2; exit 2; }
[ $command_fresh = 0 ] || { [ "$command_setup" = plain ] && [ -z "$command_before" ]; } \
  || { echo "--command-fresh: no install before the command, so no --command-setup engine or --command-before" >&2; exit 2; }
read -ra cmd_words <<< "$command"
read -ra before_words <<< "$command_before"
[ -z "$command" ] || [ ${#cmd_words[@]} -gt 0 ] || { echo "--command: no words" >&2; exit 2; }
case $subdir in /*|..|../*|*/..|*/../*) echo "--subdir $subdir: a relative path inside the project" >&2; exit 2 ;; esac
[ -d "$src/$subdir" ] || { echo "--subdir $subdir: no such directory in $src" >&2; exit 2; }
# A symlink in the path could lead out of the project: Yarn would then
# install (and run scripts) in another checkout. The resolved directory must
# stay inside the resolved project.
sub_real=$(cd "$src/$subdir" && pwd -P) && src_real0=$(cd "$src" && pwd -P) || exit 2
case "$sub_real/" in "$src_real0/"*) ;; *) echo "--subdir $subdir resolves outside the project ($sub_real)" >&2; exit 2 ;; esac
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
  "${work:?}/cache-e" "${work:?}/runs" "${work:?}/mp" "${work:?}/engine.json" "${work:?}/diff-engine.txt" "${work:?}/command-status"
echo "yarn $yver, node $("$node" -v), $("$bin" --version), $cache_mode cache, work $work"

# Fixtures install from a tarball server of this run, on a free port
# (fixture-server.sh): the one test-yarn1.sh started when it runs this
# script, else one of its own. A fixture of this checkout that names the
# placeholder 127.0.0.1:48321 is used through the run's copy of fixtures/,
# whose URLs name that port.
FX_PID=
fx_named() { grep -qs '127.0.0.1:48321' "$1/yarn.lock" "$1/package.json" "$1/.yarnrc"; }
if [ -n "${MATCHAPACK_TEST_FX-}" ] || fx_named "$src" || { [ -n "$then" ] && fx_named "$then"; }; then
  . "$here/scripts/fixture-server.sh"; fixtures_serve "$work"
  fx_real=$(cd "$here/fixtures" && pwd -P)
  for v in src then; do
    [ -n "${!v}" ] || continue
    r=$(cd "${!v}" && pwd -P)
    case "$r/" in "$fx_real/"*) printf -v "$v" '%s' "$FX/${r#"$fx_real/"}" ;; esac
    if fx_named "${!v}"; then
      echo "${!v} names 127.0.0.1:48321, the fixtures' placeholder: give a fixture under $here/fixtures (its copy is served)" >&2; exit 2
    fi
  done
fi
trap '[ -n "$FX_PID" ] && kill "$FX_PID" 2>/dev/null' EXIT

# Copies keep the source's times, modes and group (-p), so files outside
# node_modules compare equal. Not clones (cp -c): a clone takes the group of
# the directory it is made in, not the source's.
if [ "$(uname)" = Darwin ]; then cpp() { cp -p -R "$1" "$2"; }; else cpp() { cp -a "$1" "$2"; }; fi
now_ns() { python3 -c 'import time; print(time.time_ns())'; }
# install <name> <cache> <node> <how> [args]: one install in $work/<name>;
# <how> is plain or engine. Exit status: the install's.
install() {
  local name=$1 cache=$2 n=$3 how=$4 tree_real dir_real
  shift 4
  # Checked in each copy: an absolute symlink copied as is would lead back to
  # the source or elsewhere.
  tree_real=$(cd "$work/$name" && pwd -P) && dir_real=$(cd "$work/$name/$subdir" && pwd -P) || return 2
  case "$dir_real/" in "$tree_real/"*) ;; *) echo "--subdir $subdir resolves outside the copy ($dir_real)" >&2; return 2 ;; esac
  if [ "$how" = plain ]; then
    (cd "$work/$name/$subdir" && env -u GITHUB_TOKEN YARN_DISABLE_SELF_UPDATE_CHECK=true \
      "$n" "$yarnjs" install ${frozen[@]+"${frozen[@]}"} --non-interactive --cache-folder "$cache" "$@") >> "$work/$name.log" 2>&1
  else
    # The engine runs the node on PATH and the yarn.js given; a decline
    # exits 3 (MATCHAPACK_NO_FALLBACK) instead of running plain Yarn.
    (cd "$work/$name/$subdir" && env -u GITHUB_TOKEN -u MATCHAPACK_ENGINE -u MATCHAPACK_PATHS -u MATCHAPACK \
      YARN_DISABLE_SELF_UPDATE_CHECK=true PATH="$(dirname "$n"):$PATH" MATCHAPACK_YARN_JS="$yarnjs" \
      MATCHAPACK_STORE="$work/mp/store" MATCHAPACK_LOG_DIR="$work/mp/log" MATCHAPACK_CONFIG_DIR="$work/mp/config" MATCHAPACK_NO_FALLBACK=1 \
      "$bin" yarn install ${frozen[@]+"${frozen[@]}"} --non-interactive --cache-folder "$cache" "$@") >> "$work/$name.log" 2>&1
  fi
}
# run_command <name> <cache> <node> <how> <tty 0|1> <words...>: `yarn
# WORDS` (--command, --command-before) in $work/<name>, after its install;
# plain or engine as for install. Every Yarn of it, a workspace command's
# child included, gets the run's cache as YARN_CACHE_FOLDER (an option in
# WORDS overrides it, as it would for a user). Exit status: the command's.
run_command() {
  local name=$1 cache=$2 n=$3 how=$4 tty=$5 run=() extra=(--non-interactive)
  shift 5
  local cmd_words=("$@")
  [ "$tty" = 1 ] && extra=()
  if [ "$how" = plain ]; then run=("$n" "$yarnjs"); else run=("$bin" yarn); fi
  [ "$tty" = 1 ] && run=(python3 "$here/scripts/yarn1-tty.py" -- "${run[@]}")
  echo "--- yarn ${cmd_words[*]} ($how)" >> "$work/$name.log"
  if [ "$how" = plain ]; then
    (cd "$work/$name/$subdir" && env -u GITHUB_TOKEN YARN_DISABLE_SELF_UPDATE_CHECK=true YARN_CACHE_FOLDER="$cache" \
      "${run[@]}" "${cmd_words[@]}" ${extra[@]+"${extra[@]}"}) >> "$work/$name.log" 2>&1 < /dev/null
  else
    (cd "$work/$name/$subdir" && env -u GITHUB_TOKEN -u MATCHAPACK_ENGINE -u MATCHAPACK_PATHS -u MATCHAPACK \
      YARN_DISABLE_SELF_UPDATE_CHECK=true YARN_CACHE_FOLDER="$cache" PATH="$(dirname "$n"):$PATH" MATCHAPACK_YARN_JS="$yarnjs" \
      MATCHAPACK_STORE="$work/mp/store" MATCHAPACK_LOG_DIR="$work/mp/log" MATCHAPACK_CONFIG_DIR="$work/mp/config" MATCHAPACK_NO_FALLBACK=1 \
      "${run[@]}" "${cmd_words[@]}" ${extra[@]+"${extra[@]}"}) >> "$work/$name.log" 2>&1 < /dev/null
  fi
}
# yarn_run <name> <cache> <node> <how> [args]: copy the project to
# $work/<name>, install (twice with --then; then the command with
# --command); sets run_<name>=START:END.
yarn_run() {
  local name=$1 cache=$2 n=$3 how=$4 s e rc
  shift 4
  # A partial copy could still install and compare as identical: stop.
  cpp "$src" "$work/$name" || { echo "copying $src to $work/$name failed"; exit 1; }
  : > "$work/$name.log"
  s=$(now_ns)
  local ihow=$how
  [ -n "$command" ] && [ "$command_setup" = plain ] && ihow=plain
  if [ $command_fresh = 1 ]; then rc=0; else install "$name" "$cache" "$n" "$ihow" "$@"; rc=$?; fi
  if [ $rc = 0 ] && [ -n "$command_before" ]; then
    run_command "$name" "$cache" "$n" plain 0 "${before_words[@]}"; rc=$?
  fi
  if [ $rc = 0 ] && [ -n "$command" ]; then
    run_command "$name" "$cache" "$n" "$how" "$command_tty" "${cmd_words[@]}"
    echo "$name $?" >> "$work/command-status"
  fi
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
if [ -n "$engine" ] && [ -n "${MATCHAPACK_VERIFY_YARN_DAMAGE:-}" ]; then
  (cd "$work/engine" && sh -c "$MATCHAPACK_VERIFY_YARN_DAMAGE") || { echo "MATCHAPACK_VERIFY_YARN_DAMAGE failed"; exit 1; }
  echo "engine tree damaged on purpose (MATCHAPACK_VERIFY_YARN_DAMAGE)"
fi

# --- The command (--command) -------------------------------------------------
# The same exit status in every run (the prime run's included).
cmd_rc=
if [ -n "$command" ]; then
  statuses=$(awk '{ print $2 }' "$work/command-status" | sort -u)
  if [ "$(printf '%s\n' "$statuses" | wc -l | tr -d ' ')" != 1 ]; then
    echo "command: yarn ${cmd_words[*]}: the exit status differs between runs: $(tr '\n' ' ' < "$work/command-status")"
    for n in prime plain1 plain2 engine; do [ -f "$work/$n.log" ] && { echo "  $n:"; tail -6 "$work/$n.log" | sed 's/^/    /'; }; done
    exit 1
  fi
  cmd_rc=$statuses
  echo "command: yarn ${cmd_words[*]}: exit $cmd_rc in every run"
  # Each package.json and yarn.lock of the project (outside node_modules):
  # did the command change it (plain1 against the source), and is it the
  # same bytes in plain1 and the engine (also compared below, as files of
  # the trees; these lines are for the tests).
  manifests=$(cd "$work/plain1" && find . -name node_modules -prune -o \( -name package.json -o -name yarn.lock \) -type f -print | sed 's|^\./||' | LC_ALL=C sort)
  changed_pj= changed_lock=no same_pj=true same_lock=true
  while IFS= read -r m; do
    [ -n "$m" ] || continue
    if ! cmp -s "$src/$m" "$work/plain1/$m"; then
      case $m in */yarn.lock|yarn.lock) changed_lock=yes ;; *) changed_pj="$changed_pj $m" ;; esac
    fi
    if [ -n "$engine" ] && ! cmp -s "$work/plain1/$m" "$work/engine/$m"; then
      case $m in */yarn.lock|yarn.lock) same_lock=false ;; *) same_pj=false ;; esac
    fi
  done <<< "$manifests"
  echo "command: changed package.json:${changed_pj:- none}"
  echo "command: changed yarn.lock: $changed_lock"
  if [ -n "$engine" ]; then
    echo "command: package.json byte-identical (plain1, engine): $same_pj"
    echo "command: yarn.lock byte-identical (plain1, engine): $same_lock"
  fi
fi

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
words = sys.argv[2:]
args = "yes" if (r.get("args") or [])[:len(words)] == words else "no: %s" % r.get("args")
print(r.get("outcome"), y.get("cloned", 0), y.get("cloned_bytes", 0), y.get("copied_total", 0), y.get("link_steps", 0), y.get("unseen"), copied, cache,
      r.get("code"), r.get("command"), args, (y.get("declined") or "-").replace("\t", " "), sep="\t")
' "$work/engine.json" ${command:+"${cmd_words[@]}"} 2>/dev/null) || { echo "engine: no log line ($work/mp/log/log.jsonl)"; exit 1; }
IFS=$'\t' read -r outcome cloned cbytes copied steps unseen why cache code logged_cmd args_ok child_declined <<< "$stats"
echo "engine: $outcome, cloned $cloned files ($(( cbytes / 1048576 )) MiB), copied by Node $copied ($why), link steps $steps, unseen $unseen"
echo "engine: Yarn's cache folder: $cache"
if [ -n "$command" ]; then
  # The log line is the command's (not the install's before it).
  echo "engine: logged command $logged_cmd, its arguments as given: $args_ok"
  [ "$args_ok" = yes ] || { echo "engine: the last log line is not the command's"; exit 1; }
  if [ "$cmd_rc" != 0 ]; then
    { [ "$outcome" = yarn_failed ] && [ "$code" = "$cmd_rc" ]; } || { echo "engine: the command failed with $cmd_rc, logged $outcome (code $code)"; exit 1; }
    min_clones=0
  elif [ "$child_declined" != - ]; then
    # The hook in the Yarn a workspace command started declined: that Yarn
    # ran plain, unseen by the hook. Only a case that clones nothing.
    echo "engine: $child_declined"
    [ "$min_clones" = 0 ] || { echo "engine: the workspace's Yarn ran without the hook"; exit 1; }
  elif [ "$min_clones" = 0 ]; then
    # A case that expects no clone may also have nothing to link (an
    # upgrade-interactive with nothing to upgrade returns before Yarn's
    # Install).
    echo "engine: link steps $steps (none required: --engine-min-clones 0)"
  else
    [ "$steps" -ge 1 ] 2>/dev/null || { echo "engine: the hook saw no link step"; exit 1; }
  fi
fi
[ "$outcome" = installed ] || [ "${cmd_rc:-0}" != 0 ] || { echo "engine: did not install ($outcome)"; exit 1; }
if [ "$min_clones" = 0 ]; then
  [ "$cloned" = 0 ] || { echo "engine: cloned $cloned files, expected none"; exit 1; }
elif [ "$min_clones" != any ]; then
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
