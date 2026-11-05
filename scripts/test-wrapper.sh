#!/usr/bin/env bash
# Routing tests for the shell wrapper + `thinclone npm`.
#   test-wrapper.sh <work-dir>
# Each case runs a command in `zsh -c` after sourcing shell/thinclone.sh, the
# way a user's shell would, and asserts WHO installed (thinclone or plain npm),
# the exit code, and a property of the result.
set -uo pipefail
work=$1
here=$(cd "$(dirname "$0")/.." && pwd)
bin_dir=$here/target/release
# Test runs log to the work dir, never to the real trial log.
export THINCLONE_LOG_DIR=$work/logs
mkdir -p "$work"
# Every case runs inside the trial scope unless it sets its own (cases 14-15):
# the user's ~/.config/thinclone/paths must not decide the outcome.
export THINCLONE_PATHS=$(cd "$work" && pwd -P)
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
  (cd "$d" && env PATH="$bin_dir:$PATH" "$@" zsh -c ". '$here/shell/thinclone.sh'; $CMD" > "$d.out" 2>&1); echo $?
}
check() { # name condition-description test-result dir
  if [ "$3" = ok ]; then pass=$((pass+1)); echo "PASS $1: $2"; else fail=$((fail+1)); echo "FAIL $1: $2 (rc=${rc-?})"; sed 's/^/    /' "$4.out" 2>/dev/null | grep -E 'thinclone|npm error|ERR' | head -5; fi
}
by_thinclone() { grep -q 'thinclone: added' "$1.out"; }
by_npm() { ! grep -q 'thinclone: added' "$1.out" && [ -d "$1/node_modules" ]; }

# 1. bare npm ci → thinclone
d=$(new_project ci); CMD="npm ci"; rc=$(run "$d")
[ "$rc" = 0 ] && by_thinclone "$d" && [ -x "$d/node_modules/.bin/tsc" ] && r=ok || r=bad
check ci "bare npm ci installs from the store, bins linked" $r "$d"

# 2. alias npm clean-install → thinclone
d=$(new_project alias); CMD="npm clean-install --no-audit --no-fund"; rc=$(run "$d")
[ "$rc" = 0 ] && by_thinclone "$d" && r=ok || r=bad
check alias "npm clean-install with passive flags → thinclone" $r "$d"

# 3. a tree-shaping flag → plain npm, and npm's semantics apply (no dev deps)
d=$(new_project omit); CMD="npm ci --omit=dev"; rc=$(run "$d")
[ "$rc" = 0 ] && by_npm "$d" && [ ! -d "$d/node_modules/jest" ] && grep -q 'option --omit=dev is not handled' "$d.out" && r=ok || r=bad
check omit-flag "npm ci --omit=dev → plain npm with the reason printed, jest absent" $r "$d"

# 4. NODE_ENV=production → plain npm (npm omits dev)
d=$(new_project nodeenv); CMD="npm ci"; rc=$(run "$d" NODE_ENV=production)
[ "$rc" = 0 ] && by_npm "$d" && [ ! -d "$d/node_modules/jest" ] && r=ok || r=bad
check node-env "NODE_ENV=production → plain npm, dev deps omitted" $r "$d"

# 5. THINCLONE=0 → plain npm, silently
d=$(new_project optout); CMD="npm ci"; rc=$(run "$d" THINCLONE=0)
[ "$rc" = 0 ] && by_npm "$d" && ! grep -q '^thinclone' "$d.out" && r=ok || r=bad
check opt-out "THINCLONE=0 npm ci → plain npm, no thinclone output" $r "$d"

# 6. command npm ci → plain npm
d=$(new_project command); CMD="command npm ci"; rc=$(run "$d")
[ "$rc" = 0 ] && by_npm "$d" && ! grep -q '^thinclone' "$d.out" && r=ok || r=bad
check command "command npm ci bypasses the function" $r "$d"

# 7. other npm commands never reach thinclone; exit codes pass through
d=$(new_project other); CMD="npm run nope"; rc=$(run "$d")
[ "$rc" != 0 ] && ! grep -q '^thinclone' "$d.out" && grep -q 'Missing script' "$d.out" && r=ok || r=bad
check passthrough "npm run <missing> → npm's own error and non-zero exit" $r "$d"

# 8. npm version not verified → plain npm
d=$(new_project version); CMD="npm ci"; rc=$(run "$d" THINCLONE_NPM_VERSIONS=0.1)
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

# 12. --ignore-scripts → thinclone; bins linked, no root postinstall output
d=$(new_project ignorescripts); CMD="npm ci --ignore-scripts"; rc=$(run "$d")
[ "$rc" = 0 ] && by_thinclone "$d" && [ -x "$d/node_modules/.bin/tsc" ] && [ ! -f "$d/root-postinstall.txt" ] && r=ok || r=bad
check ignore-scripts "npm ci --ignore-scripts → thinclone, bins linked, root postinstall not run" $r "$d"

# 13. the root postinstall runs on a normal install (control for 12)
[ -f "$work/ci/root-postinstall.txt" ] && r=ok || r=bad
check root-script "bare npm ci ran the root postinstall (control for case 12)" $r "$work/ci"

