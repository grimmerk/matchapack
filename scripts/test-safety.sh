#!/usr/bin/env bash
# Safety tests: a tampered lockfile or a symlinked directory must never make
# matchapack delete, clone or write outside the project, and must leave what
# npm does to npm.   test-safety.sh <work-dir>
# The inject engine runs npm's own `npm ci` and only clones into the project
# (the clone server refuses any other destination: `rejected-dest`). So each
# case runs plain `npm ci` on a copy too and expects the same exit status and
# the same effect inside and outside the project: whatever npm itself writes
# (a `file:` directory symlinked elsewhere gets its dependencies installed
# there, by npm), matchapack writes nothing more. Related cases in
# scripts/test-inject.sh: the project's node_modules a symlink (8), a
# workspace symlinked out of the project (11b).
set -uo pipefail
work=$1; here=$(cd "$(dirname "$0")/.." && pwd); bin=$here/target/release/matchapack
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 2; }
mkdir -p "$work"; work=$(cd "$work" && pwd -P)
# Projects in a directory of the user's own group (#41; as test-inject.sh).
chgrp "$(id -g)" "$work"
export MATCHAPACK_NPM_VERSIONS=any MATCHAPACK_STORE=$work/store MATCHAPACK_LOG_DIR=$work/logs MATCHAPACK_PATHS=$work
unset MATCHAPACK_ENGINE MATCHAPACK_INJECT_HOOK MATCHAPACK
pass=0; fail=0
ok() { pass=$((pass+1)); echo "PASS $1"; }
bad() { fail=$((fail+1)); echo "FAIL $1"; }

