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

# 1. A key with `..` → refused before anything happens.
d=$(setup dotdot "l['packages']['node_modules/../../escape']=dict(l['packages']['node_modules/lodash'])")
if "$bin" plan "$d" >/dev/null 2>&1; then bad "dotdot: plan accepted a .. path"; else ok "dotdot: plan refuses a .. path"; fi

# 2. An absolute key → refused.
d=$(setup absolute "l['packages']['$victim/node_modules/lodash']=dict(l['packages']['node_modules/lodash'])")
if "$bin" plan "$d" >/dev/null 2>&1; then bad "absolute: plan accepted an absolute path"; else ok "absolute: plan refuses an absolute path"; fi

# 3. A link whose target is an absolute directory outside the project: the
#    install may symlink to it, but must not wipe or rewrite anything there.
d=$(setup abslink "l['packages']['node_modules/zz-link']={'resolved':'$victim','link':True}")
"$bin" install "$d" --no-rebuild > "$d.out" 2>&1 && [ -L "$d/node_modules/zz-link" ] && ok "abslink: install ran and placed the link" || bad "abslink: install did not run (case does not test placement)"
[ -d "$victim/node_modules/sentinel" ] && ok "abslink: outside node_modules kept" || bad "abslink: outside node_modules was wiped"
[ "$(shasum "$victim/package.json")" = "$sum0" ] && ok "abslink: outside package.json unchanged" || bad "abslink: outside package.json changed"

echo "== $pass passed, $fail failed"
[ "$fail" = 0 ]
