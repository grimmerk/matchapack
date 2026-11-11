#!/usr/bin/env bash
# Inject engine tests (THINCLONE_ENGINE=inject), end to end with the release
# binary and the npm on PATH:   test-inject.sh <work-dir>
# What the fixture harness cannot show: that the hook really cloned, that a
# disabled hook and a store miss still give npm ci's tree, the gates before
# npm starts, that npm's own failures stay npm's, the store lock and the
# project record, and the hook's eligibility rules (test-inject-hook.js).
# Uses its own store and log directory, never the real ones.
set -uo pipefail
work=$1; here=$(cd "$(dirname "$0")/.." && pwd); bin=$here/target/release/thinclone
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 2; }
mkdir -p "$work"; work=$(cd "$work" && pwd -P)
export THINCLONE_NPM_VERSIONS=any THINCLONE_STORE=$work/store THINCLONE_LOG_DIR=$work/logs THINCLONE_PATHS=$work
export THINCLONE_ENGINE=inject
unset THINCLONE_INJECT_HOOK THINCLONE_INJECT_THREADS THINCLONE
store=$work/store
rm -rf "${store:?}" "${work:?}/logs" "${work:?}/store-miss" "${work:?}/cache-miss" "${work:?}/outside-nm" "${work:?}/elsewhere" "${work:?}/store-classic" "${work:?}/store-odd"
pass=0; fail=0
ok() { pass=$((pass+1)); echo "PASS $1"; }
bad() { fail=$((fail+1)); echo "FAIL $1"; [ -f "$work/out" ] && sed 's/^/    /' "$work/out" | tail -6; }
npm_major=$(npm -v | cut -d. -f1)
# npm 12 refuses tarballs from a non-registry URL unless told otherwise; the
# local fixtures install from 127.0.0.1.
[ "$npm_major" -ge 12 ] && export npm_config_allow_remote=all

# The local fixtures install from a tarball server (as in verify-npm.sh).
server=
if ! curl -sf -o /dev/null http://127.0.0.1:48321/lc-q-1.0.0.tgz; then
  python3 -m http.server 48321 --bind 127.0.0.1 --directory "$here/fixtures/optional-upward/tarballs" >/dev/null 2>&1 &
  server=$!
  for _ in $(seq 100); do curl -sf -o /dev/null http://127.0.0.1:48321/lc-q-1.0.0.tgz && break; sleep 0.1; done
fi
trap '[ -n "$server" ] && kill "$server" 2>/dev/null' EXIT

project() { # name fixture [python statement on the package.json dict d]
  local d=$work/$1; rm -rf "${d:?}"; mkdir -p "$d"
  cp "$here/fixtures/$2/package.json" "$here/fixtures/$2/package-lock.json" "$d/"
  if [ -n "${3-}" ]; then python3 -c "import json;p='$d/package.json';d=json.load(open(p));$3;json.dump(d,open(p,'w'),indent=2)"; fi
  echo "$d"
}
# thinclone install <dir> [args]: output in $work/out.
install() { local d=$1; shift; "$bin" install "$d" "$@" -- --no-audit --no-fund > "$work/out" 2>&1; }
cloned() { grep -Eo 'inject: cloned [0-9]+' "$work/out" | grep -Eo '[0-9]+$'; }
filled() { grep -Eo '[0-9]+ new in the store' "$work/out" | grep -Eo '^[0-9]+'; }
by_npm() { grep -Eo 'extracted by npm [0-9]+' "$work/out" | grep -Eo '[0-9]+$'; }
checked() { grep -Eo '[0-9]+ checked against pacote' "$work/out" | grep -Eo '^[0-9]+'; }
# same <ref> <dir>: the same tree as plain npm ci's, hidden lockfile bytes included.
same() { "$bin" diff "$1" "$2" > "$work/diff" 2>&1 && grep -q 'hidden lockfile byte-identical: true' "$work/diff"; }
# The shell function, as a user's shell would have it.
wrapped() { # dir command...
  local d=$1; shift
  (cd "$d" && PATH="$(dirname "$bin"):$PATH" zsh -c ". '$here/shell/thinclone.sh'; $*" > "$work/out" 2>&1)
}
last_log() { tail -1 "$work/logs/log.jsonl" | python3 -c "import json,sys;r=json.load(sys.stdin);print($1)"; }