# matchapack's install; plain npm ci (same flags) on another directory.
install() { "$bin" install "$1" -- --no-audit --no-fund > "$1.out" 2>&1; }
plain() { (cd "$1" && npm ci --no-audit --no-fund > "$1.out" 2>&1); }
mt() { stat -f %m "$1" 2>/dev/null || stat -c %Y "$1"; }
old() { touch -t 202001010000 "$@"; }
# Every path under a directory with its type, mode and mtime (a 2020 mtime
# moves to now on any write into it).
snap() { (cd "$1" && find . -print0 | sort -z | xargs -0 python3 -c '
import os, sys
for p in sys.argv[1:]:
    s = os.lstat(p); print(p, oct(s.st_mode), s.st_mtime_ns)'); }

setup() { # name python-mutation-of-lockfile-dict(l)
  local d=$work/$1; rm -rf "${d:?}"; mkdir -p "$d"
  cp "$here/fixtures/small/package.json" "$here/fixtures/small/package-lock.json" "$d/"
  python3 -c "import json;p='$d/package-lock.json';l=json.load(open(p));$2;json.dump(l,open(p,'w'),indent=2)"
  echo "$d"
}
mkproj() { # dir package.json-text
  rm -rf "${1:?}"; mkdir -p "$1"; printf '%s\n' "$2" > "$1/package.json"
}
lockgen() { (cd "$1" && npm install --package-lock-only --ignore-scripts --no-audit --no-fund >/dev/null 2>&1); }
# twin <dir>: a copy of a prepared project at <dir>-npm, for plain npm ci.
twin() { rm -rf "${1:?}-npm"; cp -R -p "$1" "$1-npm"; }

# Outside the project: a directory with node_modules and a package.json that
# must survive every case below exactly as it is.
victim=$work/victim; rm -rf "${victim:?}"; mkdir -p "$victim/node_modules/sentinel"
echo '{"name":"victim","version":"1.0.0"}' > "$victim/package.json"
old "$victim" "$victim/package.json" "$victim/node_modules" "$victim/node_modules/sentinel"
victim0=$(snap "$victim")

# 1-2. A lockfile key with `..`, and an absolute one, pointing outside the
#      project. npm reads its own lockfile: npm 11 installs the tree it
#      builds from package.json and ignores such keys (status 0); either way
#      plain npm ci and matchapack end the same way, and nothing outside the
#      project changes. (The classic engine's own plan refused them.)
for c in dotdot absolute; do
  if [ $c = dotdot ]; then key="node_modules/../../escape-$c"; else key="$victim/node_modules/lodash"; fi
  d=$(setup "$c" "l['packages']['$key']=dict(l['packages']['node_modules/lodash'])"); twin "$d"
  plain "$d-npm"; want=$?; install "$d"; rc=$?
  [ $rc = $want ] && [ "$(snap "$victim")" = "$victim0" ] && [ ! -e "$work/escape-$c" ] \
    && ok "$c key: the same status as plain npm ci ($rc), nothing written outside" \
    || bad "$c key: matchapack $rc, npm ci $want, outside: $(ls "$work" | grep escape | tr '\n' ' ')$(snap "$victim" | diff <(echo "$victim0") - | head -2)"
done

# 3. A link whose target is an absolute directory outside the project: npm
#    may refuse it or link to it; either way matchapack ends as npm ci does,
#    and nothing in the outside directory is wiped or rewritten.
d=$(setup abslink "l['packages']['node_modules/zz-link']={'resolved':'$victim','link':True}"); twin "$d"
plain "$d-npm"; want=$?; install "$d"; rc=$?
[ $rc = $want ] && [ "$(snap "$victim")" = "$victim0" ] \
  && ok "abslink: the same status as plain npm ci ($rc), the outside directory untouched" \
  || bad "abslink: matchapack $rc, npm ci $want; outside: $(snap "$victim" | diff <(echo "$victim0") - | head -3 | tr '\n' ' ')"

# 4. R1: a `file:./lib` directory with its own node_modules: npm ci does not
#    wipe it (it empties the root's and the workspaces' node_modules only);
#    neither does matchapack.
d=$work/filelib; mkproj "$d" '{"name":"filelib","version":"1.0.0","dependencies":{"lib":"file:./lib","ms":"2.1.3"}}'
mkdir -p "$d/lib"; echo '{"name":"lib","version":"1.0.0"}' > "$d/lib/package.json"; lockgen "$d"
mkdir -p "$d/lib/node_modules/fake"; echo keep > "$d/lib/node_modules/fake/x"; twin "$d"
plain "$d-npm"; want=$?; install "$d"; rc=$?
[ -f "$d-npm/lib/node_modules/fake/x" ] && npm_keeps=yes || npm_keeps=no
[ -f "$d/lib/node_modules/fake/x" ] && keeps=yes || keeps=no
[ $rc = 0 ] && [ $rc = $want ] && [ $keeps = $npm_keeps ] && "$bin" diff "$d-npm" "$d" > "$d.diff" 2>&1 \
  && ok "file-lib: the file: directory's own node_modules kept: $keeps, as by npm ci; tree identical" \
  || bad "file-lib: matchapack $rc (kept $keeps), npm ci $want (kept $npm_keeps); $(tail -1 "$d.diff")"

# 5. R2: installing never touches workspace or file: sources, also when a
#    reinstall removes the old node_modules (node_modules/@s/ws2 is a link to
#    a workspace inside a scope directory).
d=$work/wsmtime; mkproj "$d" '{"name":"wsroot","version":"1.0.0","workspaces":["packages/*"],"dependencies":{"ms":"2.1.3","lib":"file:./lib"}}'
mkdir -p "$d/packages/ws" "$d/packages/ws2" "$d/lib"; echo '{"name":"ws","version":"1.0.0"}' > "$d/packages/ws/package.json"; echo '{"name":"lib","version":"1.0.0"}' > "$d/lib/package.json"
echo '{"name":"@s/ws2","version":"1.0.0"}' > "$d/packages/ws2/package.json"
echo 'x' > "$d/packages/ws/index.js"; echo 'x' > "$d/lib/index.js"; echo 'x' > "$d/packages/ws2/index.js"; lockgen "$d"
srcs=("$d/packages/ws" "$d/packages/ws/index.js" "$d/packages/ws/package.json" "$d/lib" "$d/lib/index.js" "$d/lib/package.json" "$d/packages/ws2" "$d/packages/ws2/index.js" "$d/packages/ws2/package.json")
old "${srcs[@]}"
times0=$(for f in "${srcs[@]}"; do mt "$f"; done)
install "$d" && ok "ws-mtime: install ran" || bad "ws-mtime: install failed: $(tail -1 "$d.out")"
[ -L "$d/node_modules/@s/ws2" ] && install "$d" && ok "ws-mtime: reinstall over the links ran" || bad "ws-mtime: reinstall failed: $(tail -1 "$d.out")"
[ "$(for f in "${srcs[@]}"; do mt "$f"; done)" = "$times0" ] && [ -L "$d/node_modules/@s/ws2" ] && [ -L "$d/node_modules/lib" ] \
  && ok "ws-mtime: workspace and file: sources kept, with their mtimes" || bad "ws-mtime: sources changed or gone"

# 7. H1: a malformed integrity: no panic, and the same status as plain npm
#    ci (npm 11 installs it). The hook gets no usable integrity for that
#    package, so npm extracts it itself (no-integrity); the tree is npm's.
d=$(setup badint "l['packages']['node_modules/lodash']['integrity']='sha512-'"); twin "$d"
plain "$d-npm"; want=$?; install "$d"; rc=$?
[ $rc != 101 ] && [ $rc = $want ] && ! grep -q 'panicked' "$d.out" && { [ $rc != 0 ] || { grep -q 'no-integrity 1' "$d.out" && "$bin" diff "$d-npm" "$d" > "$d.diff" 2>&1; }; } \
  && ok "bad-integrity: no panic, the same status as plain npm ci ($rc)$([ $rc = 0 ] && echo ', that package extracted by npm, tree identical')" || bad "bad-integrity: matchapack $rc, npm ci $want; $(grep -Eo 'extracted by npm [0-9]+ \([^)]*\)' "$d.out")"

# 12. A `file:` dependency directory (its dependencies go to lib/node_modules)
#     is a symlink to a directory outside the project. npm ci installs lib's
#     own dependency there, through the symlink; matchapack refuses to clone
#     there (rejected-dest), so npm extracts that package itself: the outside
#     directory ends as plain npm ci leaves it, and the project's tree too
#     (but for the `lib` link itself: each project points at its own
#     outside directory).
mklibsym() { # dir outside
  mkproj "$1" '{"name":"libsym","version":"1.0.0","dependencies":{"lib":"file:./lib","ms":"2.1.3"}}'
  mkdir -p "$1/lib"; echo '{"name":"lib","version":"1.0.0","dependencies":{"ms":"2.0.0"}}' > "$1/lib/package.json"; lockgen "$1"
  rm -rf "${2:?}"; mkdir -p "$2"; cp "$1/lib/package.json" "$2/"; rm -rf "${1:?}/lib"; ln -s "$2" "$1/lib"
}
d=$work/libsym; o=$work/outside-libsym; mklibsym "$d" "$o"
dn=$work/libsym-npm; on=$work/outside-libsym-npm; mklibsym "$dn" "$on"
plain "$dn"; want=$?; install "$d"; rc=$?
lib_ms() { python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['version'])" "$1/node_modules/ms/package.json" 2>/dev/null; }
[ $rc = 0 ] && [ $want = 0 ] && [ "$(lib_ms "$o")" = 2.0.0 ] && [ "$(lib_ms "$on")" = 2.0.0 ] && grep -q 'rejected-dest 1' "$d.out" \
  && "$bin" diff "$on" "$o" > "$d.diff-outside" 2>&1 && "$bin" diff --ignore lib "$dn" "$d" > "$d.diff" 2>&1 \
  && ok "file-symlink: lib's dependency installed outside by npm (as npm ci does), not cloned there (rejected-dest); both trees identical to npm ci's" \
  || bad "file-symlink: matchapack $rc, npm ci $want, outside ms $(lib_ms "$o") vs $(lib_ms "$on"); $(grep -Eo 'rejected-dest [0-9]+' "$d.out"); $(tail -1 "$d.diff-outside" 2>/dev/null)"

echo "== $pass passed, $fail failed"
[ "$fail" = 0 ]
