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
logn() { { wc -l < "$MATCHAPACK_LOG_DIR/log.jsonl" | tr -d ' '; } 2>/dev/null || echo 0; }
lastrec() { tail -1 "$MATCHAPACK_LOG_DIR/log.jsonl" | python3 -c 'import json,sys; r=json.load(sys.stdin); print(eval(sys.argv[1]))' "$1"; }
d=$work/ci
for w in "i --no-audit --no-fund" "update --no-audit --no-fund" "rm left-pad --no-audit --no-fund"; do
  n0=$(logn); CMD="npm $w"; rc=$(run "$d")
  want=install; [ "${w%% *}" = update ] && want=update; [ "${w%% *}" = rm ] && want=uninstall
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

# 32. Stopped by a signal (#64; src/signals.rs). npm stays in matchapack's
#     process group; its hook stops it when matchapack goes away or says a
#     signal stopped it. Each case is compared with plain npm given the same
#     signal at the same point: the exit status, what is left in
#     node_modules, and nothing left running or written once npm is done.
#     Without a terminal (a new session, as under CI or a supervisor).
descendants() { # every descendant pid of $1
  ps -A -o pid=,ppid= | awk -v root="$1" '{ parent[$1]=$2 } END { for (p in parent) { q=p; while (q in parent && q != root && q > 1) q=parent[q]; if (q == root && p != root) print p } }'
}
nosession=(python3 -c 'import os,sys; os.setsid(); os.execvp(sys.argv[1], sys.argv[1:])')
# stopped <case> <plain|mp> <signal> <pid|group> <extract|reify|script|count>
# [VAR=value...]: npm ci, plain or through matchapack, signalled once npm
# has extracted lodash (extract), once its reify has linked bins (reify),
# once the root postinstall runs (script: a
# loop writing node_modules/.tick; count: a script that writes down each
# signal it gets and exits 0 1.5 s after the first; both run only while
# the file `loop` exists, removed at the end), to the started
# process (pid) or its whole group (group). Sets rc, alive (processes left
# once npm is done), newer (paths written after that), nm (node_modules'
# entries, or none), sigs (what the count script got).
stopped() {
  local c=$1 who=$2 sig=$3 how=$4 at=$5; shift 5
  d=$(new_project "$c")
  case $at in
    script) printf '%s\n' "const fs = require('fs')" "fs.writeFileSync('started', '')" \
              "setInterval(() => fs.existsSync('loop') && fs.writeFileSync('node_modules/.tick', String(Date.now())), 50)" \
              "setInterval(() => fs.existsSync('loop') || process.exit(0), 50)" > "$d/loop.js" ;;
    count) printf '%s\n' "const fs = require('fs')" "fs.writeFileSync('started', '')" "let first = true" \
             "for (const s of ['SIGTERM', 'SIGINT', 'SIGHUP']) process.on(s, () => { fs.appendFileSync('sigs', s + '\\n'); if (first) { first = false; setTimeout(() => process.exit(0), 1500) } })" \
             "setInterval(() => fs.existsSync('loop') || process.exit(0), 50)" > "$d/loop.js" ;;
  esac
  touch "$d/loop"
  [ "$at" = script ] || [ "$at" = count ] && python3 -c "import json;p='$d/package.json';d=json.load(open(p));d['scripts']['postinstall']='node loop.js';json.dump(d,open(p,'w'),indent=2)"
  local npm_cmd=(matchapack npm); [ "$who" = plain ] && npm_cmd=(env MATCHAPACK=0 "$(command -v npm)")
  (cd "$d" && exec "${nosession[@]}" env PATH="$bin_dir:$PATH" "$@" "${npm_cmd[@]}" ci --no-audit --no-fund) > "$d.out" 2>&1 < /dev/null &
  local m=$! mark=$d/started _ p
  [ "$at" = reify ] && mark=$d/node_modules/.bin/tsc
  [ "$at" = extract ] && mark=$d/node_modules/lodash/package.json
  for _ in $(seq 6000); do [ -e "$mark" ] && break; sleep 0.01; done
  local tree; tree=$(descendants "$m" | tr '\n' ' ')
  if [ "$how" = group ]; then kill -"$sig" -- -"$m"; else kill -"$sig" "$m"; fi
  ( sleep 90; kill -KILL "$m" 2>/dev/null ) >/dev/null 2>&1 & local guard=$!
  wait "$m"; rc=$?
  pkill -P "$guard" 2>/dev/null; kill "$guard" 2>/dev/null; wait "$guard" 2>/dev/null
  # npm done: its rollback may outlive a matchapack killed by SIGKILL.
  for _ in $(seq 300); do
    alive=""; for p in $tree; do kill -0 "$p" 2>/dev/null && alive="$alive $p"; done
    [ -z "$alive" ] && break; sleep 0.1
  done
  touch "$work/stamp-$c"; sleep 1
  newer=$(find "$d" -newer "$work/stamp-$c" | head -3 | tr '\n' ' ')
  alive=""; for p in $tree; do kill -0 "$p" 2>/dev/null && alive="$alive $p:$(ps -o comm= -p "$p" | tr -d ' ')"; done
  for p in $tree; do kill -KILL "$p" 2>/dev/null; done
  rm -f "$d/loop"
  if [ -d "$d/node_modules" ]; then nm=$(find "$d/node_modules" | wc -l | tr -d ' '); else nm=none; fi
  sigs=$({ tr '\n' ' ' < "$d/sigs"; } 2>/dev/null)
}
interrupted_as() { # <signal name> <npm's code>: the last log record
  [ "$(lastrec 'r["outcome"], r["signal"], r["code"], r["engine"]')" = "('interrupted', '$1', $2, 'inject')" ]
}
again() { # the next npm ci in $d: the store's lock is free
  CMD="npm ci --no-audit --no-fund"; [ "$(run "$d")" = 0 ] && by_matchapack "$d"
}
not_rerun() { ! grep -q 'running npm ci to rebuild\|running plain npm' "$d.out"; }
# To matchapack's pid: npm gets the signal from the hook. During the root
# script (npm passes SIGTERM on to it, and ends by it once the script has),
# and during npm's extraction (npm rolls back and exits 1). The hook's
# signal comes 50 ms after matchapack's (src/inject.js `stopSelf`): at the
# very end of reify that can be after npm's rollback handler has gone, as
# for plain npm signalled 50 ms later; so the extraction step here.
stopped sigterm-plain plain TERM pid script; p_rc=$rc p_nm=$nm
stopped sigterm mp TERM pid script
[ "$rc" = 143 ] && [ "$rc" = "$p_rc" ] && [ "$nm" = "$p_nm" ] && [ -z "$alive" ] && [ -z "$newer" ] && interrupted_as SIGTERM 143 \
  && grep -q 'stopped by SIGTERM; npm ended by SIGTERM' "$d.out" && not_rerun && again && r=ok || r=bad