# Reference trees: plain npm ci.
ref_small=$(project ref-small small); (cd "$ref_small" && npm ci --no-audit --no-fund > "$work/ref.log" 2>&1) || { echo "npm ci failed on fixtures/small"; tail -5 "$work/ref.log"; exit 2; }
ref_local=$(project ref-local bundled); (cd "$ref_local" && npm ci --no-audit --no-fund > "$work/ref.log" 2>&1) || { echo "npm ci failed on fixtures/bundled"; tail -5 "$work/ref.log"; exit 2; }
# Packages npm placed, and how many distinct tarballs they are (a package
# installed at two paths is one store entry).
n_small=$(python3 -c "import json;print(sum(1 for k,v in json.load(open('$ref_small/node_modules/.package-lock.json'))['packages'].items() if not v.get('link') and not v.get('inBundle')))")
d_small=$(python3 -c "import json;print(len({v['integrity'] for k,v in json.load(open('$ref_small/node_modules/.package-lock.json'))['packages'].items() if not v.get('link') and not v.get('inBundle')}))")

# 0. The hook's eligibility rules, against this npm's pacote.
npm_dir=$(cd "$(dirname "$(python3 -c "import os,shutil;print(os.path.realpath(shutil.which('npm')))")")/.." && pwd)
if node "$here/scripts/test-inject-hook.js" "$npm_dir" > "$work/out" 2>&1; then
  ok "hook rules: $(grep -c '^PASS' "$work/out") eligibility cases against pacote $(node -p "require('$npm_dir/node_modules/pacote/package.json').version")"
else
  bad "hook rules: $(grep '^FAIL' "$work/out" | head -3)"
fi

