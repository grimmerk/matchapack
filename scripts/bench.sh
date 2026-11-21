#!/usr/bin/env bash
# The benchmark behind the README's disk, time and CPU numbers (issue grimmerk/matchapack-private#7):
# plain npm / Yarn 1 against matchapack on public projects pinned to a
# commit, in interleaved rounds. macOS (APFS) only.
#
#   scripts/bench.sh --work DIR [--rounds N] [--projects small,medium,large,yarn]
#                    [--cases ...] [--bench-id ID] [--ac] [options of scripts/bench.py]
#   scripts/bench.sh --work DIR --summary-only [--bench-id ID]
#   scripts/bench.sh --work DIR --cleanup      (keeps logs/ and results/)
#
# Projects (scripts/bench.py, PROJECTS): small = fixtures/small of this
# repository; medium = zwave-js/zwave-js-ui (the Linux CI job's project);
# large = apache/superset's superset-frontend (npm workspaces); yarn =
# cornerstonejs/cornerstone3D (Yarn 1 workspaces, --ignore-scripts).
# npm cases: empty store; first use per npm version (store filled by
# another npm, --fill-npm, default 11.10.1); a fresh worktree with a warm
# store; a reinstall over node_modules. Yarn 1 cases: empty Yarn cache; a
# fresh worktree with a warm cache; already installed; an incremental
# install after a real lockfile bump.
# matchapack runs as the shell function runs it (`matchapack npm ci`,
# `matchapack yarn install …`), with MATCHAPACK_NO_FALLBACK=1, so a decline
# fails the run instead of timing plain npm or Yarn.
#
# Isolation: everything is under DIR (never under ~/git): matchapack's
# store, config, logs and trial scope (MATCHAPACK_PATHS=DIR), npm's cache,
# Yarn's cache and offline mirror; NPM_CONFIG_USERCONFIG,
# NPM_CONFIG_GLOBALCONFIG and YARN_USERCONFIG name empty files, so the
# user's npmrc and yarnrc are neither read nor used. The real
# ~/.cache/matchapack, ~/.config/matchapack, ~/.npm and Yarn's cache are
# checked before and after (exit 4 when changed).
#
# Unattended: runs under `caffeinate -i` (no idle sleep while it runs).
# --ac uses `caffeinate -s` instead (no system sleep, on AC power only).
# Neither keeps a MacBook awake with its lid closed (no external display):
# leave the lid open. Noise: each run records the load average and the CPU
# of Spotlight's indexers and of a virtual machine at its start, and waits
# (--load-wait, default 60 s) for the load to fall below --load-max and
# Spotlight to go quiet; a run that starts anyway is flagged. A DIR whose
# name ends in .noindex is not indexed by Spotlight. A timeout per
# install (--timeout, default 1800 s); every timed run is one JSON line,
# written whole; the same --bench-id resumes an interrupted run. While it
# runs, /tmp/matchapack-bench-running holds its pid, for other agents on
# the machine to see (and not start heavy work); it is removed on exit,
# however the run ends.
# Output: DIR/results/summary.md (the tables), runs.jsonl, projects.jsonl,
# meta.jsonl, sentinels.jsonl; DIR/logs/bench.out is this run's output.
set -uo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
[ "$(uname)" = Darwin ] || { echo "bench.sh: macOS only" >&2; exit 2; }
[ -x "$here/target/release/matchapack" ] || { echo "build first: cargo build --release" >&2; exit 2; }
caff=-i
args=()
work=
prev=
for a in "$@"; do
  case $a in
    --ac) caff=-s ;;
    *) args+=("$a") ;;
  esac
  [ "$prev" = --work ] && work=$a
  case $a in --work=*) work=${a#--work=} ;; esac
  prev=$a
done
[ -n "$work" ] || { echo "usage: bench.sh --work DIR [options]  (scripts/bench.py --help)" >&2; exit 2; }
case " ${args[*]} " in
  *" --summary-only "*|*" --cleanup "*|*" --help "*|*" -h "*)
    exec python3 -I "$here/scripts/bench.py" "${args[@]}" ;;
esac
mkdir -p "$work/logs" || exit 2
flag=/tmp/matchapack-bench-running
if [ -e "$flag" ] && kill -0 "$(cat "$flag" 2>/dev/null)" 2>/dev/null; then
  echo "bench.sh: another benchmark runs (pid $(cat "$flag"), $flag)" >&2; exit 2
fi
echo $$ > "$flag"
cleanup_flag() { [ "$(cat "$flag" 2>/dev/null)" = $$ ] && rm -f "$flag"; }
trap cleanup_flag EXIT
# Not exec: the EXIT trap removes the flag file however python ends. A
# signal to this script is passed on to the bench (which stops the install
# it runs, keeps the runs recorded so far, and exits); bench.py also writes
# its output to DIR/logs/bench.out.
child=
# TERM for every signal: a background job of a non-interactive shell
# ignores INT, and bench.py stops cleanly on TERM.
forward() { [ -n "$child" ] && kill -TERM "$child" 2>/dev/null; }
trap forward TERM INT HUP
# No idle sleep while this script runs (-w: until this pid exits).
caffeinate "$caff" -w $$ &
python3 -I "$here/scripts/bench.py" "${args[@]}" &
child=$!
rc=0
while :; do
  wait "$child"; rc=$?
  kill -0 "$child" 2>/dev/null || break
done
exit "$rc"