check sigterm "SIGTERM to matchapack's pid during the root script: exit $rc (plain npm $p_rc), node_modules $nm entries (plain $p_nm), nothing left running (${alive:-none}) or written after (${newer:-nothing}), logged interrupted, npm not run again, the next npm ci succeeds" $r "$d"
for s in HUP INT; do
  stopped "sig$s-plain" plain "$s" pid extract; p_rc=$rc p_nm=$nm
  stopped "sig$s" mp "$s" pid extract
  [ "$rc" = 1 ] && [ "$rc" = "$p_rc" ] && [ "$nm" = "$p_nm" ] && [ -z "$alive" ] && [ -z "$newer" ] && interrupted_as "SIG$s" 1 && not_rerun && r=ok || r=bad
  check "sig$s" "SIG$s to matchapack's pid during npm's extraction: npm rolled back, exit $rc (plain npm $p_rc), node_modules: $nm (plain $p_nm), nothing left running (${alive:-none}) or written after (${newer:-nothing}), logged interrupted" $r "$d"
done
# SIGKILL of matchapack: the socket closes; npm stops (SIGTERM) and rolls
# back after matchapack has exited.
stopped sigkill mp KILL pid reify
[ "$rc" = 137 ] && [ "$nm" = none ] && [ -z "$alive" ] && [ -z "$newer" ] && grep -q 'matchapack has gone; stopping npm (SIGTERM)' "$d.out" && again && r=ok || r=bad
check sigkill "SIGKILL to matchapack during npm's reify: exit 137; npm stopped and rolled back (node_modules: $nm), nothing left running (${alive:-none}) or written after (${newer:-nothing}); the next npm ci succeeds" $r "$d"
# SIGTERM to matchapack's pid while a root script that survives SIGTERM runs
# (npm passes the signal on to it and waits), then SIGKILL 2 s later, as a
# supervisor sends them: npm is gone at once, as plain npm given the same
# two is (the script runs on in both). The hook asked npm to stop, and on
# matchapack's end with no word ends it (SIGKILL).
termkill() { # <case> <plain|mp>: sets npm_alive, script_alive, sigs
  d=$(new_project "$1")
  printf '%s\n' "const fs = require('fs')" "fs.writeFileSync('started', '')" \
    "for (const s of ['SIGTERM', 'SIGINT', 'SIGHUP']) process.on(s, () => fs.appendFileSync('sigs', s + '\\n'))" \
    "setInterval(() => fs.existsSync('loop') || process.exit(0), 50)" > "$d/loop.js"
  python3 -c "import json;p='$d/package.json';d=json.load(open(p));d['scripts']['postinstall']='node loop.js';json.dump(d,open(p,'w'),indent=2)"
  touch "$d/loop"
  local npm_cmd=(matchapack npm); [ "$2" = plain ] && npm_cmd=(env MATCHAPACK=0 "$(command -v npm)")
  (cd "$d" && exec "${nosession[@]}" env PATH="$bin_dir:$PATH" "${npm_cmd[@]}" ci --no-audit --no-fund) > "$d.out" 2>&1 < /dev/null &
  local m=$! _ npm_pid script_pid
  for _ in $(seq 6000); do [ -e "$d/started" ] && break; sleep 0.01; done
  sleep 0.3
  npm_pid=$m; [ "$2" = mp ] && npm_pid=$(pgrep -P "$m" | head -1)
  script_pid=$(descendants "$m" | while read -r p; do [ "$(ps -o command= -p "$p" | tr -s ' ')" = "node loop.js" ] && echo "$p"; done | head -1)
  kill -TERM "$m"; sleep 2; kill -KILL "$m"; wait "$m" 2>/dev/null
  sleep 1
  npm_alive=no; kill -0 "$npm_pid" 2>/dev/null && npm_alive=yes
  script_alive=no; kill -0 "$script_pid" 2>/dev/null && script_alive=yes
  rm -f "$d/loop"; sleep 0.3
  kill -KILL "$npm_pid" "$script_pid" 2>/dev/null
  sigs=$({ tr '\n' ' ' < "$d/sigs"; } 2>/dev/null)
}
termkill termkill-plain plain; p_npm=$npm_alive p_sigs=$sigs
termkill termkill mp
[ "$p_npm" = no ] && [ "$npm_alive" = no ] && [ "$sigs" = "$p_sigs" ] && grep -q 'ending npm (SIGKILL)' "$d.out" && r=ok || r=bad
check term-then-kill "SIGTERM then SIGKILL to matchapack's pid, a root script that survives SIGTERM running: npm gone 1 s after (alive: $npm_alive; plain npm: $p_npm), the script got [$sigs] ([$p_sigs] under plain npm)" $r "$d"
# To the whole group, as `timeout`, `kill -- -<pgid>` and Ctrl-C send it:
# npm gets the kernel's copy and no other (the hook adds none, also when
# its own would come after npm's reify handler has gone: the wait is 1 s
# here, the script lives 1.5 s), so the root script gets as many as under
# plain npm (the group's and npm's pass-on).
stopped group-plain plain TERM group count; p_sigs=$sigs p_rc=$rc
stopped group mp TERM group count MATCHAPACK_STOP_GRACE_MS=1000
[ -n "$p_sigs" ] && [ "$sigs" = "$p_sigs" ] && [ "$rc" = "$p_rc" ] && [ -z "$alive" ] && r=ok || r=bad
check group-term "SIGTERM to matchapack's process group: the root script got [$sigs] ([$p_sigs] under plain npm), exit $rc (plain $p_rc)" $r "$d"
# The same with a SIGTERM listener that a NODE_OPTIONS --require preload
# adds before the hook runs (node runs preloads before its main script): the
# hook still sees npm's SIGTERM (src/inject.js `watchSignals`), and adds none.
printf '%s\n' "process.once('SIGTERM', () => {})" > "$work/preload.js"
stopped group-preload-plain plain TERM group count NODE_OPTIONS="--require $work/preload.js"; p_sigs=$sigs p_rc=$rc
stopped group-preload mp TERM group count MATCHAPACK_STOP_GRACE_MS=1000 NODE_OPTIONS="--require $work/preload.js"
[ -n "$p_sigs" ] && [ "$sigs" = "$p_sigs" ] && [ "$rc" = "$p_rc" ] && [ -z "$alive" ] && r=ok || r=bad
check group-term-preload "SIGTERM to the process group, with a preload's SIGTERM listener: the root script got [$sigs] ([$p_sigs] under plain npm), exit $rc (plain $p_rc)" $r "$d"
# Ctrl-C: the terminal sends SIGINT to its whole foreground group. In a new
# pseudo-terminal whose foreground job is npm ci (plain, or matchapack's),
# ^C typed into it once the script runs; the same comparison.
ctrlc_py='
import os, pty, select, signal, sys, time
mark, out, cmd = sys.argv[1], sys.argv[2], sys.argv[3:]
pid, fd = pty.fork()
if pid == 0:
    os.execvp(cmd[0], cmd)
