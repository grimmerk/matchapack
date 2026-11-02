#!/usr/bin/env bash
# Routing tests for the shell wrapper + `lockclone npm`.
#   test-wrapper.sh <work-dir>
# Each case runs a command in `zsh -c` after sourcing shell/lockclone.sh, the
# way a user's shell would, and asserts WHO installed (lockclone or plain npm),
# the exit code, and a property of the result.
set -uo pipefail
work=$1
here=$(cd "$(dirname "$0")/.." && pwd)
bin_dir=$here/target/release
fx=$here/fixtures/small
pass=0; fail=0

new_project() { # name [package.json patch (python expression on d)]
  local d=$work/$1; rm -rf "${d:?}"; mkdir -p "$d"
  cp "$fx/package.json" "$fx/package-lock.json" "$d/"
  if [ -n "${2-}" ]; then python3 -c "import json;p='$d/package.json';d=json.load(open(p));$2;json.dump(d,open(p,'w'),indent=2)"; fi
  echo "$d"
}
run() { # dir env-assignments command...
  local d=$1; shift
  (cd "$d" && env PATH="$bin_dir:$PATH" "$@" zsh -c ". '$here/shell/lockclone.sh'; $CMD" > "$d.out" 2>&1); echo $?
}
check() { # name condition-description test-result
  if [ "$3" = ok ]; then pass=$((pass+1)); echo "PASS $1: $2"; else fail=$((fail+1)); echo "FAIL $1: $2"; sed 's/^/    /' "$4.out" | grep -E 'lockclone|npm error|ERR' | head -5; fi
}
by_lockclone() { grep -q 'lockclone: added' "$1.out"; }
by_npm() { ! grep -q 'lockclone: added' "$1.out" && [ -d "$1/node_modules" ]; }

# 1. bare npm ci → lockclone
d=$(new_project ci); CMD="npm ci"; rc=$(run "$d")
[ "$rc" = 0 ] && by_lockclone "$d" && [ -x "$d/node_modules/.bin/tsc" ] && r=ok || r=bad
check ci "bare npm ci installs from the store, bins linked" $r "$d"

# 2. alias npm clean-install → lockclone
d=$(new_project alias); CMD="npm clean-install --no-audit --no-fund"; rc=$(run "$d")
[ "$rc" = 0 ] && by_lockclone "$d" && r=ok || r=bad
check alias "npm clean-install with passive flags → lockclone" $r "$d"

# 3. a tree-shaping flag → plain npm, and npm's semantics apply (no dev deps)
d=$(new_project omit); CMD="npm ci --omit=dev"; rc=$(run "$d")
[ "$rc" = 0 ] && by_npm "$d" && [ ! -d "$d/node_modules/jest" ] && grep -q 'option --omit=dev is not handled' "$d.out" && r=ok || r=bad
check omit-flag "npm ci --omit=dev → plain npm with the reason printed, jest absent" $r "$d"

# 4. NODE_ENV=production → plain npm (npm omits dev)
d=$(new_project nodeenv); CMD="npm ci"; rc=$(run "$d" NODE_ENV=production)
[ "$rc" = 0 ] && by_npm "$d" && [ ! -d "$d/node_modules/jest" ] && r=ok || r=bad
check node-env "NODE_ENV=production → plain npm, dev deps omitted" $r "$d"

# 5. LOCKCLONE=0 → plain npm, silently
d=$(new_project optout); CMD="npm ci"; rc=$(run "$d" LOCKCLONE=0)
[ "$rc" = 0 ] && by_npm "$d" && ! grep -q '^lockclone' "$d.out" && r=ok || r=bad
check opt-out "LOCKCLONE=0 npm ci → plain npm, no lockclone output" $r "$d"

# 6. command npm ci → plain npm
d=$(new_project command); CMD="command npm ci"; rc=$(run "$d")
[ "$rc" = 0 ] && by_npm "$d" && ! grep -q '^lockclone' "$d.out" && r=ok || r=bad
check command "command npm ci bypasses the function" $r "$d"

# 7. other npm commands never reach lockclone; exit codes pass through
d=$(new_project other); CMD="npm run nope"; rc=$(run "$d")
[ "$rc" != 0 ] && ! grep -q '^lockclone' "$d.out" && grep -q 'Missing script' "$d.out" && r=ok || r=bad
check passthrough "npm run <missing> → npm's own error and non-zero exit" $r "$d"

# 8. npm version not verified → plain npm
d=$(new_project version); CMD="npm ci"; rc=$(run "$d" LOCKCLONE_NPM_VERSIONS=10.9)
[ "$rc" = 0 ] && by_npm "$d" && grep -q 'has not been verified' "$d.out" && r=ok || r=bad
check version-gate "unverified npm version → plain npm with the reason" $r "$d"

# 9. lockfile out of sync → plain npm, which refuses; old node_modules kept
d=$(new_project unsync "d['dependencies']['lodash']='^3.0.0'"); mkdir -p "$d/node_modules/keepme"; CMD="npm ci"; rc=$(run "$d")
[ "$rc" != 0 ] && grep -q 'running plain npm instead' "$d.out" && grep -q 'in sync' "$d.out" && [ -d "$d/node_modules/keepme" ] && r=ok || r=bad
check unsync "out-of-sync lockfile → npm's own EUSAGE, node_modules untouched" $r "$d"

# 10. a failing root lifecycle script → reported, not retried with npm
d=$(new_project scriptfail "d['scripts']['postinstall']='node -e \"process.exit(3)\"'"); CMD="npm ci"; rc=$(run "$d")
n_ci=$(grep -c 'npm ci from the store' "$d.out")
[ "$rc" != 0 ] && [ "$n_ci" = 1 ] && grep -q 'root lifecycle script failed' "$d.out" && ! grep -q 'running plain npm' "$d.out" && r=ok || r=bad
check script-fail "failing postinstall → non-zero exit, no npm retry" $r "$d"

# 11. a `file:` tarball dependency (unsupported) → plain npm
d=$work/filedep; rm -rf "$d"; mkdir -p "$d"; cp "$here/fixtures/optional-upward/tarballs/lc-q-1.0.0.tgz" "$d/"
echo '{"name":"filedep","version":"1.0.0","dependencies":{"lc-q":"file:lc-q-1.0.0.tgz"}}' > "$d/package.json"
(cd "$d" && command npm install --package-lock-only --ignore-scripts >/dev/null 2>&1)
CMD="npm ci"; rc=$(run "$d")
[ "$rc" = 0 ] && by_npm "$d" && [ -f "$d/node_modules/lc-q/index.js" ] && grep -q 'unsupported lockfile entries' "$d.out" && r=ok || r=bad
check file-dep "file: tarball dependency → plain npm installs it" $r "$d"

# 12. --ignore-scripts → lockclone; bins linked, no root postinstall output
d=$(new_project ignorescripts); CMD="npm ci --ignore-scripts"; rc=$(run "$d")
[ "$rc" = 0 ] && by_lockclone "$d" && [ -x "$d/node_modules/.bin/tsc" ] && [ ! -f "$d/root-postinstall.txt" ] && r=ok || r=bad
check ignore-scripts "npm ci --ignore-scripts → lockclone, bins linked, root postinstall not run" $r "$d"

# 13. the root postinstall runs on a normal install (control for 12)
[ -f "$work/ci/root-postinstall.txt" ] && r=ok || r=bad
check root-script "bare npm ci ran the root postinstall (control for case 12)" $r "$work/ci"

echo "== $pass passed, $fail failed"
[ "$fail" = 0 ]
