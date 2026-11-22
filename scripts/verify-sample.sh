#!/usr/bin/env bash
# Sampled verification of real runs during the trial (issue grimmerk/matchapack-private#77).
#   verify-sample.sh [--yarn] [N] [project-dir...]
# With no directories: takes the N (default 2) most recent distinct projects
# whose newest run in the run log (~/.cache/matchapack/log.jsonl) can be
# replayed, and replays that run on copies of the project, plain and through
# matchapack, and compares the trees. One JSON line per project goes to
# ~/.cache/matchapack/verify.jsonl, shown by `matchapack report`. Each
# replay installs the project three or four times, so run it now and then,
# not per install. The copies are removed after each comparison
# (MATCHAPACK_VERIFY_KEEP=1 keeps them).
#
# npm (default):
# - `npm ci`, with its logged words: on copy-on-write copies of the whole
#   project, npm ci twice (A, A2) and `matchapack npm ci` once (B); paths
#   that differ between A and A2 are build noise and ignored
#   (scripts/verify.sh). Directories given are replayed as a plain `npm ci`
#   of the project as it is now.
# - `npm install` / `update` / `uninstall` (and their aliases) that started
#   with no node_modules (part 1b): the same words, on copies of the
#   manifests as they were before the run (package.json files, the
#   lockfile, .npmrc; no node_modules), plain npm twice and `matchapack npm`
#   once; package.json and the lockfile must also be byte-identical.
# --yarn:
# - `yarn` / `yarn install`, with or without --frozen-lockfile: replayed
#   with the logged run's Yarn, node, cache folder and flags on copies of
#   the manifests (every package.json, yarn.lock, .yarnrc and .npmrc outside
#   node_modules): plain Yarn three times (one to make sure the cache holds
#   every package, two compared) and `matchapack yarn` once, compared by
#   scripts/verify-yarn1.sh --engine yarn1.
# - `yarn add` / `upgrade` / `remove` (and `yarn workspace <name> …`) that
#   started with no node_modules: the same command from the state before
#   the run (verify-yarn1.sh --command … --command-fresh).
# A run is replayed from the files it ran on: for a full install, the
# lockfile and manifests as the run left them; for a command that started
# empty, as they were before it (the files now, while they still have the
# hashes the run logged, or the project's git HEAD). Otherwise, and when the
# directory, node, npm or Yarn is gone or another version, or the run cannot
# be repeated, the project is recorded as SKIPPED with the reason.
# The caches and the store are the ones the runs used (the user's,
# normally): plain npm and Yarn may add to them, as they would in a real
# install, and matchapack's own runs fill the store with packages it lacks
# and may replace an entry that differs from npm's extraction, as any
# install does. A sample says IDENTICAL only when the replay's inputs equal
# what the run recorded (the files it hashed, the words it logged),
# matchapack cloned at least as much in it as in the run, and every check
# passed (TESTING.md, "Sampled verification of real runs").
# MATCHAPACK_LOG_DIR_REAL: the log directory (default ~/.cache/matchapack).
set -uo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
logdir=${MATCHAPACK_LOG_DIR_REAL:-$HOME/.cache/matchapack}
yarn=0
if [ "${1-}" = --yarn ]; then yarn=1; shift; fi
n=2
if [[ "${1-}" =~ ^[0-9]+$ ]]; then n=$1; shift; fi
dirs=("$@")
[ -x "$here/target/release/matchapack" ] || { echo "build first: cargo build --release" >&2; exit 2; }
# Picked, replayed and recorded by the helper, one project at a time.
exec python3 -I "$here/scripts/verify-sample.py" "$([ $yarn = 1 ] && echo yarn || echo npm)" "$logdir" "$n" "$here" ${dirs[@]+"${dirs[@]}"}
