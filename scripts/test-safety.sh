#!/usr/bin/env bash
# Safety tests: a tampered lockfile must never make thinclone delete, clone or
# write outside the project.   test-safety.sh <work-dir>
set -uo pipefail
work=$1; here=$(cd "$(dirname "$0")/.." && pwd); bin=$here/target/release/thinclone
pass=0; fail=0
ok() { pass=$((pass+1)); echo "PASS $1"; }
bad() { fail=$((fail+1)); echo "FAIL $1"; }

setup() { # name python-mutation-of-lockfile-dict(l)
  local d=$work/$1; rm -rf "${d:?}"; mkdir -p "$d"
  cp "$here/fixtures/small/package.json" "$here/fixtures/small/package-lock.json" "$d/"
  python3 -c "import json;p='$d/package-lock.json';l=json.load(open(p));$2;json.dump(l,open(p,'w'),indent=2)"
  echo "$d"
}

# Outside the project: a directory with node_modules and a package.json that
# must survive every case below.
victim=$work/victim; rm -rf "${victim:?}"; mkdir -p "$victim/node_modules/sentinel"
echo '{"name":"victim","version":"1.0.0"}' > "$victim/package.json"
sum0=$(shasum "$victim/package.json")
touch -t 202001010000 "$victim/package.json"; mt0=$(stat -f %m "$victim/package.json" 2>/dev/null || stat -c %Y "$victim/package.json")

# 1. A key with `..` → refused before anything happens.
d=$(setup dotdot "l['packages']['node_modules/../../escape']=dict(l['packages']['node_modules/lodash'])")
if "$bin" plan "$d" >/dev/null 2>&1; then bad "dotdot: plan accepted a .. path"; else ok "dotdot: plan refuses a .. path"; fi

# 2. An absolute key → refused.
d=$(setup absolute "l['packages']['$victim/node_modules/lodash']=dict(l['packages']['node_modules/lodash'])")
if "$bin" plan "$d" >/dev/null 2>&1; then bad "absolute: plan accepted an absolute path"; else ok "absolute: plan refuses an absolute path"; fi

# 3. A link whose target is an absolute directory outside the project: the
#    install may symlink to it, but must not wipe or rewrite anything there.
d=$(setup abslink "l['packages']['node_modules/zz-link']={'resolved':'$victim','link':True}")
# Since every install runs npm's own dry-run validation first, npm refuses this
# tampered lockfile before placement; either way, nothing outside may change.
"$bin" install "$d" --no-rebuild > "$d.out" 2>&1 && ok "abslink: install ran" || ok "abslink: refused before placement ($(tail -1 "$d.out" | cut -c1-60))"
[ -d "$victim/node_modules/sentinel" ] && ok "abslink: outside node_modules kept" || bad "abslink: outside node_modules was wiped"
[ "$(shasum "$victim/package.json")" = "$sum0" ] && ok "abslink: outside package.json unchanged" || bad "abslink: outside package.json changed"

[ "$(stat -f %m "$victim/package.json" 2>/dev/null || stat -c %Y "$victim/package.json")" = "$mt0" ] && ok "abslink: outside package.json mtime unchanged" || bad "abslink: outside package.json mtime changed"

mkproj() { # dir package.json-text
  rm -rf "${1:?}"; mkdir -p "$1"; printf '%s\n' "$2" > "$1/package.json"
}
lockgen() { (cd "$1" && npm install --package-lock-only --ignore-scripts --no-audit --no-fund >/dev/null 2>&1); }
old() { touch -t 202001010000 "$@"; }
mt() { stat -f %m "$1" 2>/dev/null || stat -c %Y "$1"; }

# 4. R1: a `file:./lib` directory with its own node_modules is not wiped.
d=$work/filelib; mkproj "$d" '{"name":"filelib","version":"1.0.0","dependencies":{"lib":"file:./lib","ms":"2.1.3"}}'
mkdir -p "$d/lib"; echo '{"name":"lib","version":"1.0.0"}' > "$d/lib/package.json"; lockgen "$d"
mkdir -p "$d/lib/node_modules/fake"; echo keep > "$d/lib/node_modules/fake/x"
"$bin" install "$d" --no-rebuild > "$d.out" 2>&1; rc=$?
[ -f "$d/lib/node_modules/fake/x" ] && ok "file-lib: the file: directory's node_modules kept" || bad "file-lib: the file: directory's node_modules was wiped"
[ $rc != 0 ] && grep -q 'has its own node_modules' "$d.out" && ok "file-lib: refused with the reason" || bad "file-lib: not refused (rc=$rc)"

# 5. R2: installing never touches workspace or file: sources.
d=$work/wsmtime; mkproj "$d" '{"name":"wsroot","version":"1.0.0","workspaces":["packages/*"],"dependencies":{"ms":"2.1.3","lib":"file:./lib"}}'
mkdir -p "$d/packages/ws" "$d/lib"; echo '{"name":"ws","version":"1.0.0"}' > "$d/packages/ws/package.json"; echo '{"name":"lib","version":"1.0.0"}' > "$d/lib/package.json"
echo 'x' > "$d/packages/ws/index.js"; echo 'x' > "$d/lib/index.js"; lockgen "$d"; old "$d/packages/ws/index.js" "$d/lib/index.js"
m1=$(mt "$d/packages/ws/index.js"); m2=$(mt "$d/lib/index.js")
"$bin" install "$d" --no-rebuild > "$d.out" 2>&1 && ok "ws-mtime: install ran" || bad "ws-mtime: install failed: $(tail -1 "$d.out")"
[ "$(mt "$d/packages/ws/index.js")" = "$m1" ] && [ "$(mt "$d/lib/index.js")" = "$m2" ] && ok "ws-mtime: workspace and file: sources keep their mtime" || bad "ws-mtime: source mtimes changed"

# 6. R3: a lockfile missing a transitive entry is refused (npm ci: EUSAGE).
d=$(setup missing "del l['packages']['node_modules/@sinonjs/commons']")
if "$bin" plan "$d" > "$d.out" 2>&1; then bad "missing-entry: plan accepted a lockfile npm ci refuses"; else ok "missing-entry: refused"; fi

# 7. H1: a malformed integrity is refused, not a panic.
d=$(setup badint "l['packages']['node_modules/lodash']['integrity']='sha512-'")
"$bin" install "$d" --no-rebuild > "$d.out" 2>&1; rc=$?
[ $rc != 0 ] && [ $rc != 101 ] && ok "bad-integrity: refused without a panic (rc=$rc)" || bad "bad-integrity: rc=$rc"

# 8. H2: an entry under a link entry is refused.
d=$(setup underlink "l['packages']['node_modules/zz']={'resolved':'zz','link':True}; l['packages']['node_modules/zz/evil']=dict(l['packages']['node_modules/lodash'])")
if "$bin" plan "$d" > "$d.out" 2>&1; then bad "under-link: accepted"; else grep -q 'under a link' "$d.out" && ok "under-link: refused" || bad "under-link: refused for another reason: $(tail -1 "$d.out")"; fi

echo "== $pass passed, $fail failed"
[ "$fail" = 0 ]
