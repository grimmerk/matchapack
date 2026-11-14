#!/usr/bin/env bash
# Routing tests for the shell wrapper + `matchapack npm`.
#   test-wrapper.sh <work-dir>
# Each case runs a command in `zsh -c` after sourcing shell/matchapack.sh, the
# way a user's shell would, and asserts WHO installed (matchapack or plain npm),
# the exit code, and a property of the result. The inject engine's own cases
# (flags, gates, npm's failures, workspaces) are in scripts/test-inject.sh;
# these are the routing cases that do not depend on how the install is made.
set -uo pipefail
work=$1
here=$(cd "$(dirname "$0")/.." && pwd)
bin_dir=$here/target/release
mkdir -p "$work"; work=$(cd "$work" && pwd -P)
# Projects in a directory of the user's own group (macOS /tmp is wheel, a
# group the user is not in: some packages would then be npm's own extraction,
# #41; nothing here counts them, but the work dir stays like test-inject's).
chgrp "$(id -g)" "$work"
# Test runs log to the work dir and use a store of their own, never the real
# ones.
export MATCHAPACK_LOG_DIR=$work/logs MATCHAPACK_STORE=$work/store
rm -rf "${work:?}/logs"
# Every case runs inside the trial scope unless it sets its own (cases 14-15):
# the user's ~/.config/matchapack/paths must not decide the outcome.
export MATCHAPACK_PATHS=$work
# Any npm 10-12 is taken by the inject engine's gate; a run on another npm
# would pass the routing cases through plain npm and fail them, not skip.
unset MATCHAPACK_ENGINE MATCHAPACK_NPM_VERSIONS MATCHAPACK_INJECT_HOOK MATCHAPACK
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
  (cd "$d" && env PATH="$bin_dir:$PATH" "$@" zsh -c ". '$here/shell/matchapack.sh'; $CMD" > "$d.out" 2>&1); echo $?
}
check() { # name condition-description test-result dir
  if [ "$3" = ok ]; then pass=$((pass+1)); echo "PASS $1: $2"; else fail=$((fail+1)); echo "FAIL $1: $2 (rc=${rc-?})"; sed 's/^/    /' "$4.out" 2>/dev/null | grep -E 'matchapack|npm error|ERR' | head -5; fi
}
by_matchapack() { grep -q 'matchapack: added' "$1.out"; }
by_npm() { ! grep -q 'matchapack: added' "$1.out" && [ -d "$1/node_modules" ]; }

# 1. bare npm ci → matchapack
d=$(new_project ci); CMD="npm ci"; rc=$(run "$d")
[ "$rc" = 0 ] && by_matchapack "$d" && [ -x "$d/node_modules/.bin/tsc" ] && r=ok || r=bad
check ci "bare npm ci installs from the store, bins linked" $r "$d"

# 2. alias npm clean-install → matchapack
d=$(new_project alias); CMD="npm clean-install --no-audit --no-fund"; rc=$(run "$d")
[ "$rc" = 0 ] && by_matchapack "$d" && [ -x "$d/node_modules/.bin/tsc" ] && r=ok || r=bad
check alias "npm clean-install --no-audit --no-fund → matchapack" $r "$d"

# 5. MATCHAPACK=0 → plain npm, silently
d=$(new_project optout); CMD="npm ci"; rc=$(run "$d" MATCHAPACK=0)
[ "$rc" = 0 ] && by_npm "$d" && ! grep -q '^matchapack' "$d.out" && r=ok || r=bad
check opt-out "MATCHAPACK=0 npm ci → plain npm, no matchapack output" $r "$d"

# 6. command npm ci → plain npm
d=$(new_project command); CMD="command npm ci"; rc=$(run "$d")
[ "$rc" = 0 ] && by_npm "$d" && ! grep -q '^matchapack' "$d.out" && r=ok || r=bad
check command "command npm ci bypasses the function" $r "$d"

# 7. other npm commands never reach matchapack; exit codes pass through
d=$(new_project other); CMD="npm run nope"; rc=$(run "$d")
[ "$rc" != 0 ] && ! grep -q '^matchapack' "$d.out" && grep -q 'Missing script' "$d.out" && r=ok || r=bad
check passthrough "npm run <missing> → npm's own error and non-zero exit" $r "$d"

# 10. a failing root lifecycle script → npm's own failure, said once, not
#     retried with plain npm
d=$(new_project scriptfail "d['scripts']['postinstall']='node -e \"process.exit(3)\"'"); CMD="npm ci"; rc=$(run "$d")
n_ci=$(grep -c 'npm ci with clones from the store' "$d.out")
[ "$rc" != 0 ] && [ "$n_ci" = 1 ] && [ "$(grep -c 'npm error code' "$d.out")" = 1 ] && ! grep -q 'running plain npm\|running npm ci to rebuild' "$d.out" && r=ok || r=bad
check script-fail "failing postinstall → npm's non-zero exit, npm ci run once" $r "$d"