# The pacote and tar npm's arborist loads, as node resolves them: what the
# store records as the maker of the entries it fills.
want_key=$(node -e "
const path = require('path')
const pacoteDir = path.dirname(path.dirname(require.resolve('pacote', { paths: [path.join(process.argv[1], 'node_modules/@npmcli/arborist')] })))
const tar = require(require.resolve('tar/package.json', { paths: [pacoteDir] })).version
console.log('pacote@' + require(path.join(pacoteDir, 'package.json')).version + ' tar@' + tar)" "$npm_dir")

# 1. A first install with an empty store: pacote extracts every package into
#    the store (thinclone's own extractor is not used), then it is cloned.
#    The hook fired.
p=$(project small1 small)
install "$p" && [ "$(cloned)" = "$n_small" ] && [ "$(filled)" = "$d_small" ] && [ "$(by_npm)" = 0 ] && same "$ref_small" "$p" \
  && [ "$(cat "$store/made-by/sha512/"*/* 2>/dev/null | sort -u)" = "$want_key" ] \
  && ok "first install: all $n_small packages cloned (the hook fired), $d_small entries filled by pacote and marked $want_key, tree identical" \
  || bad "first install: cloned $(cloned) of $n_small, filled $(filled), by npm $(by_npm)"

# 2. A reinstall over the existing node_modules, with a changed file and a
#    stray directory in it: npm removes the old tree, nothing new in the store.
echo "// changed" >> "$p/node_modules/lodash/lodash.js"; mkdir -p "$p/node_modules/stray"; echo x > "$p/node_modules/stray/x"
install "$p" && [ "$(cloned)" = "$n_small" ] && [ "$(filled)" = 0 ] && [ ! -e "$p/node_modules/stray" ] && same "$ref_small" "$p" \
  && ok "reinstall: old node_modules replaced, $n_small cloned, 0 new in the store, tree identical" \
  || bad "reinstall: cloned $(cloned), filled $(filled)"
# The edit above went to the clone, never to the store.
! grep -rqs '^// changed' "$store/v2" \
  && ok "a file changed in node_modules left the store entry as it was" || bad "the store entry changed"

# 3. The hook installed but switched off: npm extracts everything itself and
#    the tree is the same.
p=$(project hookoff small)
THINCLONE_INJECT_HOOK=off install "$p" && [ "$(cloned)" = 0 ] && grep -q "hook-off $n_small" "$work/out" && same "$ref_small" "$p" \
  && ok "hook off: 0 cloned, $n_small extracted by npm, tree identical" || bad "hook off: cloned $(cloned)"

# 3b. No hook at all (what an npm that no longer extracts through the
#     patched method would look like): the tree is npm's, and thinclone says
#     that npm placed packages its hook never saw.
p=$(project nohook small)
THINCLONE_INJECT_HOOK=none install "$p" && [ "$(cloned)" = 0 ] && [ "$(by_npm)" = 0 ] \
  && grep -q "npm installed $n_small packages, but only 0 went through thinclone's hook" "$work/out" && same "$ref_small" "$p" \
  && ok "no hook: tree identical, and the $n_small packages the hook never saw are reported" || bad "no hook: cloned $(cloned)"

# 4. A package in neither the store nor npm's cache: pacote fetches it into
#    the store, and it is cloned in the same install.
p=$(project miss bundled)
THINCLONE_STORE=$work/store-miss npm_config_cache=$work/cache-miss install "$p" && [ "$(cloned)" = 3 ] && [ "$(filled)" = 3 ] && same "$ref_local" "$p" \
  && ok "store and npm cache empty: pacote fetches all 3 into the store, all 3 cloned, tree identical" || bad "store miss: cloned $(cloned), filled $(filled)"
THINCLONE_STORE=$work/store-miss npm_config_cache=$work/cache-miss install "$p" && [ "$(cloned)" = 3 ] && [ "$(filled)" = 0 ] && same "$ref_local" "$p" \
  && ok "the next install: 0 new in the store, all 3 cloned" || bad "after a miss: cloned $(cloned), filled $(filled)"

# 4b. Store entries that thinclone's own extractor made (the classic
#     engine's) are compared with pacote's extraction before their first
#     use; the first install under this pacote and tar compares every entry
#     and records them as checked.
cs=$work/store-classic
p=$(project classic-fill small)
THINCLONE_STORE=$cs env -u THINCLONE_ENGINE "$bin" install "$p" --no-rebuild > "$work/out" 2>&1 && [ -z "$(ls "$cs/made-by" 2>/dev/null)" ] || bad "classic fill"
p=$(project classic-then-inject small)
THINCLONE_STORE=$cs install "$p" && [ "$(cloned)" = "$n_small" ] && [ "$(checked)" = "$n_small" ] && grep -q 'checked against pacote (all)' "$work/out" \
  && [ "$(cut -f1 "$cs/inject-checked" | sort -u | wc -l | tr -d ' ')" = 1 ] && same "$ref_small" "$p" \
  && ok "entries made by the classic engine: all $n_small compared before use, the version recorded as checked, tree identical" \
  || bad "classic then inject: cloned $(cloned), checked $(checked)"
# Now made by this pacote and recorded: sampled (one in a million here).
p=$(project sampled small)
THINCLONE_STORE=$cs THINCLONE_INJECT_CHECK=1000000 install "$p" && [ "$(cloned)" = "$n_small" ] && [ "$(checked)" = 0 ] && same "$ref_small" "$p" \
  && ok "the next install: sampled, nothing compared at one in a million" || bad "sampled: checked $(checked)"

# 4c. An entry changed since pacote made it: a check finds it, pacote's
#     extraction is placed and replaces the entry, and thinclone says so.
lp=$(python3 -c "import json;print(json.load(open('$ref_small/node_modules/.package-lock.json'))['packages']['node_modules/lodash']['integrity'])")
lhex=$(python3 -c "import base64;a,b='$lp'.split('-',1);print(base64.b64decode(b).hex())")
lentry=$cs/v2/sha512/${lhex:0:2}/${lhex:2}
chmod 600 "$lentry/package.json"
p=$(project changed small)
THINCLONE_STORE=$cs THINCLONE_INJECT_CHECK=all install "$p" && [ "$(cloned)" = "$n_small" ] && grep -q '1 store entries had changed since' "$work/out" \
  && [ "$(stat -f %Lp "$lentry/package.json" 2>/dev/null || stat -c %a "$lentry/package.json")" = 644 ] && same "$ref_small" "$p" \
  && ok "a store entry changed since it was made: found, replaced by pacote's extraction, tree identical" || bad "changed entry: $(grep -c 'had changed' "$work/out")"

# 4d. An entry an earlier pacote made extracts differently with this one:
#     the rules changed. Replaced, and from then on every entry an earlier
#     pacote made is compared before use (inject-strict).
old="pacote@0.0.1 tar@0.0.1"
echo "$old" > "$cs/made-by/sha512/${lhex:0:2}/${lhex:2}"; chmod 600 "$lentry/package.json"
p=$(project rules-changed small)
THINCLONE_STORE=$cs THINCLONE_INJECT_CHECK=all install "$p" && grep -q "extracts 1 package(s) differently from store entries an earlier pacote made" "$work/out" \
  && [ -s "$cs/inject-strict" ] && same "$ref_small" "$p" \
  && ok "an earlier pacote's entry differs: replaced, this version made strict, tree identical" || bad "rules changed: $(tail -3 "$work/out")"
for f in "$cs"/made-by/sha512/*/*; do echo "$old" > "$f"; done
p=$(project strict small)
THINCLONE_STORE=$cs THINCLONE_INJECT_CHECK=1000000 install "$p" && [ "$(checked)" = "$n_small" ] && same "$ref_small" "$p" \
  && ok "strict: every entry an earlier pacote made is compared before use ($n_small)" || bad "strict: checked $(checked)"

# 4e. A tarball whose path is listed twice: the classic engine's extractor
#     gives the file another mode than npm does. Its entry is compared
#     before the inject engine's first use and replaced by pacote's.
os=$work/store-odd
ref_odd=$(project ref-odd odd-tarballs); (cd "$ref_odd" && npm ci --no-audit --no-fund > "$work/ref.log" 2>&1) || bad "npm ci on odd-tarballs"
p=$(project odd-classic odd-tarballs)
THINCLONE_STORE=$os env -u THINCLONE_ENGINE "$bin" install "$p" --no-rebuild > "$work/out" 2>&1
! same "$ref_odd" "$p" || bad "odd-tarballs: the classic engine was expected to differ (lc-dup/run.sh)"
p=$(project odd-inject odd-tarballs)
THINCLONE_STORE=$os install "$p" && [ "$(cloned)" = 2 ] && same "$ref_odd" "$p" \
  && grep -q "1 store entries made by thinclone's extractor differed from npm's extraction and were replaced (first: .*run.sh: mode 644, the tarball gives 755)" "$work/out" \
  && ok "a path listed twice: the classic engine's entry is replaced by pacote's before use, tree identical" || bad "odd-tarballs inject: cloned $(cloned)"

# 5. The project is recorded with the entries it cloned, so gc keeps them.
p=$(project recorded small); install "$p"
rec=$(grep -l "\"$p\"" "$store"/projects/*.json 2>/dev/null | head -1)
[ -n "$rec" ] && [ "$(python3 -c "import json;print(len(json.load(open('$rec'))['entries']))")" -gt 0 ] \
  && find "$store/v2" -mindepth 3 -maxdepth 3 -type d -exec touch -t 202001010000 {} + \
  && "$bin" store gc --dry-run > "$work/out" 2>&1 && grep -q 'would remove 0 of' "$work/out" \
  && ok "record: the project and its entries are recorded; gc would remove nothing" || bad "record: missing, or gc would remove entries in use"

# 6. The store's shared lock is held while npm runs: a gc started from the
#    root postinstall is refused.
p=$(project locked bundled "d['scripts']={'postinstall':'\"$bin\" store gc > gc.out 2>&1; echo \$? > gc.rc'}")
install "$p" && [ "$(cat "$p/gc.rc")" != 0 ] && grep -q 'in use by a running install' "$p/gc.out" \
  && ok "lock: store gc during the install is refused" || bad "lock: gc ran during an install (rc $(cat "$p/gc.rc" 2>/dev/null))"

# 7. Install scripts' files keep their later times: every touch is done
#    before npm runs a script (script-env's lc-env writes env.json).
#    npm 12 blocks dependency scripts unless allowed; this fixture's
#    package.json `allowScripts` entry is honoured by npm 11 only.
p=$(project times script-env)
allow=(); [ "$npm_major" -ge 12 ] && allow=(npm_config_dangerously_allow_all_scripts=true)
env ${allow[@]+"${allow[@]}"} "$bin" install "$p" -- --no-audit --no-fund > "$work/out" 2>&1 && python3 -c "
import os,sys
d='$p/node_modules/lc-env'
sys.exit(0 if os.stat(d+'/env.json').st_mtime_ns > os.stat(d+'/package.json').st_mtime_ns else 1)" \
  && ok "times: a file written by an install script is newer than the package's cloned files" || bad "times: the cloned files are not older than the script's file"

# 8. Gates before npm starts: nothing in the project changes.
p=$(project umask small); mkdir -p "$p/node_modules/sentinel"
(umask 002; "$bin" install "$p" > "$work/out" 2>&1); rc=$?
[ $rc != 0 ] && grep -q 'umask is 002' "$work/out" && [ -d "$p/node_modules/sentinel" ] \
  && ok "umask 002: refused before npm starts, node_modules untouched" || bad "umask 002: rc=$rc"
(umask 002; wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
[ $rc = 0 ] && grep -q 'running plain npm instead (umask is 002' "$work/out" && ! grep -q 'thinclone: added' "$work/out" && [ ! -e "$p/node_modules/sentinel" ] && [ -d "$p/node_modules/lodash" ] \
  && ok "umask 002 through the shell function: plain npm ci installs" || bad "umask 002 wrapper: rc=$rc"

p=$(project nmlink small); mkdir -p "$work/outside-nm/sentinel"; ln -s "$work/outside-nm" "$p/node_modules"
install "$p"; rc=$?
[ $rc != 0 ] && grep -q 'node_modules is a symlink' "$work/out" && [ -d "$work/outside-nm/sentinel" ] && [ -L "$p/node_modules" ] \
  && ok "node_modules symlink: refused, the link's target untouched" || bad "node_modules symlink: rc=$rc"

p=$(project badengine small)
THINCLONE_ENGINE=bogus "$bin" install "$p" > "$work/out" 2>&1; rc=$?
[ $rc != 0 ] && grep -q 'THINCLONE_ENGINE=bogus is not an engine' "$work/out" && [ ! -e "$p/node_modules" ] \
  && ok "an unknown engine name is an error, not a default" || bad "unknown engine: rc=$rc"
THINCLONE_ENGINE=bogus wrapped "$p" "npm ci --no-audit --no-fund"; rc=$?
[ $rc = 0 ] && grep -q 'running plain npm instead (THINCLONE_ENGINE=bogus' "$work/out" && [ -d "$p/node_modules/lodash" ] \
  && ok "an unknown engine name through the shell function: plain npm" || bad "unknown engine wrapper: rc=$rc"

# 9. Options of the other engine are refused, not ignored.
p=$(project flags small)
"$bin" install "$p" --no-rebuild > "$work/out" 2>&1; rc=$?
[ $rc != 0 ] && grep -q 'not available with THINCLONE_ENGINE=inject' "$work/out" && [ ! -e "$p/node_modules" ] && ok "--no-rebuild is refused with the inject engine" || bad "--no-rebuild: rc=$rc"
env -u THINCLONE_ENGINE "$bin" install "$p" -- --no-audit > "$work/out" 2>&1; rc=$?
[ $rc != 0 ] && grep -q 'need THINCLONE_ENGINE=inject' "$work/out" && [ ! -e "$p/node_modules" ] && ok "npm arguments are refused by the classic engine" || bad "classic with npm args: rc=$rc"
env -u THINCLONE_ENGINE "$bin" install "$p" > "$work/out" 2>&1; rc=$?
[ $rc = 0 ] && ! grep -q 'inject:' "$work/out" && grep -q 'post-install' "$work/out" && ok "without THINCLONE_ENGINE the classic engine installs" || bad "default engine: rc=$rc"

# 10. npm's failures are npm's: same status, said once, nothing retried.
p=$(project outofsync small "d['dependencies']['left-pad']='1.3.0'"); mkdir -p "$p/node_modules/sentinel"
(cd "$p" && npm ci --no-audit --no-fund > "$work/plain.out" 2>&1); want=$?
install "$p"; rc=$?
[ $want != 0 ] && [ $rc = $want ] && [ "$(grep -c 'npm error code EUSAGE' "$work/out")" = 1 ] && [ -d "$p/node_modules/sentinel" ] \
  && ok "lockfile out of sync: npm's own error and status ($rc), node_modules untouched" || bad "out of sync: rc=$rc, npm ci gives $want"
wrapped "$p" "npm ci --no-audit --no-fund"; rc=$?
[ $rc = $want ] && [ "$(grep -c 'npm error code EUSAGE' "$work/out")" = 1 ] && [ "$(last_log "r['outcome'], r['engine'], r['code']")" = "npm_failed inject $want" ] \
  && ok "lockfile out of sync through the shell function: status $rc, npm not run again, logged as npm_failed" || bad "out of sync wrapper: rc=$rc ($(last_log "r['outcome']"))"

# 10b. The helper stops before npm starts (here: an npm whose pacote has no
# fetchers): status 90, nothing changed, so the wrapper declines and runs
# plain npm, once. The fake npm is the real one except for a stub pacote.
real_npm=$(cd "$(dirname "$(python3 -c "import os,shutil;print(os.path.realpath(shutil.which('npm')))")")/.." && pwd -P)
fake=$work/fake-npm; rm -rf "${fake:?}"; mkdir -p "$fake/bin" "$fake/npm/bin" "$fake/npm/node_modules/pacote"
cp "$real_npm/package.json" "$fake/npm/"; cp "$real_npm/bin/npm-cli.js" "$fake/npm/bin/"; ln -s "$real_npm/lib" "$fake/npm/lib"
for m in "$real_npm"/node_modules/* "$real_npm"/node_modules/.bin; do [ "$(basename "$m")" = pacote ] || ln -s "$m" "$fake/npm/node_modules/"; done
echo '{"name":"pacote","version":"0.0.0"}' > "$fake/npm/node_modules/pacote/package.json"; echo 'module.exports = {}' > "$fake/npm/node_modules/pacote/index.js"
ln -s ../npm/bin/npm-cli.js "$fake/bin/npm"
p=$(project helper-declined small)
(PATH=$fake/bin:$PATH; wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
[ $rc = 0 ] && grep -q 'no FetcherBase.prototype.extract' "$work/out" && [ "$(last_log "r['outcome'], r['engine']")" = "declined inject" ] \
  && [ "$(grep -c 'added [0-9]* packages' "$work/out")" = 1 ] && same "$ref_small" "$p" \
  && ok "the helper stops before npm: declined, plain npm runs once" || bad "helper declined: rc=$rc ($(last_log "r['outcome']"))"
p=$(project failscript bundled "d['scripts']={'postinstall':'exit 7'}")
(cd "$p" && npm ci --no-audit --no-fund > "$work/plain.out" 2>&1); want=$?
install "$p"; rc=$?
[ $want != 0 ] && [ $rc = $want ] && [ "$(cloned)" = 3 ] && ok "a failing root script: npm's status ($rc) after cloning" || bad "failing root script: rc=$rc, npm ci gives $want"

# 11. Through the shell function npm's arguments are npm's: a flag the
#     classic engine hands to plain npm is handled, and logged.
p=$(project omit small)
wrapped "$p" "npm ci --omit=dev --no-audit --no-fund"; rc=$?
[ $rc = 0 ] && grep -q 'thinclone: added' "$work/out" && [ ! -d "$p/node_modules/jest" ] && [ -d "$p/node_modules/lodash" ] \
  && [ "$(last_log "r['outcome'], r['engine'], r['inject']['cloned'] > 0, r['inject']['cloned'] < $n_small, '--omit=dev' in r['args'], r['inject']['installed'] == r['inject']['cloned'], r['inject']['unseen']")" = "installed inject True True True True 0" ] \
  && ok "npm ci --omit=dev through the shell function: cloned, dev dependencies absent, logged with the counts" || bad "omit wrapper: rc=$rc"
mkdir -p "$work/elsewhere"; cp "$here/fixtures/bundled/package.json" "$here/fixtures/bundled/package-lock.json" "$work/elsewhere/"
wrapped "$p" "npm ci --prefix '$work/elsewhere' --no-audit --no-fund"; rc=$?
[ $rc = 0 ] && grep -q 'moves the install out of this directory' "$work/out" && ! grep -q 'thinclone: added' "$work/out" && [ -d "$work/elsewhere/node_modules/lc-s" ] \
  && ok "npm ci --prefix: plain npm (the project is not this directory)" || bad "prefix: rc=$rc"

wrapped "$p" "npm ci --dry-run --no-audit --no-fund"; rc=$?
[ $rc = 0 ] && grep -q 'running plain npm instead (option --dry-run installs nothing' "$work/out" && ! grep -q 'thinclone: added' "$work/out" \
  && ok "npm ci --dry-run: plain npm (not an install)" || bad "dry-run: rc=$rc"

# 11b. A workspace directory that is a symlink to a directory outside the
#      project: npm installs the workspace's own dependency there, as npm ci
#      does; thinclone refuses to clone outside the project (rejected-dest),
#      so that package is npm's own extraction.
p=$work/wslink; rm -rf "${p:?}" "${work:?}/outside-ws"; mkdir -p "$p/packages" "$work/outside-ws"
echo '{"name":"wsroot","version":"1.0.0","private":true,"workspaces":["packages/*"],"dependencies":{"lc-s":"http://127.0.0.1:48321/lc-s-1.0.0.tgz"}}' > "$p/package.json"
echo '{"name":"ws","version":"1.0.0","dependencies":{"lc-s":"http://127.0.0.1:48321/lc-s-2.0.0.tgz"}}' > "$work/outside-ws/package.json"
ln -s "$work/outside-ws" "$p/packages/ws"
(cd "$p" && npm install --package-lock-only --ignore-scripts --no-audit --no-fund > "$work/out" 2>&1)
install "$p"; rc=$?
[ $rc = 0 ] && [ "$(cloned)" = 1 ] && grep -q 'rejected-dest 1' "$work/out" && grep -q 'outside the project' "$work/out" \
  && ! grep -q "went through thinclone's hook" "$work/out" \
  && grep -q '"version":"2.0.0"' "$work/outside-ws/node_modules/lc-s/package.json" && grep -q '"version":"1.0.0"' "$p/node_modules/lc-s/package.json" \
  && ok "a workspace symlinked out of the project: its dependency is refused by the clone server and extracted by npm" || bad "symlinked workspace: rc=$rc, cloned $(cloned)"

# 11b. `npm ci` inside a workspace of a monorepo: npm installs into the
#      root's node_modules (the workspace's subset), outside the directory
#      thinclone started in, so nothing could be cloned (#36). Without a
#      lockfile there the wrapper hands it to npm up front; with a stray one
#      the run is logged as npm installing elsewhere, not as a dead hook.
ws=$work/ws-src; rm -rf "${ws:?}"; mkdir -p "$ws/packages/a"
printf '{"name":"ws-root","version":"1.0.0","private":true,"workspaces":["packages/*"],"dependencies":{"lc-r":"http://127.0.0.1:48321/lc-r-1.0.0.tgz"}}\n' > "$ws/package.json"
printf '{"name":"a","version":"1.0.0","dependencies":{"lc-s":"http://127.0.0.1:48321/lc-s-1.0.0.tgz"}}\n' > "$ws/packages/a/package.json"
(cd "$ws" && npm install --package-lock-only --no-audit --no-fund > "$work/ref.log" 2>&1) || bad "workspace lockfile"
wscopy() { local d=$work/$1; rm -rf "${d:?}"; cp -R "$ws" "$d"; echo "$d"; }
ref_ws=$(wscopy ws-ref); (cd "$ref_ws/packages/a" && npm ci --no-audit --no-fund > "$work/ref.log" 2>&1) || bad "npm ci in a workspace"
p=$(wscopy ws-plain)
wrapped "$p/packages/a" "npm ci --no-audit --no-fund"; rc=$?
[ $rc = 0 ] && [ "$(last_log "r['outcome'], r['engine']")" = "declined inject" ] && last_log "r['reason']" | grep -q 'no package-lock.json' \
  && same "$ref_ws" "$p" \
  && ok "npm ci in a workspace directory: handed to npm up front (no lockfile there), tree identical" || bad "workspace: rc=$rc ($(last_log "r['outcome'], r.get('reason')"))"
p=$(wscopy ws-stray); cp "$p/package-lock.json" "$p/packages/a/"
wrapped "$p/packages/a" "npm ci --no-audit --no-fund"; rc=$?
rm -f "$p/packages/a/package-lock.json"; rm -f "$ref_ws/packages/a/package-lock.json"
[ $rc = 0 ] && [ "$(last_log "r['outcome']")" = installed ] && [ "$(last_log "r['inject']['cloned']")" = 0 ] \
  && [ "$(last_log "r['inject']['fallbacks'].get('npm-root-elsewhere', 0) > 0")" = True ] && same "$ref_ws" "$p" \
  && ok "with a stray lockfile there: npm installs at the root, logged as npm-root-elsewhere, tree identical" || bad "workspace stray lockfile: rc=$rc ($(last_log "r['inject']['fallbacks']"))"

# 12. The report counts the inject runs.
"$bin" report > "$work/out" 2>&1 && grep -Eq 'inject engine: [0-9]+ runs, [0-9]+ packages cloned' "$work/out" && grep -q 'npm failed 1' "$work/out" \
  && ok "report: inject runs, clones and npm failures are counted" || bad "report"
# The store checks: compared entries, and the differing ones by who made
# them (a record as inject::run writes it, appended to this run's log).
python3 -c "
import json
r={'outcome':'installed','engine':'inject','dir':'$work/x','npm':'11.19.1','ts':0,'secs':1.0,
   'inject':{'cloned':5,'filled':0,'checked':5,'check_all':True,'fallbacks':{},
             'differs':{'extractor':{'n':2,'first':['sha512-aa: run.sh: mode 644, the tarball gives 755']},
                        'pacote':{'n':1,'first':['sha512-bb: x.js: contents differ']}}}}
print(json.dumps(r))" >> "$work/logs/log.jsonl"
"$bin" report > "$work/out" 2>&1 \
  && grep -Eq "inject engine: [0-9]+ store entries compared with npm's extraction \([0-9]+ runs compared every entry\), differed and replaced: extractor 2, pacote 1" "$work/out" \
  && grep -q "2 differing entries made by the classic engine's extractor (expected for odd tarballs); first: sha512-aa" "$work/out" \
  && grep -q "1 differing entries made by an earlier npm's pacote" "$work/out" \
  && ok "report: entries compared with npm's extraction, and the differing ones by kind" || bad "report: store checks ($(grep 'compared with' "$work/out"))"

# 13. npm 12: a tarball npm refuses (allow-remote=none, its default) is
#     refused the same way with the hook, which is never asked.
if [ "$npm_major" -ge 12 ]; then
  p=$(project remote bundled)
  (unset npm_config_allow_remote; cd "$p" && npm ci --no-audit --no-fund > "$work/plain.out" 2>&1); want=$?
  (unset npm_config_allow_remote; install "$p"); rc=$?
  [ $want != 0 ] && [ $rc = $want ] && grep -q EALLOWREMOTE "$work/plain.out" && grep -q EALLOWREMOTE "$work/out" && [ "$(cloned)" = 0 ] && [ "$(by_npm)" = 0 ] \
    && ok "npm $npm_major allow-remote: EALLOWREMOTE with and without the hook (status $rc), nothing cloned" || bad "allow-remote: rc=$rc, npm ci gives $want"
else
  echo "SKIP allow-remote: npm $npm_major has no default refusal of remote tarballs (npm 12 does)"
fi

echo "== $pass passed, $fail failed"
[ "$fail" = 0 ]