log, sent, end, done = open(out, "wb"), 0, time.monotonic() + 90, False
times, last = int(os.environ.get("CTRLC_TIMES", "1")), 0.0
while time.monotonic() < end:
    if select.select([fd], [], [], 0.01)[0]:
        try:
            data = os.read(fd, 65536)
        except OSError:
            data = b""
        if not data:
            done = True
            break
        log.write(data)
    if sent < times and os.path.exists(mark) and time.monotonic() - last > 1:
        os.write(fd, b"\x03")
        sent, last = sent + 1, time.monotonic()
if not done:
    # Still running after 90 s: its whole group (the terminal session it
    # leads) is killed and reaped, and the case fails.
    os.killpg(pid, signal.SIGKILL)
    os.waitpid(pid, 0)
    sys.exit("timed out")
os.waitpid(pid, 0)
'
ctrlc() { # <case> <plain|mp> [times, 1 s apart]: sets sigs
  d=$(new_project "$1")
  printf '%s\n' "const fs = require('fs')" "fs.writeFileSync('started', '')" "let first = true" \
    "for (const s of ['SIGTERM', 'SIGINT', 'SIGHUP']) process.on(s, () => { fs.appendFileSync('sigs', s + '\\n'); if (first) { first = false; setTimeout(() => process.exit(0), 1500) } })" \
    "setInterval(() => {}, 1000)" > "$d/loop.js"
  python3 -c "import json;p='$d/package.json';d=json.load(open(p));d['scripts']['postinstall']='node loop.js';json.dump(d,open(p,'w'),indent=2)"
  local npm_cmd=(matchapack npm); [ "$2" = plain ] && npm_cmd=(env MATCHAPACK=0 "$(command -v npm)")
  (cd "$d" && CTRLC_TIMES=${3:-1} exec python3 -c "$ctrlc_py" "$d/started" "$d.out" env PATH="$bin_dir:$PATH" MATCHAPACK_STOP_GRACE_MS=1000 "${npm_cmd[@]}" ci --no-audit --no-fund) < /dev/null
  local timed_out=$?
  sleep 0.5
  sigs=$({ tr '\n' ' ' < "$d/sigs"; } 2>/dev/null)
  [ "$timed_out" = 0 ] || sigs="(timed out after 90 s) $sigs"
}
ctrlc ctrlc-plain plain; p_sigs=$sigs
ctrlc ctrlc mp
[ -n "$p_sigs" ] && [ "$sigs" = "$p_sigs" ] && [[ "$sigs$p_sigs" != *"timed out"* ]] && r=ok || r=bad
check ctrl-c "Ctrl-C in a terminal: the root script got [$sigs] ([$p_sigs] under plain npm)" $r "$d"
# Ctrl-C twice, 1 s apart: the second ends matchapack at once (its stop line
# says so), and npm, which got both from the terminal, gets nothing more.
# Only the SIGINTs are compared: matchapack leads this test's terminal
# session, and its exit hangs the terminal up (SIGHUP to the rest).
ctrlc ctrlc2-plain plain 2; p_ints=$(grep -o SIGINT <<< "$sigs" | wc -l | tr -d ' ')
ctrlc ctrlc2 mp 2; ints=$(grep -o SIGINT <<< "$sigs" | wc -l | tr -d ' ')
[ "$p_ints" -ge 3 ] && [ "$ints" = "$p_ints" ] && [[ "$sigs" != *"timed out"* ]] && r=ok || r=bad
check ctrl-c-twice "Ctrl-C twice in a terminal: the root script got $ints SIGINTs ($p_ints under plain npm)" $r "$d"

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

# 25. `init --alias ls` (a command on PATH): from an rc line (stdout a pipe)
# nothing on stderr; by hand (stdout a terminal, a pty here) the warning.
piped=$(env PATH="$bin_dir:$PATH" matchapack init zsh --alias ls 2>&1 >/dev/null)
tty=$(env PATH="$bin_dir:$PATH" python3 -c 'import pty, sys; pty.spawn(sys.argv[1:])' matchapack init zsh --alias ls 2>&1)
[ -z "$piped" ] && printf '%s\n' "$tty" | grep -q 'warning: `ls` is also' && printf '%s\n' "$tty" | grep -q '^alias ls=matchapack' && r=ok || r=bad
check init-alias-warning "init --alias ls: no warning when stdout is a pipe (an rc line), the warning on a terminal" $r "$work/none"

echo "== $pass passed, $fail failed"
[ "$fail" = 0 ]