# 12. --ignore-scripts → matchapack; bins linked, no root postinstall
d=$(new_project ignorescripts); CMD="npm ci --ignore-scripts"; rc=$(run "$d")
[ "$rc" = 0 ] && by_matchapack "$d" && [ -x "$d/node_modules/.bin/tsc" ] && [ ! -f "$d/root-postinstall.txt" ] && r=ok || r=bad
check ignore-scripts "npm ci --ignore-scripts → matchapack, bins linked, root postinstall not run" $r "$d"

# 13. the root postinstall runs on a normal install (control for 12)
[ -f "$work/ci/root-postinstall.txt" ] && r=ok || r=bad
check root-script "bare npm ci ran the root postinstall (control for case 12)" $r "$work/ci"

# 14. trial scope: a project outside MATCHAPACK_PATHS → plain npm with the reason
d=$(new_project outscope); CMD="npm ci"; rc=$(run "$d" MATCHAPACK_PATHS=/nonexistent-prefix)
[ "$rc" = 0 ] && by_npm "$d" && grep -q 'outside the trial scope' "$d.out" && [ -x "$d/node_modules/.bin/tsc" ] && r=ok || r=bad
check scope-out "project outside the trial scope → plain npm with the reason" $r "$d"

# 15. inside the scope → matchapack
d=$(new_project inscope); CMD="npm ci"; rc=$(run "$d" MATCHAPACK_PATHS="/nonexistent-prefix:$work")
[ "$rc" = 0 ] && by_matchapack "$d" && r=ok || r=bad
check scope-in "project inside the trial scope → matchapack" $r "$d"

# 29. A hand-over happens before matchapack changes anything (#27). With
# MATCHAPACK_NO_FALLBACK=1 matchapack exits 3 instead of running npm, so the
# project shows exactly what matchapack did: an existing node_modules (a
# sentinel, a 2020 mtime) must be untouched, mtimes included. Two hand-overs
# decided at different points: the trial scope (before the engine's gates)
# and the npm version gate (inject::gates, the last check before npm starts).
for why in scope version; do
  d=$(new_project "untouched-$why")
  mkdir -p "$d/node_modules/sentinel"; echo keep > "$d/node_modules/sentinel/x"
  touch -t 202001010000 "$d/node_modules" "$d/node_modules/sentinel" "$d/node_modules/sentinel/x"
  before=$(cd "$d" && find node_modules -print0 | xargs -0 stat -f '%N %m' 2>/dev/null | sort)
  if [ $why = scope ]; then envs=(MATCHAPACK_PATHS=/nonexistent-prefix); else envs=(MATCHAPACK_NPM_VERSIONS=0.1); fi
  CMD="npm ci"; rc=$(run "$d" MATCHAPACK_NO_FALLBACK=1 "${envs[@]}")
  after=$(cd "$d" && find node_modules -print0 | xargs -0 stat -f '%N %m' 2>/dev/null | sort)
  [ "$rc" = 3 ] && grep -q 'not handled, left unchanged' "$d.out" && [ -n "$before" ] && [ "$before" = "$after" ] && r=ok || r=bad
  check "untouched-$why" "a hand-over ($why) leaves node_modules exactly as it was (exit 3 with MATCHAPACK_NO_FALLBACK=1)" $r "$d"
done

# 30. A root script killed by a signal: npm ci fails, and matchapack passes
# npm's status on; never retried with npm ci, so the script runs exactly once.
d=$(new_project rootkill "d['scripts']['postinstall']='echo ran >> runs.txt; kill -TERM \$\$'"); CMD="npm ci"; rc=$(run "$d")
runs=$(wc -l < "$d/runs.txt" 2>/dev/null | tr -d ' ')
[ "$rc" != 0 ] && [ "$runs" = 1 ] && ! grep -q 'running npm ci to rebuild\|running plain npm' "$d.out" && r=ok || r=bad
check root-killed "a root script killed by a signal → npm's failure, not retried (ran $runs time(s))" $r "$d"

# 31. The install family is routed; other commands, a flag first, and the
#     two bypasses never start matchapack (it would log a line).
logn() { { wc -l < "$MATCHAPACK_LOG_DIR/log.jsonl"; } 2>/dev/null || echo 0; }
lastrec() { tail -1 "$MATCHAPACK_LOG_DIR/log.jsonl" | python3 -c 'import json,sys; r=json.load(sys.stdin); print(eval(sys.argv[1]))' "$1"; }
d=$work/ci
for w in "i --no-audit --no-fund" "update --no-audit --no-fund" "rm left-pad --no-audit --no-fund"; do
  n0=$(logn); CMD="npm $w"; rc=$(run "$d")
  want=$(case ${w%% *} in i) echo install;; update) echo update;; rm) echo uninstall;; esac)
  [ "$rc" = 0 ] && [ "$(logn)" = $((n0+1)) ] && [ "$(lastrec 'r["outcome"], r["command"], r["args"][0]')" = "('installed', '$want', '${w%% *}')" ] && r=ok || r=bad
  check "routed-${w%% *}" "npm $w → matchapack npm, npm's $want" $r "$d"
