#!/usr/bin/env bash
# The npm upgrade routine (#11): run the harness on the npm on PATH, and when
# every case is identical to `npm ci`, record its major.minor in
# <config dir>/npm-verified (~/.config/thinclone, or $THINCLONE_CONFIG_DIR),
# which thinclone's version gate reads next to the built-in VERIFIED_NPM.
#   verify-npm.sh [--no-record] <work-dir> [extra project dir...]
# Cases: the fixtures (hidden lockfile byte-identical), script-env through the
# shell function, legacy-peer with legacy-peer-deps, then each extra project
# (a real repository is worth adding: the fixtures are small). CI runs it with
# --no-record.
set -uo pipefail
record=1
if [ "${1-}" = --no-record ]; then record=0; shift; fi
work=${1:?usage: verify-npm.sh [--no-record] <work-dir> [extra project dir...]}; shift
here=$(cd "$(dirname "$0")/.." && pwd)
bin=$here/target/release/thinclone
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 2; }
mkdir -p "$work"
npm_version=$(npm -v) || { echo "no npm on PATH" >&2; exit 2; }
echo "npm $npm_version, node $(node -v), $("$bin" --version)"

# The optional-upward fixture installs from a local tarball server.
server=
if ! curl -sf -o /dev/null http://127.0.0.1:48321/lc-q-1.0.0.tgz; then
  python3 -m http.server 48321 --bind 127.0.0.1 --directory "$here/fixtures/optional-upward/tarballs" >/dev/null 2>&1 &
  server=$!
  for _ in $(seq 100); do curl -sf -o /dev/null http://127.0.0.1:48321/lc-q-1.0.0.tgz && break; sleep 0.1; done
fi
trap '[ -n "$server" ] && kill "$server" 2>/dev/null' EXIT

fail=0
case_() { # name env-assignments... -- source
  local name=$1; shift
  local envs=()
  while [ "$1" != -- ]; do envs+=("$1"); shift; done
  shift
  local out="$work/$name.txt"
  if env THINCLONE_VERIFY_BYTES=1 ${envs[@]+"${envs[@]}"} "$here/scripts/verify.sh" "$1" "$work/$name" > "$out" 2>&1; then
    echo "ok    $name ($(grep -m1 '^npm ci:' "$out"))"
  else
    fail=1; echo "FAIL  $name (log: $out)"; tail -15 "$out" | sed 's/^/      /'
  fi
}
for f in small stale-flags optional-upward optional-engines legacy-peer; do
  case_ "$f" -- "$here/fixtures/$f"
done
# script-env records the npm environment scripts see; the user's flags
# (--no-audit) reach thinclone only through the shell function.
case_ script-env THINCLONE_VERIFY_VIA=wrapper -- "$here/fixtures/script-env"
case_ legacy-peer-on npm_config_legacy_peer_deps=true -- "$here/fixtures/legacy-peer"
for p in "$@"; do
  case_ "project-$(basename "$p")" -- "$p"
done

if [ $fail != 0 ]; then
  echo "npm $npm_version: NOT verified; thinclone keeps handing npm ci to npm on it"
  exit 1
fi
mm=$(echo "$npm_version" | cut -d. -f1-2)
if [ $record = 0 ]; then
  echo "npm $npm_version: every case identical (not recorded: --no-record)"
  exit 0
fi
cfg=${THINCLONE_CONFIG_DIR:-$HOME/.config/thinclone}
mkdir -p "$cfg"
if grep -qsE "^$mm([[:space:]#]|\$)" "$cfg/npm-verified"; then
  echo "npm $npm_version: every case identical; $mm was already in $cfg/npm-verified"
else
  echo "$mm  # $(date +%F): scripts/verify-npm.sh, npm $npm_version, $("$bin" --version)${*:+, plus $*}" >> "$cfg/npm-verified"
  echo "npm $npm_version: every case identical; recorded $mm in $cfg/npm-verified"
fi
echo "(for every machine: add \"$mm\" to VERIFIED_NPM in src/wrapper.rs)"
