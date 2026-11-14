#!/usr/bin/env bash
# The npm upgrade routine (#11): run the harness on the npm on PATH.
#   verify-npm.sh [--no-record] <work-dir> [extra project dir...]
# Cases: the fixtures (hidden lockfile byte-identical; rootpath writes a file
# named after its own directory), script-env through the shell function,
# legacy-peer with legacy-peer-deps, then each extra project (a real
# repository is worth adding: the fixtures are small). Every case must have
# cloned something (a run in which the hook never fired would pass without
# testing it). Nothing is recorded: the version gate is by npm major
# (INJECT_NPM_MAJORS in src/wrapper.rs), and inside a major by the pacote
# fingerprint (src/npm-fingerprints.json; the cases run on any, and the
# first line says whether this npm's is listed). --no-record is accepted and ignored
# (it turned off recording into npm-verified, the classic engine's gate,
# removed in PR C2).
set -uo pipefail
if [ "${1-}" = --no-record ]; then shift; fi
work=${1:?usage: verify-npm.sh [--no-record] <work-dir> [extra project dir...]}; shift
here=$(cd "$(dirname "$0")/.." && pwd)
bin=$here/target/release/matchapack
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 2; }
mkdir -p "$work"
npm_version=$(npm -v) || { echo "no npm on PATH" >&2; exit 2; }
# What is certified is fixed here, never inherited: a caller's settings that
# loosen the comparison (ignored paths, noise filtering, another baseline,
# mtime handling) or route the install differently do not apply.
unset MATCHAPACK_VERIFY_IGNORE MATCHAPACK_VERIFY_NOISE MATCHAPACK_VERIFY_BASELINE MATCHAPACK_VERIFY_MTIME MATCHAPACK_VERIFY_VIA MATCHAPACK_VERIFY_BYTES MATCHAPACK_VERIFY_CLONES MATCHAPACK_VERIFY_META
# The engine is fixed too: a caller's MATCHAPACK_ENGINE other than inject
# would make every install an error.
unset MATCHAPACK_ENGINE
# The fixtures use a store of their own in the work directory, never the
# user's, unless the caller names one. verify.sh puts each case's trial log
# (MATCHAPACK_LOG_DIR) and its trial scope (MATCHAPACK_PATHS) in the work
# directory too; nothing else is written under ~/.cache/matchapack or
# ~/.config/matchapack (the run's helper files go to <store>/tmp).
export MATCHAPACK_STORE=${MATCHAPACK_STORE:-$(cd "$work" && pwd -P)/store}
export MATCHAPACK_VERIFY_CLONES=some
# npm 12 refuses tarballs from a non-registry URL unless told otherwise; the
# local fixtures install from 127.0.0.1 (as in test-inject.sh).
[ "${npm_version%%.*}" -ge 12 ] && export npm_config_allow_remote=all
echo "npm $npm_version, node $(node -v), $("$bin" --version)"
# The cases run whatever this npm's pacote fingerprint is (verify.sh); say
# whether the binary would take it without an override.
# npm's package directory as npm itself reports it (npm_execpath, its
# bin/npm-cli.js): also right behind a version manager's shim, where the
# `npm` on PATH is not a symlink into the package.
npm_dir=$(cd "$(dirname "$(npm exec -c 'printf %s "$npm_execpath"')")/.." && pwd)
fp=$(node "$here/src/inject.js" --fingerprint "$npm_dir")
if grep -q "\"$fp\"" "$here/src/npm-fingerprints.json"; then
  echo "pacote fingerprint $fp: listed in src/npm-fingerprints.json"
else
  echo "pacote fingerprint $fp: NOT listed in src/npm-fingerprints.json; add it once this passes (NOTES \"G-lite\")"
fi

# The optional-upward fixture installs from a local tarball server.
server=
if ! curl -sf -o /dev/null http://127.0.0.1:48321/lc-q-1.0.0.tgz; then
  python3 -m http.server 48321 --bind 127.0.0.1 --directory "$here/fixtures/optional-upward/tarballs" >/dev/null 2>&1 &
  server=$!
  for _ in $(seq 100); do curl -sf -o /dev/null http://127.0.0.1:48321/lc-q-1.0.0.tgz && break; sleep 0.1; done
fi
trap '[ -n "$server" ] && kill "$server" 2>/dev/null' EXIT

fail=0
case_() { # name env-assignments... -- source [matchapack install args...]
  local name=$1; shift
  local envs=()
  while [ "$1" != -- ]; do envs+=("$1"); shift; done
  shift
  local out="$work/$name.txt"
  local src=$1; shift
  if env MATCHAPACK_VERIFY_BYTES=1 ${envs[@]+"${envs[@]}"} "$here/scripts/verify.sh" "$src" "$work/$name" "$@" > "$out" 2>&1; then
    echo "ok    $name ($(grep -m1 '^npm ci:' "$out"))"
  else
    fail=1; echo "FAIL  $name (log: $out)"; tail -15 "$out" | sed 's/^/      /'
  fi
}
# rootpath: an install script names a file after its own directory, so A and
# B have another path there; verify.sh's --normalize-roots must make them the
# same (issue grimmerk/matchapack-private#48). workspaces: `npm ci` at the root of an npm workspaces
# project (links, a workspace's own nested node_modules and .bin, an install
# script), also compared with --meta (times outside the runs, uid, gid,
# xattr names).
for f in small stale-flags optional-upward optional-engines legacy-peer bundled rootpath; do
  case_ "$f" -- "$here/fixtures/$f"
done
case_ workspaces MATCHAPACK_VERIFY_META=1 -- "$here/fixtures/workspaces"
# The root's predependencies / dependencies / postdependencies scripts,
# which npm ci runs (the classic engine did not; NOTES "Inject engine").
case_ dep-event -- "$here/fixtures/dep-event"
# A failing optional dependency script: npm ci removes the package.
case_ optional-script-fail -- "$here/fixtures/optional-script-fail"
# The same package placed by per-file clones (the Linux placement).
case_ small-file-clones -- "$here/fixtures/small" --clone-mode file
# Tarballs where an extractor other than pacote's can give another tree
# (a path listed twice; .gitignore next to .npmignore): the store is
# filled through pacote.
case_ odd-tarballs -- "$here/fixtures/odd-tarballs"
# script-env records the npm environment scripts see; the user's flags
# (--no-audit) reach matchapack only through the shell function.
case_ script-env MATCHAPACK_VERIFY_VIA=wrapper -- "$here/fixtures/script-env"
case_ legacy-peer-on npm_config_legacy_peer_deps=true -- "$here/fixtures/legacy-peer"
for p in "$@"; do
  case_ "project-$(basename "$p")" -- "$p"
done

if [ $fail != 0 ]; then
  echo "npm $npm_version: matchapack is NOT identical to npm ci here (see the failing cases)"
  exit 1
fi
echo "npm $npm_version: every case identical"
