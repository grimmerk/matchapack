#!/usr/bin/env bash
# The npm suites (test-inject.sh and verify-npm.sh) on one npm version,
# installed into a scratch prefix: what PR CI runs per pinned npm line and
# what the npm canary runs per new release.
#   npm-suites.sh <npm version> <work dir>
# Put <work dir> on a filesystem that can clone (Linux: scripts/btrfs-scratch.sh;
# macOS: the runner's APFS). Everything goes into <work dir>: the npm copy
# (npm/), npm's cache for the suites (npm-cache/), each suite's work
# directory and log, and suites.json, the summary the canary reads:
#   {"npm", "node", "os", "fs", "suites": {"test-inject": {"status", "pass",
#    "fail", "skip", "failed": [...]}, "verify-npm": {...}}}
# Exit status 0 only when both suites passed. Needs the release binary
# (cargo build --release), python3, curl and zsh (the shell-function cases).
# The caller's MATCHAPACK_NPM_* overrides pass through (the canary sets
# MATCHAPACK_NPM_FINGERPRINT=any: it tests a release whatever the gate says).
set -uo pipefail
version=${1:?usage: npm-suites.sh <npm version> <work dir>}; work=${2:?usage: npm-suites.sh <npm version> <work dir>}
case $version in
  [0-9]*.[0-9]*.[0-9]*) ;;
  *) echo "not an npm version: $version" >&2; exit 2 ;;
esac
here=$(cd "$(dirname "$0")/.." && pwd)
[ -x "$here/target/release/matchapack" ] || { echo "build first: cargo build --release" >&2; exit 2; }
for t in python3 curl zsh; do command -v "$t" >/dev/null || { echo "missing: $t" >&2; exit 2; }; done
mkdir -p "$work"; work=$(cd "$work" && pwd -P)

# The npm under test, from the registry, into a prefix of its own. The
# caller's npm only installs it; no scripts run (npm's package has none).
rm -rf "${work:?}/npm"
command npm install --prefix "$work/npm" --no-audit --no-fund --no-package-lock --ignore-scripts \
  --cache "$work/npm-install-cache" "npm@$version" > "$work/npm-install.log" 2>&1 \
  || { echo "could not install npm@$version"; tail -5 "$work/npm-install.log"; exit 2; }
export PATH="$work/npm/node_modules/.bin:$PATH"
got=$(npm -v) || got=none
[ "$got" = "$version" ] || { echo "npm on PATH is $got, not $version" >&2; exit 2; }
export npm_config_cache=$work/npm-cache npm_config_update_notifier=false
if [ "$(uname)" = Darwin ]; then
  # mount prints "<device> on <dir> (apfs, local, ...)".
  fs=$(mount | awk -v d="$(df "$work" | awk 'NR==2 {print $1}')" '$1==d {sub(/^\(/,"",$4); sub(/,$/,"",$4); print $4; exit}')
else
  fs=$(stat -f -c %T "$work")
fi
echo "npm $version, node $(node -v), $(uname -sm), work on ${fs:-?}"

run() { # name script args...
  local name=$1 t0 rc; shift
  t0=$(date +%s)
  "$@" > "$work/$name.log" 2>&1; rc=$?
  echo "$name: exit $rc, $(( $(date +%s) - t0 )) s ($(grep -cE '^(PASS|ok) ' "$work/$name.log") passed, $(grep -cE '^FAIL ' "$work/$name.log") failed)"
  # Each failure with the indented lines the suite prints after it.
  awk '/^FAIL / {p=1; print; next} p && /^ / {print; next} {p=0}' "$work/$name.log" | head -60 | sed 's/^/    /'
  echo "$rc" > "$work/$name.status"
}
run test-inject "$here/scripts/test-inject.sh" "$work/inject"
run verify-npm "$here/scripts/verify-npm.sh" "$work/vi"

python3 - "$work" "$version" "$(node -v)" "$(uname -sm)" "${fs:-?}" <<'PY'
import json, re, sys
work, version, node, os_, fs = sys.argv[1:6]
suites = {}
for name in ("test-inject", "verify-npm"):
    log = open(f"{work}/{name}.log", errors="replace").read().splitlines()
    status = int(open(f"{work}/{name}.status").read().strip())
    suites[name] = {
        "status": status,
        "pass": sum(1 for l in log if re.match(r"^(PASS|ok) ", l)),
        "fail": sum(1 for l in log if l.startswith("FAIL ")),
        "skip": sum(1 for l in log if l.startswith("SKIP ")),
        "failed": [l[:300] for l in log if l.startswith("FAIL ")][:20],
        "last": log[-1][:300] if log else "",
    }
json.dump({"npm": version, "node": node, "os": os_, "fs": fs, "suites": suites}, open(f"{work}/suites.json", "w"), indent=2)
PY
[ "$(cat "$work/test-inject.status")" = 0 ] && [ "$(cat "$work/verify-npm.status")" = 0 ]
