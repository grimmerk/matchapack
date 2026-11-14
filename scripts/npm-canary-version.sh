#!/usr/bin/env bash
# One npm release in the canary (.github/workflows/npm-canary.yml), Linux job:
#   npm-canary-version.sh <item.json> <work dir> <out dir>
# <item.json> is one entry of the poll's plan ({"version", "baseline",
# "reason", "newMajor", ...}). Runs the npm suites on that release with both
# overrides (the canary tests a release whatever the gates say), installs the
# baseline release next to it, and records the fingerprint and the diff of
# the tracked functions. <out dir> gets item.json, suites.json, the suites'
# logs, inspect-new.json, inspect-base.json, compare.json, diffs/ and
# result.json (scripts/npm-canary.js has the formats). Exit status: the
# suites'; everything else is recorded even when they fail.
set -uo pipefail
item=${1:?usage: npm-canary-version.sh <item.json> <work dir> <out dir>}; work=${2:?}; out=${3:?}
here=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$work" "$out"; work=$(cd "$work" && pwd -P); out=$(cd "$out" && pwd -P)
field() { node -e 'const v = require(process.argv[1])[process.argv[2]]; process.stdout.write(v == null ? "" : String(v))' "$(cd "$(dirname "$item")" && pwd)/$(basename "$item")" "$1"; }
version=$(field version); baseline=$(field baseline)
case $version in [0-9]*.[0-9]*.[0-9]*) ;; *) echo "bad version in $item" >&2; exit 2 ;; esac
cp "$item" "$out/item.json"
canary() { node --expose-internals "$here/scripts/npm-canary.js" "$@"; }

MATCHAPACK_NPM_VERSIONS=any MATCHAPACK_NPM_FINGERPRINT=any "$here/scripts/npm-suites.sh" "$version" "$work/new"; rc=$?
for f in suites.json test-inject.log verify-npm.log npm-install.log; do [ -f "$work/new/$f" ] && cp "$work/new/$f" "$out/"; done

if [ -d "$work/new/npm/node_modules/npm" ]; then
  canary inspect "$work/new/npm/node_modules/npm" "$out/inspect-new.json"
  if [ -n "$baseline" ]; then
    if command npm install --prefix "$work/base" --no-audit --no-fund --no-package-lock --ignore-scripts \
         --cache "$work/npm-install-cache" "npm@$baseline" > "$work/base-install.log" 2>&1; then
      canary inspect "$work/base/node_modules/npm" "$out/inspect-base.json" && canary compare "$out/inspect-base.json" "$out/inspect-new.json" "$out"
    else
      echo "could not install the baseline npm@$baseline"; tail -5 "$work/base-install.log"
    fi
  fi
fi
canary result "$out"
exit "$rc"
