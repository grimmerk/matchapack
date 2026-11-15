#!/usr/bin/env bash
# Sampled verification during the trial.
#   verify-sample.sh [--yarn] [N] [project-dir...]
# With no directories: takes the N (default 2) most recent distinct projects
# that matchapack installed (from ~/.cache/matchapack/log.jsonl). For each, on
# copy-on-write copies of the project: npm ci twice (A, A2) and matchapack once
# (B); paths that differ between A and A2 are build noise and ignored; then the
# harness compares A with B. One JSON line per project goes to
# ~/.cache/matchapack/verify.jsonl, shown by `matchapack report`.
# Costs a real npm ci twice per project, so run it now and then, not per install.
#
# --yarn: the same for `matchapack yarn` full installs (`yarn`, `yarn
# install`, with or without --frozen-lockfile; not add / upgrade / remove /
# upgrade-interactive / workspace, #77 part 2): the newest full install of
# each project is replayed with the logged run's Yarn, node, cache folder and
# flags, on copies of the project's manifests (every package.json, yarn.lock,
# .yarnrc and .npmrc outside node_modules): plain Yarn three times (one to
# make sure the cache holds every package, two compared) and `matchapack
# yarn` once, compared by scripts/verify-yarn1.sh --engine yarn1. A project
# whose yarn.lock or package.json changed since, whose directory, node or
# Yarn is gone, or whose run cannot be repeated, is recorded as SKIPPED with
# the reason. Directories given: the newest logged full install of each.
# The cache is the one the run used (the user's, normally): plain Yarn may
# add to it, as it would in a real install; nothing else changes it.
# MATCHAPACK_VERIFY_KEEP=1 keeps each Yarn sample's work directory.
# MATCHAPACK_LOG_DIR_REAL: the log directory (default ~/.cache/matchapack).
set -uo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
logdir=${MATCHAPACK_LOG_DIR_REAL:-$HOME/.cache/matchapack}
yarn=0
if [ "${1-}" = --yarn ]; then yarn=1; shift; fi
n=2
if [[ "${1-}" =~ ^[0-9]+$ ]]; then n=$1; shift; fi
dirs=("$@")
if [ $yarn = 1 ]; then
  # Yarn 1: picked, replayed and recorded by the helper (one at a time).
  [ -x "$here/target/release/matchapack" ] || { echo "build first: cargo build --release" >&2; exit 2; }
  exec python3 -I "$here/scripts/verify-sample.py" yarn "$logdir" "$n" "$here" ${dirs[@]+"${dirs[@]}"}
fi
if [ ${#dirs[@]} -eq 0 ]; then
  while IFS= read -r d; do [ -n "$d" ] && dirs+=("$d"); done < <(python3 -I "$here/scripts/verify-sample.py" pick-npm "$logdir/log.jsonl" "$n")
fi
[ ${#dirs[@]} -gt 0 ] || { echo "nothing to verify: no matchapack installs in $logdir/log.jsonl yet"; exit 0; }

work=$(mktemp -d "${TMPDIR:-/tmp}/matchapack-verify.XXXXXX")
trap 'rm -rf "$work"' EXIT
for d in "${dirs[@]}"; do
  echo "######## $d"
  out=$(MATCHAPACK_VERIFY_NOISE=1 MATCHAPACK_LOG_DIR="$work/logs" "$here/scripts/verify.sh" "$d" "$work/v" 2>&1)
  printf '%s\n' "$out" | grep -E 'RESULT|npm ci:|noise|npm ls:|stale in B|^  ' | head -20
  printf '%s\n' "$out" > "$work/out.txt"
  # The build verify.sh ran (target/release), which may differ from the one on PATH.
  build=$("$here/target/release/matchapack" --version 2>/dev/null | sed -n 's/.*(\(.*\)).*/\1/p')
  python3 -I "$here/scripts/verify-sample.py" record-npm "$logdir/verify.jsonl" "$d" "$work/out.txt" "${build:-unknown}"
  rm -rf "${work:?}/v"
done