# 14. trial scope: a project outside THINCLONE_PATHS → plain npm with the reason
d=$(new_project outscope); CMD="npm ci"; rc=$(run "$d" THINCLONE_PATHS=/nonexistent-prefix)
[ "$rc" = 0 ] && by_npm "$d" && grep -q 'outside the trial scope' "$d.out" && r=ok || r=bad
check scope-out "project outside the trial scope → plain npm with the reason" $r "$d"

# 15. inside the scope → thinclone
d=$(new_project inscope); CMD="npm ci"; rc=$(run "$d" THINCLONE_PATHS="/nonexistent-prefix:$work")
[ "$rc" = 0 ] && by_thinclone "$d" && r=ok || r=bad
check scope-in "project inside the trial scope → thinclone" $r "$d"

# 16. the run log records installs and declines (in the test's own log dir)
n_inst=$(grep -c '"outcome":"installed"' "$THINCLONE_LOG_DIR/log.jsonl" 2>/dev/null || echo 0)
n_decl=$(grep -c '"outcome":"declined"' "$THINCLONE_LOG_DIR/log.jsonl" 2>/dev/null || echo 0)
n_nobuild=$(grep -vc '"build":"' "$THINCLONE_LOG_DIR/log.jsonl" 2>/dev/null)
[ "$n_inst" -ge 4 ] && [ "$n_decl" -ge 5 ] && [ "${n_nobuild:-1}" = 0 ] && r=ok || r=bad
check log "log.jsonl records installs ($n_inst) and declines ($n_decl), each with the build (missing: ${n_nobuild:-?})" $r "$work/logs"

# 17–19. settings npm applies and thinclone does not → plain npm with the reason
d=$(new_project strict); CMD="npm ci"; rc=$(run "$d" npm_config_strict_allow_scripts=true)
grep -q 'strict-allow-scripts' "$d.out" && ! by_thinclone "$d" && r=ok || r=bad
check strict-scripts "strict-allow-scripts=true → plain npm (npm refuses before wiping)" $r "$d"
d=$(new_project umask); CMD="umask 077; npm ci"; rc=$(run "$d")
grep -q 'umask is 077' "$d.out" && by_npm "$d" && r=ok || r=bad
check umask "umask 077 → plain npm" $r "$d"
d=$(new_project allowremote); CMD="npm ci"; rc=$(run "$d" npm_config_allow_remote=none)
grep -q 'allow-remote' "$d.out" && ! by_thinclone "$d" && r=ok || r=bad
check allow-remote "allow-remote=none → plain npm" $r "$d"

# 20–21. engine-strict: matching engines → thinclone; a mismatch → npm (EBADENGINE)
d=$(new_project esok); echo 'engine-strict=true' > "$d/.npmrc"; CMD="npm ci"; rc=$(run "$d")
[ "$rc" = 0 ] && by_thinclone "$d" && r=ok || r=bad
check engine-ok "engine-strict with matching engines → thinclone" $r "$d"
d=$(new_project esbad); echo 'engine-strict=true' > "$d/.npmrc"
python3 -c "import json;p='$d/package-lock.json';l=json.load(open(p));l['packages']['node_modules/lodash']['engines']={'node':'>=99'};json.dump(l,open(p,'w'),indent=2)"
mkdir -p "$d/node_modules/keepme"; CMD="npm ci"; rc=$(run "$d")
[ "$rc" != 0 ] && grep -q 'EBADENGINE' "$d.out" && [ -d "$d/node_modules/keepme" ] && r=ok || r=bad
check engine-bad "engine-strict with an engines mismatch → npm's EBADENGINE, node_modules untouched" $r "$d"

# 22. --legacy-peer-deps → thinclone follows npm's ideal tree for it: npm 11
# leaves out react (only a peer of react-dom), npm 10 still installs it.
d=$work/legacypeer; rm -rf "${d:?}"; mkdir -p "$d"
cp "$here/fixtures/legacy-peer/package.json" "$here/fixtures/legacy-peer/package-lock.json" "$d/"
CMD="npm ci --legacy-peer-deps"; rc=$(run "$d")
if [ "$(npm -v | cut -d. -f1)" -ge 11 ]; then want_react=no; else want_react=yes; fi
[ -d "$d/node_modules/react" ] && has_react=yes || has_react=no
[ "$rc" = 0 ] && by_thinclone "$d" && [ "$has_react" = "$want_react" ] && r=ok || r=bad
check legacy-peer "npm ci --legacy-peer-deps → thinclone, react installed: $has_react (npm $(npm -v) installs it: $want_react)" $r "$d"