done
for c in "run nope" "test" "--version" "exec -- node -e 0" "link --dry-run" "dedupe --dry-run" "--prefix $d install --no-audit --no-fund"; do
  n0=$(logn); CMD="npm $c"; rc=$(run "$d")
  [ "$(logn)" = "$n0" ] && ! grep -q '^matchapack' "$d.out" && r=ok || r=bad
  check "not-routed" "npm $c → npm itself, matchapack not started" $r "$d"
done
n0=$(logn); CMD="npm install --no-audit --no-fund"; rc=$(run "$d" MATCHAPACK=0)
[ "$rc" = 0 ] && [ "$(logn)" = "$n0" ] && r=ok || r=bad
check opt-out-install "MATCHAPACK=0 npm install → plain npm, not logged" $r "$d"
n0=$(logn); CMD="command npm install --no-audit --no-fund"; rc=$(run "$d")
[ "$rc" = 0 ] && [ "$(logn)" = "$n0" ] && r=ok || r=bad
check command-install "command npm install bypasses the function" $r "$d"
# `npm u`: an alias of update from npm 11; on npm 10 no command at all. The
# shell function routes it either way; on npm 10 matchapack declines on
# npm's own answer and npm prints its own error, once.
n0=$(logn); CMD="npm u --no-audit --no-fund"; rc=$(run "$d")
if [ "$(npm -v | cut -d. -f1)" = 10 ]; then
  [ "$rc" != 0 ] && [ "$(lastrec 'r["outcome"]')" = declined ] && grep -q 'running plain npm instead (npm has no command `u`)' "$d.out" \
    && [ "$(grep -Ec 'Unknown command: "u"' "$d.out")" = 1 ] && ! grep -q 'Exit handler never called' "$d.out" && r=ok || r=bad
  check npm-u "npm u on npm 10: declined on npm's answer, npm's own unknown-command error once" $r "$d"
else
  [ "$rc" = 0 ] && [ "$(lastrec 'r["outcome"], r["command"]')" = "('installed', 'update')" ] && r=ok || r=bad
  check npm-u "npm u on npm $(npm -v): routed, npm's update" $r "$d"
fi

# 16. the run log records installs and declines (in the test's own log dir)
n_inst=$(grep -c '"outcome":"installed"' "$MATCHAPACK_LOG_DIR/log.jsonl" 2>/dev/null || echo 0)
n_decl=$(grep -c '"outcome":"declined"' "$MATCHAPACK_LOG_DIR/log.jsonl" 2>/dev/null || echo 0)
n_npmfail=$(grep -c '"outcome":"npm_failed"' "$MATCHAPACK_LOG_DIR/log.jsonl" 2>/dev/null || echo 0)
n_nobuild=$(grep -vc '"build":"' "$MATCHAPACK_LOG_DIR/log.jsonl" 2>/dev/null)
n_noengine=$(grep -vc '"engine":"inject"' "$MATCHAPACK_LOG_DIR/log.jsonl" 2>/dev/null)
# The load average at the start and the end of every run: [1 min, 5 min].
n_noload=$(( $(grep -vc '"load_start":\[[0-9.]*,[0-9.]*\]' "$MATCHAPACK_LOG_DIR/log.jsonl" 2>/dev/null) + $(grep -vc '"load_end":\[[0-9.]*,[0-9.]*\]' "$MATCHAPACK_LOG_DIR/log.jsonl" 2>/dev/null) ))
[ "$n_inst" -ge 4 ] && [ "$n_decl" -ge 3 ] && [ "$n_npmfail" -ge 2 ] && [ "${n_nobuild:-1}" = 0 ] && [ "${n_noengine:-1}" = 0 ] && [ "$n_noload" = 0 ] && r=ok || r=bad
check log "log.jsonl records installs ($n_inst), declines ($n_decl) and npm's failures ($n_npmfail), each with the build (missing: ${n_nobuild:-?}), the engine (missing: ${n_noengine:-?}) and the load average (missing: $n_noload)" $r "$work/logs"

# 23–24. The rc one-liner from the README, `eval "$(matchapack init)"`, detects
# the shell and loads the same function from the binary (no checkout needed).
for sh in zsh bash; do
  d=$(new_project "init-$sh")
  # Loading prints nothing of its own: the first line of output is `type npm`
  # saying it is a function (bash: "a function", zsh: "a shell function"), so
  # any diagnostic from init or the eval would come first and fail this.
  load=$(env PATH="$bin_dir:$PATH" "$sh" -c 'eval "$(matchapack init)" && type npm' 2>&1 | head -1)
  (cd "$d" && env PATH="$bin_dir:$PATH" "$sh" -c 'eval "$(matchapack init)"; npm ci --no-audit --no-fund' > "$d.out" 2>&1); rc=$?
  case $load in "npm is a function" | "npm is a shell function"*) loaded=yes ;; *) loaded=no ;; esac
  [ "$rc" = 0 ] && [ "$loaded" = yes ] && by_matchapack "$d" && r=ok || r=bad
  check "init-$sh" "eval \"\$(matchapack init)\" in $sh (detected), then npm ci → matchapack" $r "$d"
done

echo "== $pass passed, $fail failed"
[ "$fail" = 0 ]