# 25–26. #6: npm settings are an allow-list. A setting npm defines that is
# not on the list hands over to npm, naming it; settings npm does not define
# (another package manager's) and listed ones (min-release-age) do not.
d=$(new_project cfgdeny); printf 'prefer-dedupe=true\n' > "$d/.npmrc"; CMD="npm ci"; rc=$(run "$d")
[ "$rc" = 0 ] && by_npm "$d" && grep -q 'not on thinclone.s list.*prefer-dedupe (from project)' "$d.out" && r=ok || r=bad
check config-allowlist "a setting not on the allow-list (prefer-dedupe) → plain npm, named" $r "$d"
d=$(new_project cfgok); printf 'resolution-mode=highest\nmin-release-age=7\n' > "$d/.npmrc"; CMD="npm ci"; rc=$(run "$d")
[ "$rc" = 0 ] && by_thinclone "$d" && r=ok || r=bad
check config-allowed "a pnpm-only setting and min-release-age → thinclone" $r "$d"
# 27. `call` (an `npm exec` option) is ignored by npm ci. thinclone used to
# run the root scripts through `npm exec`, which refuses it with a command, so
# the install failed after changing node_modules. The post-install step no
# longer uses `npm exec`: thinclone installs, root postinstall included.
d=$(new_project cfgcall); printf 'call=echo hello\n' > "$d/.npmrc"; CMD="npm ci"; rc=$(run "$d")
[ "$rc" = 0 ] && by_thinclone "$d" && [ -f "$d/root-postinstall.txt" ] && r=ok || r=bad
check config-call "call=… in .npmrc → thinclone, root postinstall ran" $r "$d"

# 28. An optional dependency whose (approved) install script fails: npm ci
# drops it and succeeds. thinclone's post-install step reports the failure,
# and the wrapper hands over to npm ci, which rebuilds node_modules the same
# way (the outcome is logged as "broke"). Needs the fixtures' tarball server.
if ! curl -sf -o /dev/null http://127.0.0.1:48321/lc-fail-1.0.0.tgz; then
  python3 -m http.server 48321 --bind 127.0.0.1 --directory "$here/fixtures/optional-upward/tarballs" >/dev/null 2>&1 &
  started_server=$!
  until curl -sf -o /dev/null http://127.0.0.1:48321/lc-fail-1.0.0.tgz; do sleep 0.3; done
fi
d=$work/optfail; rm -rf "${d:?}"; mkdir -p "$d"
cp "$here/fixtures/optional-script-fail/package.json" "$here/fixtures/optional-script-fail/package-lock.json" "$d/"
CMD="npm ci"; rc=$(run "$d")
[ -n "${started_server-}" ] && kill "$started_server"
[ "$rc" = 0 ] && [ "$(ls "$d/node_modules" | tr '\n' ' ')" = "lc-r lc-s " ] && grep -q 'running npm ci to rebuild it' "$d.out" && r=ok || r=bad
check optional-script-fail "a failing optional dependency script → npm ci takes over, the package is dropped as npm ci does" $r "$d"

# 29. A hand-over happens before thinclone changes anything (#27). With
# THINCLONE_NO_FALLBACK=1 thinclone exits 3 instead of running npm, so the
# project shows exactly what thinclone did: an existing node_modules (a
# sentinel, a 2020 mtime) must be untouched, mtimes included.
d=$(new_project untouched); printf 'prefer-dedupe=true\n' > "$d/.npmrc"
mkdir -p "$d/node_modules/sentinel"; echo keep > "$d/node_modules/sentinel/x"
touch -t 202001010000 "$d/node_modules" "$d/node_modules/sentinel" "$d/node_modules/sentinel/x"
before=$(cd "$d" && find node_modules -print0 | xargs -0 stat -f '%N %m' 2>/dev/null | sort)
CMD="npm ci"; rc=$(run "$d" THINCLONE_NO_FALLBACK=1)
after=$(cd "$d" && find node_modules -print0 | xargs -0 stat -f '%N %m' 2>/dev/null | sort)
[ "$rc" = 3 ] && grep -q 'not handled, left unchanged' "$d.out" && [ "$before" = "$after" ] && r=ok || r=bad
check untouched "a hand-over leaves node_modules exactly as it was (exit 3 with THINCLONE_NO_FALLBACK=1)" $r "$d"

# 23–24. The rc one-liner from the README, `eval "$(thinclone init)"`, detects
# the shell and loads the same function from the binary (no checkout needed).
for sh in zsh bash; do
  d=$(new_project "init-$sh")
  # Loading prints nothing of its own: the first line of output is `type npm`
  # saying it is a function (bash: "a function", zsh: "a shell function"), so
  # any diagnostic from init or the eval would come first and fail this.
  load=$(env PATH="$bin_dir:$PATH" "$sh" -c 'eval "$(thinclone init)" && type npm' 2>&1 | head -1)
  (cd "$d" && env PATH="$bin_dir:$PATH" "$sh" -c 'eval "$(thinclone init)"; npm ci --no-audit --no-fund' > "$d.out" 2>&1); rc=$?
  case $load in "npm is a function" | "npm is a shell function"*) loaded=yes ;; *) loaded=no ;; esac
  [ "$rc" = 0 ] && [ "$loaded" = yes ] && by_thinclone "$d" && r=ok || r=bad
  check "init-$sh" "eval \"\$(thinclone init)\" in $sh (detected), then npm ci → thinclone" $r "$d"
done

echo "== $pass passed, $fail failed"
[ "$fail" = 0 ]
