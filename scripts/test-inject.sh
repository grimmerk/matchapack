#!/usr/bin/env bash
# Inject engine tests (the only engine since PR C2), end to end with the
# release binary and the npm on PATH:
#   test-inject.sh <work-dir>
# What the fixture harness cannot show: that the hook really cloned, that a
# disabled hook and a store miss still give npm ci's tree, the gates before
# npm starts, that npm's own failures stay npm's, the store lock and the
# project record, the hook's eligibility rules and result trap, the helper's
# start-up (test-inject-hook.js), the pacote fingerprint gate, the install
# root npm reports and the gates on it (#36), and npm 12 patches.
# Uses its own store and log directory, never the real ones.
# To run it with another npm, put that npm first on PATH (for example one
# installed with `npm i --prefix <dir> npm@12.2.2`: PATH=<dir>/node_modules/.bin:$PATH).
# An npm whose pacote fingerprint is not in src/npm-fingerprints.json fails
# case 0b and clones nothing; MATCHAPACK_NPM_FINGERPRINT=any runs the cases
# anyway (to see whether its fingerprint can be added).
set -uo pipefail
work=$1; here=$(cd "$(dirname "$0")/.." && pwd); bin=$here/target/release/matchapack
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 2; }
mkdir -p "$work"; work=$(cd "$work" && pwd -P)
# The projects sit in a directory of the user's own group (macOS: new paths
# take their directory's group). Under a directory of a group the user is
# not in (macOS /tmp: wheel), placements moved in from the store's tmp/ are
# left to npm, which changes the counts below (#41; case 4f covers it).
chgrp "$(id -g)" "$work"
export MATCHAPACK_NPM_VERSIONS=any MATCHAPACK_STORE=$work/store MATCHAPACK_LOG_DIR=$work/logs MATCHAPACK_PATHS=$work
unset MATCHAPACK_ENGINE MATCHAPACK_INJECT_HOOK MATCHAPACK_INJECT_THREADS MATCHAPACK
store=$work/store
rm -rf "${store:?}" "${work:?}/logs" "${work:?}/store-miss" "${work:?}/cache-miss" "${work:?}/outside-nm" "${work:?}/elsewhere" "${work:?}/elsewhere-out" "${work:?}/not-there" "${work:?}/not-there-plain" "${work:?}/store-classic" "${work:?}/store-odd" "${work:?}/store-gid" "${work:?}/gid-projects"
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
# matchapack install <dir> [args]: output in $work/out.
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
  (cd "$d" && PATH="$(dirname "$bin"):$PATH" zsh -c ". '$here/shell/matchapack.sh'; $*" > "$work/out" 2>&1)
}
last_log() { tail -1 "$work/logs/log.jsonl" | python3 -c "import json,sys;r=json.load(sys.stdin);print($1)"; }
last_log_in() { tail -1 "$1/log.jsonl" | python3 -c "import json,sys;r=json.load(sys.stdin);print($2)"; }
# classic_entries <store>: make the store's entries what the classic engine
# (removed in PR C2) left in stores it filled: no made-by record, and lc-dup's
# run.sh (listed twice in its tarball) with mode 644 where npm's extraction
# gives 755. Stores filled before PR C2 still hold such entries.
classic_entries() {
  rm -rf "${1:?}/made-by"
  local f; f=$(find "$1/v2" -path '*/run.sh' -type f | head -1)
  [ -z "$f" ] || chmod 644 "$f"
}

# Reference trees: plain npm ci.
ref_small=$(project ref-small small); (cd "$ref_small" && npm ci --no-audit --no-fund > "$work/ref.log" 2>&1) || { echo "npm ci failed on fixtures/small"; tail -5 "$work/ref.log"; exit 2; }
ref_local=$(project ref-local bundled); (cd "$ref_local" && npm ci --no-audit --no-fund > "$work/ref.log" 2>&1) || { echo "npm ci failed on fixtures/bundled"; tail -5 "$work/ref.log"; exit 2; }
# Packages npm placed, and how many distinct tarballs they are (a package
# installed at two paths is one store entry).
n_small=$(python3 -c "import json;print(sum(1 for k,v in json.load(open('$ref_small/node_modules/.package-lock.json'))['packages'].items() if not v.get('link') and not v.get('inBundle')))")
d_small=$(python3 -c "import json;print(len({v['integrity'] for k,v in json.load(open('$ref_small/node_modules/.package-lock.json'))['packages'].items() if not v.get('link') and not v.get('inBundle')}))")

# 0. The hook's eligibility rules, against this npm's pacote, and the
#    helper's start-up (test-inject-hook.js).
npm_dir=$(cd "$(dirname "$(python3 -c "import os,shutil;print(os.path.realpath(shutil.which('npm')))")")/.." && pwd)
if node "$here/scripts/test-inject-hook.js" "$npm_dir" > "$work/out" 2>&1; then
  ok "hook rules: $(grep -c '^PASS' "$work/out") cases (eligibility, trap, fingerprint, helper start-up) against pacote $(node -p "require('$npm_dir/node_modules/pacote/package.json').version")"
else
  bad "hook rules: $(grep '^FAIL' "$work/out" | head -3)"
fi

# 0b. This npm's pacote code is on the list the binary carries (otherwise
#     every install below runs as plain npm and clones nothing).
npm_fp=$(node "$here/src/inject.js" --fingerprint "$npm_dir")
if grep -q "\"$npm_fp\"" "$here/src/npm-fingerprints.json"; then
  ok "fingerprint: npm $(npm -v)'s pacote ($npm_fp) is in src/npm-fingerprints.json"
elif [ "${MATCHAPACK_NPM_FINGERPRINT-}" = any ]; then
  echo "NOTE fingerprint: npm $(npm -v)'s pacote ($npm_fp) is NOT in src/npm-fingerprints.json; running with MATCHAPACK_NPM_FINGERPRINT=any"
else
  bad "fingerprint: npm $(npm -v)'s pacote ($npm_fp) is not in src/npm-fingerprints.json (MATCHAPACK_NPM_FINGERPRINT=any to run the cases anyway)"
fi

# The key of the extraction code npm runs here (pacote's lib/fetcher.js, the
# tar files node loads, libuv's version), as the hook computes it: what the
# store records as the maker of the entries it fills (test-inject-hook.js
# checks that the hook's key is this one).
want_key=$(node "$here/src/inject.js" --key "$npm_dir" | head -1)
want_tar_fp=$(node "$here/src/inject.js" --key "$npm_dir" | sed -n 2p)
[ -n "$want_key" ] || bad "the store key of this npm's extraction code: none"

# 1. A first install with an empty store: pacote extracts every package into
#    the store (matchapack's own extractor is not used), then it is cloned.
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
MATCHAPACK_INJECT_HOOK=off install "$p" && [ "$(cloned)" = 0 ] && grep -q "hook-off $n_small" "$work/out" && same "$ref_small" "$p" \
  && ok "hook off: 0 cloned, $n_small extracted by npm, tree identical" || bad "hook off: cloned $(cloned)"

# 3b. No hook at all (what an npm that no longer extracts through the
#     patched method would look like): the tree is npm's, and matchapack says
#     that npm placed packages its hook never saw.
p=$(project nohook small)
MATCHAPACK_INJECT_HOOK=none install "$p" && [ "$(cloned)" = 0 ] && [ "$(by_npm)" = 0 ] \
  && grep -q "npm installed $n_small packages, but only 0 went through matchapack's hook" "$work/out" && same "$ref_small" "$p" \
  && ok "no hook: tree identical, and the $n_small packages the hook never saw are reported" || bad "no hook: cloned $(cloned)"

# 4. A package in neither the store nor npm's cache: pacote fetches it into
#    the store, and it is cloned in the same install.
p=$(project miss bundled)
MATCHAPACK_STORE=$work/store-miss npm_config_cache=$work/cache-miss install "$p" && [ "$(cloned)" = 3 ] && [ "$(filled)" = 3 ] && same "$ref_local" "$p" \
  && ok "store and npm cache empty: pacote fetches all 3 into the store, all 3 cloned, tree identical" || bad "store miss: cloned $(cloned), filled $(filled)"
MATCHAPACK_STORE=$work/store-miss npm_config_cache=$work/cache-miss install "$p" && [ "$(cloned)" = 3 ] && [ "$(filled)" = 0 ] && same "$ref_local" "$p" \
  && ok "the next install: 0 new in the store, all 3 cloned" || bad "after a miss: cloned $(cloned), filled $(filled)"

# 4b. Store entries that matchapack's own extractor made (the classic
#     engine's, removed in PR C2: entries with no made-by record) are
#     compared with pacote's extraction before their first use, and then
#     recorded as known to this pacote and tar.
cs=$work/store-classic
p=$(project classic-fill small)
MATCHAPACK_STORE=$cs install "$p" && classic_entries "$cs" && [ -z "$(ls "$cs/made-by" 2>/dev/null)" ] || bad "classic-like fill"
p=$(project classic-then-inject small)
MATCHAPACK_STORE=$cs install "$p" && [ "$(cloned)" = "$n_small" ] && [ "$(checked)" = "$n_small" ] \
  && [ "$(cat "$cs/made-by/sha512/"*/* | sort -u)" = "$want_key" ] && same "$ref_small" "$p" \
  && ok "entries made by the classic engine: all $n_small compared before use, then recorded for this extraction code, tree identical" \
  || bad "classic then inject: cloned $(cloned), checked $(checked)"
# Now known to this extraction code: sampled (one in a million here).
p=$(project sampled small)
MATCHAPACK_STORE=$cs MATCHAPACK_INJECT_CHECK=1000000 install "$p" && [ "$(cloned)" = "$n_small" ] && [ "$(checked)" = 0 ] && same "$ref_small" "$p" \
  && ok "the next install: sampled, nothing compared at one in a million" || bad "sampled: checked $(checked)"

# 4c. An entry changed since pacote made it: a check finds it, pacote's
#     extraction is placed and replaces the entry, and matchapack says so.
lp=$(python3 -c "import json;print(json.load(open('$ref_small/node_modules/.package-lock.json'))['packages']['node_modules/lodash']['integrity'])")
lhex=$(python3 -c "import base64;a,b='$lp'.split('-',1);print(base64.b64decode(b).hex())")
lentry=$cs/v2/sha512/${lhex:0:2}/${lhex:2}
chmod 600 "$lentry/package.json"
p=$(project changed small)
MATCHAPACK_STORE=$cs MATCHAPACK_INJECT_CHECK=all install "$p" && [ "$(cloned)" = "$n_small" ] && grep -q '1 store entries had changed since' "$work/out" \
  && [ "$(python3 -c 'import os,sys; print(format(os.stat(sys.argv[1]).st_mode & 0o7777, "o"))' "$lentry/package.json")" = 644 ] && same "$ref_small" "$p" \
  && ok "a store entry changed since it was made: found, replaced by pacote's extraction, tree identical" || bad "changed entry: $(grep -c 'had changed' "$work/out")"

# 4d. Entries known only to other extraction code (an install under
#     another npm) are compared once under this one before use, whatever the
#     sampling rate; one that differs is replaced. Then both versions know the
#     ones that matched: switching back and forth compares nothing again.
old="fetcher:00000000000000000000000000000000 tar:00000000000000000000000000000000 uv:0.0.0"
for f in "$cs"/made-by/sha512/*/*; do echo "$old" > "$f"; done
chmod 600 "$lentry/package.json"
p=$(project other-version small)
MATCHAPACK_STORE=$cs MATCHAPACK_INJECT_CHECK=1000000 install "$p" && [ "$(checked)" = "$n_small" ] \
  && grep -q "extracts 1 package(s) differently from what other extraction code put in the store" "$work/out" \
  && [ "$(python3 -c 'import os,sys; print(format(os.stat(sys.argv[1]).st_mode & 0o7777, "o"))' "$lentry/package.json")" = 644 ] && same "$ref_small" "$p" \
  && ok "entries of other extraction code: all $n_small compared once under this one, the differing one replaced, tree identical" \
  || bad "other version: checked $(checked), $(tail -2 "$work/out")"
python3 - "$cs/made-by" "$old" "$want_key" <<'PY' \
  && ok "the matching entries now record both versions, the replaced one this version only" || bad "made-by after the other version"
import os, sys
d, old, new = sys.argv[1:]
recs = [open(os.path.join(r, f)).read().split("\n")[:-1] for r, _, fs in os.walk(d) for f in fs]
both, only_new = sum(r == [old, new] for r in recs), sum(r == [new] for r in recs)
sys.exit(0 if both + only_new == len(recs) and only_new >= 1 and both >= 1 else 1)
PY
p=$(project both-versions small)
MATCHAPACK_STORE=$cs MATCHAPACK_INJECT_CHECK=1000000 install "$p" && [ "$(checked)" = 0 ] && same "$ref_small" "$p" \
  && ok "known to this version too: nothing compared again" || bad "both versions: checked $(checked)"

# 4e. A tarball whose path is listed twice: the classic engine's extractor
#     gave the file another mode than npm does (classic_entries makes that
#     entry). Its entry is compared before the first use and replaced by
#     pacote's.
os=$work/store-odd
ref_odd=$(project ref-odd odd-tarballs); (cd "$ref_odd" && npm ci --no-audit --no-fund > "$work/ref.log" 2>&1) || bad "npm ci on odd-tarballs"
p=$(project odd-fill odd-tarballs)
MATCHAPACK_STORE=$os install "$p" && classic_entries "$os" || bad "odd-tarballs: classic-like fill"
p=$(project odd-inject odd-tarballs)
MATCHAPACK_STORE=$os install "$p" && [ "$(cloned)" = 2 ] && same "$ref_odd" "$p" \
  && grep -q "1 store entries made by matchapack's extractor differed from npm's extraction and were replaced (first: .*run.sh: mode 644, the tarball gives 755)" "$work/out" \
  && ok "a path listed twice: the classic engine's entry is replaced by pacote's before use, tree identical" || bad "odd-tarballs inject: cloned $(cloned)"

# 4f. #41: a store under another group than the project. npm's extraction
#     gives every path the project's group (macOS: the directory's; Linux:
#     the process's, or a setgid directory's); placements moved in from the
#     store's tmp/ (merges, checked entries) kept the store's. `diff --meta`
#     against plain npm ci, gid class only (the times differ by design):
#     an empty store (fills), then every entry compared.
#     The projects sit in a directory of the user's own group, the store in
#     another group the user is in (setgid, so Linux hands it on too).
gs=$work/store-gid; gp=$work/gid-projects
mine=$(id -g)
other_gid=$(python3 -c "import os;print(next((g for g in os.getgroups() if g!=$mine),''))")
gid_diffs() { "$bin" diff --meta "$1" "$2" > "$work/meta" 2>&1; grep -Eo '^  gid: [0-9]+' "$work/meta" | grep -Eo '[0-9]+$'; }
gproject() { local d=$gp/$1; rm -rf "${d:?}"; mkdir -p "$d"; cp "$here/fixtures/small/package.json" "$here/fixtures/small/package-lock.json" "$d/"; echo "$d"; }
if [ -z "$other_gid" ]; then
  echo "SKIP store under another group: the user is in one group only"
else
  rm -rf "${gp:?}"; mkdir -p "$gp" "$gs"; chgrp "$mine" "$gp"; chgrp "$other_gid" "$gs"; chmod g+s "$gs"
  ref_gid=$(gproject ref); (cd "$ref_gid" && npm ci --no-audit --no-fund > "$work/ref.log" 2>&1) || bad "npm ci for the gid reference"
  p=$(gproject fill)
  MATCHAPACK_STORE=$gs install "$p" && [ "$(cloned)" = "$n_small" ] && [ "$(gid_diffs "$ref_gid" "$p")" = 0 ] && same "$ref_gid" "$p" \
    && [ "$(python3 -c 'import os,sys; print(os.stat(sys.argv[1]).st_gid)' "$gs/v2")" = "$other_gid" ] \
    && ok "store under group $other_gid, empty: all $n_small cloned, every path has npm ci's group ($(grep -Eo '[0-9]+ paths regrouped' "$work/out"))" \
    || bad "gid, empty store: cloned $(cloned), $(gid_diffs "$ref_gid" "$p") paths differ in group"
  p=$(gproject checked)
  MATCHAPACK_STORE=$gs MATCHAPACK_INJECT_CHECK=all install "$p" && [ "$(cloned)" = "$n_small" ] && [ "$(checked)" = "$n_small" ] && [ "$(gid_diffs "$ref_gid" "$p")" = 0 ] \
    && ok "store under group $other_gid, every entry compared: all cloned, every path has npm ci's group" || bad "gid, checked: cloned $(cloned), $(gid_diffs "$ref_gid" "$p") paths differ in group"
  # A project directory of a group the user is NOT in (macOS: /tmp's wheel):
  # npm's files get it from the kernel, matchapack cannot lchown to it. A
  # clone made at the destination gets it too; a placement that would be
  # moved in from tmp/ (a merge) is extracted by npm instead.
  fd=$work.foreign; rm -rf "${fd:?}"; mkdir -p "$fd"
  fgid=$(python3 -c 'import os,sys; print(os.stat(sys.argv[1]).st_gid)' "$fd")
  if python3 -c "import os,sys;sys.exit(0 if $fgid in os.getgroups() or $fgid==os.getegid() or os.geteuid()==0 else 1)"; then
    echo "SKIP a project of a group the user is not in: $fd's group $fgid is one of the user's"
  else
    fproj() { local d=$fd/$1; mkdir -p "$d"; cp "$here/fixtures/small/package.json" "$here/fixtures/small/package-lock.json" "$d/"; echo "$d"; }
    fref=$(fproj ref); (cd "$fref" && npm ci --no-audit --no-fund > "$work/ref.log" 2>&1) || bad "npm ci for the foreign-group reference"
    for chk in off all; do
      p=$(fproj "p-$chk")
      MATCHAPACK_STORE=$gs MATCHAPACK_INJECT_CHECK=$chk install "$p" && [ "$(gid_diffs "$fref" "$p")" = 0 ] && same "$fref" "$p" && [ "$(cloned)" -gt 0 ] \
        && ok "a project of group $fgid, which the user is not in (checks $chk): identical to npm ci, gid included (cloned $(cloned), extracted by npm $(by_npm): $(grep -Eo 'group [0-9]+' "$work/out" | head -1 || true))" \
        || bad "gid, foreign group, checks $chk: cloned $(cloned), by npm $(by_npm), $(gid_diffs "$fref" "$p") paths differ in group"
    done
  fi
fi

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
[ $rc = 0 ] && grep -q 'running plain npm instead (umask is 002' "$work/out" && ! grep -q 'matchapack: added' "$work/out" && [ ! -e "$p/node_modules/sentinel" ] && [ -d "$p/node_modules/lodash" ] \
  && ok "umask 002 through the shell function: plain npm ci installs" || bad "umask 002 wrapper: rc=$rc"

# (Decided on the root npm reports, after npm loaded its config: npm's exit
# handler must stay silent.)
p=$(project nmlink small); mkdir -p "$work/outside-nm/sentinel"; ln -s "$work/outside-nm" "$p/node_modules"
install "$p"; rc=$?
[ $rc != 0 ] && grep -q 'node_modules is a symlink' "$work/out" && [ -d "$work/outside-nm/sentinel" ] && [ -L "$p/node_modules" ] \
  && ! grep -q 'Exit handler never called' "$work/out" \
  && ok "node_modules symlink: refused, the link's target untouched" || bad "node_modules symlink: rc=$rc"

p=$(project badengine small)
MATCHAPACK_ENGINE=bogus "$bin" install "$p" > "$work/out" 2>&1; rc=$?
[ $rc != 0 ] && grep -q 'MATCHAPACK_ENGINE=bogus is not an engine' "$work/out" && [ ! -e "$p/node_modules" ] \
  && ok "an unknown engine name is an error, not a default" || bad "unknown engine: rc=$rc"
MATCHAPACK_ENGINE=bogus wrapped "$p" "npm ci --no-audit --no-fund"; rc=$?
[ $rc = 0 ] && grep -q 'running plain npm instead (MATCHAPACK_ENGINE=bogus' "$work/out" && [ -d "$p/node_modules/lodash" ] \
  && ok "an unknown engine name through the shell function: plain npm" || bad "unknown engine wrapper: rc=$rc"

# 9. The classic engine and its options are gone (PR C2): asking for them is
#    an error before anything changes, and the shell function runs plain npm
#    with the reason. The owner's shell exports MATCHAPACK_ENGINE=inject.
p=$(project flags small)
for o in --no-rebuild "--mtime keep" "--npm-cache $work/cache-miss"; do
  "$bin" install "$p" $o -- --no-audit > "$work/out" 2>&1; rc=$?
  [ $rc = 2 ] && grep -q "unexpected argument '${o%% *}'" "$work/out" && [ ! -e "$p/node_modules" ] && ok "$o (a classic engine option): an error, nothing changed" || bad "$o: rc=$rc"
done
"$bin" plan "$p" > "$work/out" 2>&1; rc=$?
[ $rc = 2 ] && grep -q "unrecognized subcommand 'plan'" "$work/out" && ok "matchapack plan (the classic engine's) is gone" || bad "plan: rc=$rc"
MATCHAPACK_ENGINE=classic "$bin" install "$p" -- --no-audit --no-fund > "$work/out" 2>&1; rc=$?
[ $rc != 0 ] && grep -q 'MATCHAPACK_ENGINE=classic: the classic engine was removed in PR C2' "$work/out" && [ ! -e "$p/node_modules" ] \
  && ok "MATCHAPACK_ENGINE=classic: an error naming the removal, nothing changed" || bad "MATCHAPACK_ENGINE=classic: rc=$rc"
MATCHAPACK_ENGINE=classic wrapped "$p" "npm ci --no-audit --no-fund"; rc=$?
[ $rc = 0 ] && grep -q 'running plain npm instead (MATCHAPACK_ENGINE=classic: the classic engine was removed in PR C2' "$work/out" && ! grep -q 'matchapack: added' "$work/out" \
  && [ "$(last_log "r['outcome'], r['engine']")" = "declined inject" ] && same "$ref_small" "$p" \
  && ok "MATCHAPACK_ENGINE=classic through the shell function: plain npm with the reason, tree identical" || bad "classic wrapper: rc=$rc"
p=$(project explicit small)
MATCHAPACK_ENGINE=inject wrapped "$p" "npm ci --no-audit --no-fund"; rc=$?
[ $rc = 0 ] && grep -q 'matchapack: added' "$work/out" && [ "$(last_log "r['outcome'], r['engine']")" = "installed inject" ] && same "$ref_small" "$p" \
  && ok "MATCHAPACK_ENGINE=inject through the shell function: the inject engine" || bad "inject named: rc=$rc"
# 9b. Without MATCHAPACK_ENGINE, or with it empty, the inject engine
#     installs, directly and through the shell function.
p=$(project default small)
env -u MATCHAPACK_ENGINE "$bin" install "$p" -- --no-audit --no-fund > "$work/out" 2>&1; rc=$?
[ $rc = 0 ] && [ "$(cloned)" = "$n_small" ] && ! grep -q 'post-install' "$work/out" && same "$ref_small" "$p" \
  && ok "without MATCHAPACK_ENGINE the inject engine installs" || bad "default engine: rc=$rc"
(export MATCHAPACK_ENGINE=; wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
[ $rc = 0 ] && grep -q 'npm ci with clones from the store' "$work/out" && [ "$(last_log "r['outcome'], r['engine']")" = "installed inject" ] \
  && ok "MATCHAPACK_ENGINE empty, through the shell function: the inject engine" || bad "default engine wrapper: rc=$rc"

# 10. npm's failures are npm's: same status, said once, nothing retried.
#     (npm before 10.6 prints `npm ERR!`, later ones `npm error`.)
p=$(project outofsync small "d['dependencies']['left-pad']='1.3.0'"); mkdir -p "$p/node_modules/sentinel"
(cd "$p" && npm ci --no-audit --no-fund > "$work/plain.out" 2>&1); want=$?
install "$p"; rc=$?
[ $want != 0 ] && [ $rc = $want ] && [ "$(grep -Ec 'npm (error|ERR!) code EUSAGE' "$work/out")" = 1 ] && [ -d "$p/node_modules/sentinel" ] \
  && ok "lockfile out of sync: npm's own error and status ($rc), node_modules untouched" || bad "out of sync: rc=$rc, npm ci gives $want"
wrapped "$p" "npm ci --no-audit --no-fund"; rc=$?
[ $rc = $want ] && [ "$(grep -Ec 'npm (error|ERR!) code EUSAGE' "$work/out")" = 1 ] && [ "$(last_log "r['outcome'], r['engine'], r['code']")" = "npm_failed inject $want" ] \
  && ok "lockfile out of sync through the shell function: status $rc, npm not run again, logged as npm_failed" || bad "out of sync wrapper: rc=$rc ($(last_log "r['outcome']"))"

# 10a. The version gate is by npm major: this npm (11 or 12) is taken with
#      no override; an npm 13 is handed to plain npm.
# (Run on an npm major not listed yet, to see whether it can be: the first
# case is skipped, since the gate rightly hands that npm to plain npm.)
if grep -q "pub const INJECT_NPM_MAJORS: &\[u32\] = &\[.*\b$npm_major\b" "$here/src/wrapper.rs"; then
  p=$(project gate-ok small)
  (unset MATCHAPACK_NPM_VERSIONS; MATCHAPACK_CONFIG_DIR=$work/cfg-empty wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
  [ $rc = 0 ] && [ "$(last_log "r['outcome'], r['engine']")" = "installed inject" ] \
    && ok "version gate: npm $(npm -v) taken by its major, no override, no npm-verified" || bad "version gate ok: rc=$rc ($(last_log "r['outcome'], r.get('reason')"))"
else
  echo "SKIP version gate: npm $(npm -v) is not a listed major"
fi
real_npm13=$(cd "$(dirname "$(python3 -c "import os,shutil;print(os.path.realpath(shutil.which('npm')))")")/.." && pwd -P)
fake13=$work/fake-npm13; rm -rf "${fake13:?}"; mkdir -p "$fake13/bin" "$fake13/npm/bin"
python3 -c "import json;d=json.load(open('$real_npm13/package.json'));d['version']='13.0.0';json.dump(d,open('$fake13/npm/package.json','w'))"
cp "$real_npm13/bin/npm-cli.js" "$fake13/npm/bin/"; ln -s "$real_npm13/lib" "$fake13/npm/lib"; ln -s "$real_npm13/node_modules" "$fake13/npm/node_modules"
ln -s ../npm/bin/npm-cli.js "$fake13/bin/npm"
p=$(project gate-13 small)
(unset MATCHAPACK_NPM_VERSIONS; PATH=$fake13/bin:$PATH; MATCHAPACK_CONFIG_DIR=$work/cfg-empty wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
[ $rc = 0 ] && [ "$(last_log "r['outcome'], r['engine']")" = "declined inject" ] && last_log "r['reason']" | grep -q 'npm 13.0.0: the inject engine (the default) runs on npm' \
  && same "$ref_small" "$p" \
  && ok "version gate: npm 13 handed to plain npm, tree identical" || bad "version gate 13: rc=$rc ($(last_log "r['outcome'], r.get('reason')"))"

# 10b. The helper stops before npm starts (here: an npm whose pacote has no
# lib/fetcher.js to take the fingerprint from): status 90, nothing changed,
# so the wrapper declines and runs plain npm, once. The fake npm is the real
# one except for a stub pacote.
real_npm=$(cd "$(dirname "$(python3 -c "import os,shutil;print(os.path.realpath(shutil.which('npm')))")")/.." && pwd -P)
fake=$work/fake-npm; rm -rf "${fake:?}"; mkdir -p "$fake/bin" "$fake/npm/bin" "$fake/npm/node_modules/pacote"
cp "$real_npm/package.json" "$fake/npm/"; cp "$real_npm/bin/npm-cli.js" "$fake/npm/bin/"; ln -s "$real_npm/lib" "$fake/npm/lib"
for m in "$real_npm"/node_modules/* "$real_npm"/node_modules/.bin; do [ "$(basename "$m")" = pacote ] || ln -s "$m" "$fake/npm/node_modules/"; done
echo '{"name":"pacote","version":"0.0.0"}' > "$fake/npm/node_modules/pacote/package.json"; echo 'module.exports = {}' > "$fake/npm/node_modules/pacote/index.js"
ln -s ../npm/bin/npm-cli.js "$fake/bin/npm"
p=$(project helper-declined small)
(PATH=$fake/bin:$PATH; wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
[ $rc = 0 ] && grep -q "running plain npm instead (cannot read npm's pacote (.*fetcher.js): ENOENT)" "$work/out" && [ "$(last_log "r['outcome'], r['engine']")" = "declined inject" ] \
  && [ "$(grep -c 'added [0-9]* packages' "$work/out")" = 1 ] && same "$ref_small" "$p" \
  && ok "the helper stops before npm: declined, plain npm runs once" || bad "helper declined: rc=$rc ($(last_log "r['outcome']"))"

# npm_copy <name>: a full copy of the npm on PATH, as $work/<name>/bin/npm,
# for the doctored npm of the cases below (a copy, so that npm and the hook
# load the same changed files).
npm_copy() {
  local c=$work/$1; rm -rf "${c:?}"; mkdir -p "$c/bin"
  cp -R "$real_npm" "$c/npm"; ln -s ../npm/bin/npm-cli.js "$c/bin/npm"; echo "$c"
}

# 4g. The store key is the extraction code, not the versions: an npm whose
#     pacote says another version but runs the same code (a full copy with
#     only node_modules/pacote/package.json's version changed) compares
#     nothing in a store the original npm filled.
ks=$work/store-key; rm -rf "${ks:?}"
p=$(project key-fill small)
MATCHAPACK_STORE=$ks install "$p" && [ "$(filled)" = "$d_small" ] || bad "key: fill with this npm ($(cloned) cloned)"
kv=$(npm_copy npm-key-version)
python3 -c "import json,sys;p=sys.argv[1];d=json.load(open(p));d['version']=d['version']+'-matchapack';json.dump(d,open(p,'w'),indent=2)" "$kv/npm/node_modules/pacote/package.json"
p=$(project key-version small)
(PATH=$kv/bin:$PATH; MATCHAPACK_STORE=$ks MATCHAPACK_INJECT_CHECK=1000000 install "$p") && [ "$(cloned)" = "$n_small" ] && [ "$(checked)" = 0 ] && same "$ref_small" "$p" \
  && grep -q "inject: pacote [0-9.]*-matchapack, .*, extraction code $want_key\$" "$work/out" && ! grep -q 'extraction code was new to the store' "$work/out" \
  && ok "key: another pacote version with the same code compares nothing ($n_small cloned, 0 checked), tree identical" \
  || bad "key: same code, other pacote version: cloned $(cloned), checked $(checked)"

# 4h. One comment line added to the tar file npm loads: every entry is
#     compared once under the new code (and the summary says why the install
#     was slow), none in the next install, the records hold both keys, and
#     back on the original npm nothing is compared.
kt=$(npm_copy npm-key-tar)
tar_main=$(node -e "
const path = require('path')
const pacote = require.resolve('pacote', { paths: [path.join(process.argv[1], 'node_modules/@npmcli/arborist')] })
console.log(require.resolve('tar', { paths: [path.dirname(path.dirname(pacote))] }))" "$kt/npm")
printf '\n// matchapack test: changed tar code\n' >> "$tar_main"
tar_key=$(node "$here/src/inject.js" --key "$kt/npm" | head -1)
p=$(project key-tar small)
(PATH=$kt/bin:$PATH; MATCHAPACK_STORE=$ks MATCHAPACK_INJECT_CHECK=1000000 install "$p") && [ "$(cloned)" = "$n_small" ] && [ "$(checked)" = "$n_small" ] && same "$ref_small" "$p" \
  && [ "$tar_key" != "$want_key" ] && [ "${tar_key%% *}" = "${want_key%% *}" ] && grep -q "extraction code $tar_key\$" "$work/out" \
  && grep -q "matchapack: this npm's extraction code was new to the store: $n_small packages were compared with npm's own extraction, once (matchapack store verify does this ahead of time)" "$work/out" \
  && ok "key: tar's code changed (${tar_main#"$kt/npm/"}): all $n_small compared once, the summary says why, tree identical" \
  || bad "key: tar changed: cloned $(cloned), checked $(checked), key $tar_key"
p=$(project key-tar2 small)
(PATH=$kt/bin:$PATH; MATCHAPACK_STORE=$ks MATCHAPACK_INJECT_CHECK=1000000 install "$p") && [ "$(checked)" = 0 ] && same "$ref_small" "$p" \
  && ! grep -q 'extraction code was new to the store' "$work/out" \
  && ok "key: the next install under that tar compares nothing, and says nothing about it" || bad "key: tar changed, second install: checked $(checked)"
python3 - "$ks/made-by" "$want_key" "$tar_key" <<'PY' && ok "key: every record holds both keys" || bad "key: records after the tar change"
import os, sys
d, a, b = sys.argv[1:]
recs = [open(os.path.join(r, f)).read().split("\n")[:-1] for r, _, fs in os.walk(d) for f in fs]
sys.exit(0 if recs and all(r == [a, b] for r in recs) else 1)
PY
p=$(project key-back small)
MATCHAPACK_STORE=$ks MATCHAPACK_INJECT_CHECK=1000000 install "$p" && [ "$(checked)" = 0 ] && same "$ref_small" "$p" \
  && ok "key: back on the original npm, nothing is compared" || bad "key: back on the original npm: checked $(checked)"

# declined_keys <store>: the keys on the store's decline list (one file per
# key in <store>/declined, the key on its first line), sorted.
declined_keys() { for f in "$1"/declined/*; do [ -f "$f" ] && head -1 "$f"; done | sort; }

# 4i. pacote's extraction is no longer what a clone reproduces: an npm whose
#     pacote keeps the tarball's file times (noMtime off; it runs with
#     MATCHAPACK_NPM_FINGERPRINT=any). Its code is new to the store, so the
#     entries are compared, and the first comparison sees files older than
#     the install: through the shell function the run counts as broken and
#     plain npm ci rebuilds node_modules, and the code goes on the store's
#     decline list. The next install under it leaves every package to npm.
km=$(npm_copy npm-key-mtime)
python3 - "$km/npm/node_modules/pacote/lib/fetcher.js" <<'PY' || bad "key: could not turn pacote's noMtime off"
import sys
p = sys.argv[1]; s = open(p).read()
assert s.count("noMtime: true,") == 1
open(p, "w").write(s.replace("noMtime: true,", "noMtime: false,"))
PY
mt_key=$(node "$here/src/inject.js" --key "$km/npm" | head -1)
ref_mt=$(project ref-mtime small); (cd "$ref_mt" && PATH=$km/bin:$PATH npm ci --no-audit --no-fund > "$work/ref.log" 2>&1) || bad "npm ci with the noMtime-off npm"
mt_log=$work/logs-mtime; rm -rf "${mt_log:?}"
p=$(project key-mtime small)
(PATH=$km/bin:$PATH; export MATCHAPACK_NPM_FINGERPRINT=any MATCHAPACK_STORE=$ks MATCHAPACK_LOG_DIR=$mt_log MATCHAPACK_INJECT_CHECK=1000000; wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
[ $rc = 0 ] && grep -q "running npm ci to rebuild node_modules" "$work/out" && grep -q "not what matchapack's clones reproduce (.*before this install began)" "$work/out" \
  && [ "$(last_log_in "$mt_log" "r['outcome']")" = broke ] && [ "$(declined_keys "$ks")" = "$mt_key" ] && same "$ref_mt" "$p" \
  && ok "key: an extraction with old file times: the run is rebuilt by plain npm ci, the code declined in this store, tree identical" \
  || bad "key: old file times: rc=$rc, $(last_log_in "$mt_log" "r['outcome'], r.get('reason')")"
p=$(project key-mtime2 small)
(PATH=$km/bin:$PATH; export MATCHAPACK_NPM_FINGERPRINT=any MATCHAPACK_STORE=$ks MATCHAPACK_LOG_DIR=$mt_log MATCHAPACK_INJECT_CHECK=1000000; wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
[ $rc = 0 ] && grep -q "no clones from the store, npm extracted the packages itself (this npm's extraction code is on the store's decline list" "$work/out" \
  && [ "$(last_log_in "$mt_log" "r['outcome'], r['inject']['cloned'], r['inject']['fallbacks'].get('key-declined', 0) > 0")" = "installed 0 True" ] && same "$ref_mt" "$p" \
  && ok "key: the next install under the declined code: npm extracts every package, said once, tree identical" \
  || bad "key: declined code, second install: rc=$rc, $(last_log_in "$mt_log" "r['outcome'], (r.get('inject') or {}).get('cloned')")"
MATCHAPACK_LOG_DIR=$mt_log "$bin" report > "$work/report" 2>&1 && grep -q "in 1 runs pacote's extraction was not what the clones reproduce" "$work/report" \
  && ok "key: matchapack report counts the run whose extraction was not what the clones reproduce" || bad "key: report ($(grep -c reproduce "$work/report"))"

# 4j. The same npm with an empty store: pacote's extractions would become the
#     entries (fills, nothing to compare with). They are checked the same
#     way first: the run is rebuilt by plain npm ci, the code is declined,
#     and no entry is left recorded as this code's.
es=$work/store-mtime-empty; rm -rf "${es:?}"
mt_log2=$work/logs-mtime-empty; rm -rf "${mt_log2:?}"
p=$(project key-mtime-empty small)
(PATH=$km/bin:$PATH; export MATCHAPACK_NPM_FINGERPRINT=any MATCHAPACK_STORE=$es MATCHAPACK_LOG_DIR=$mt_log2; wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
[ $rc = 0 ] && grep -q "running npm ci to rebuild node_modules" "$work/out" && grep -q "not what matchapack's clones reproduce (.*before this install began)" "$work/out" \
  && [ "$(last_log_in "$mt_log2" "r['outcome']")" = broke ] && [ "$(declined_keys "$es")" = "$mt_key" ] \
  && [ -z "$(cat "$es"/made-by/sha512/*/* 2>/dev/null | grep -xF "$mt_key")" ] && same "$ref_mt" "$p" \
  && ok "key: the same with an empty store: the fills are checked too, the run rebuilt by plain npm ci, the code declined, no entry recorded under it, tree identical" \
  || bad "key: old file times, empty store: rc=$rc, $(last_log_in "$mt_log2" "r['outcome'], r.get('reason')")"
p=$(project key-mtime-empty2 small)
(PATH=$km/bin:$PATH; export MATCHAPACK_NPM_FINGERPRINT=any MATCHAPACK_STORE=$es MATCHAPACK_LOG_DIR=$mt_log2; wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
[ $rc = 0 ] && grep -q "this npm's extraction code is on the store's decline list" "$work/out" \
  && [ "$(last_log_in "$mt_log2" "r['outcome'], r['inject']['cloned']")" = "installed 0" ] && same "$ref_mt" "$p" \
  && ok "key: the next install there: npm extracts every package, tree identical" || bad "key: empty store, second install: rc=$rc"
# Clearing a decline by hand: remove its file; the next install clones again
# (from the original npm's code here, which was never declined).
dk=$(grep -lxF "$mt_key" "$es"/declined/* 2>/dev/null)
[ -n "$dk" ] && rm -f "$dk" && [ -z "$(declined_keys "$es")" ] && ok "key: removing the key's file in <store>/declined clears the decline" || bad "key: clearing a decline (${dk:-no file for the key})"

# 10c. The fingerprint gate: an npm whose pacote class has one statement more
#      inside a private method (#entryMode: not part of extract's own text)
#      runs as plain npm before anything changes, with the fingerprint and
#      the way out in the message, logged, and counted by `matchapack report`.
#      MATCHAPACK_NPM_FINGERPRINT=any, or that fingerprint, lets it clone;
#      another fingerprint does not.
fpc=$(npm_copy fp-npm)
python3 - "$fpc/npm/node_modules/pacote/lib/fetcher.js" <<'PY' || bad "fingerprint: could not change pacote's #entryMode"
import re, sys
p = sys.argv[1]; s = open(p).read()
s, n = re.subn(r"(\n  (?:#entryMode|\[_entryMode\]) \(path, mode, type\) \{\n)", r"\1    void 'matchapack test: changed pacote code'\n", s)
assert n == 1, n
open(p, "w").write(s)
PY
fp_new=$(node "$here/src/inject.js" --fingerprint "$fpc/npm")
fp_log=$work/logs-fp; rm -rf "${fp_log:?}"
p=$(project fp-unknown small); mkdir -p "$p/node_modules/sentinel"
(unset MATCHAPACK_NPM_FINGERPRINT; PATH=$fpc/bin:$PATH; MATCHAPACK_LOG_DIR=$fp_log wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
[ $rc = 0 ] && [ "$fp_new" != "$npm_fp" ] \
  && grep -qF "matchapack: running plain npm instead (npm $(npm -v) has pacote code this matchapack was not tested with; update matchapack, or set MATCHAPACK_NPM_FINGERPRINT=any to try it; fingerprint $fp_new)" "$work/out" \
  && ! grep -q 'matchapack inject helper' "$work/out" && [ "$(grep -c 'added [0-9]* packages' "$work/out")" = 1 ] && [ ! -e "$p/node_modules/sentinel" ] \
  && [ "$(MATCHAPACK_LOG_DIR=$fp_log last_log_in "$fp_log" "r['outcome'], r['engine']")" = "declined inject" ] && same "$ref_small" "$p" \
  && ok "fingerprint gate: pacote code not on the list ($fp_new) runs as plain npm, once, with the message, tree identical" \
  || bad "fingerprint gate, unknown: rc=$rc, fingerprint $fp_new ($(grep '^matchapack' "$work/out" | head -2))"
MATCHAPACK_LOG_DIR=$fp_log "$bin" report > "$work/report" 2>&1 \
  && grep -qF "npm fingerprint gate: 1 runs went to plain npm, pacote code not tested with this build (npm $(npm -v) ${fp_new:0:16}…: 1)" "$work/report" \
  && ok "fingerprint gate: matchapack report counts the run" || bad "fingerprint gate: report ($(grep fingerprint "$work/report"))"
for fpv in any "$fp_new" "$npm_fp,$fp_new"; do
  p=$(project fp-accepted small)
  (PATH=$fpc/bin:$PATH; export MATCHAPACK_NPM_FINGERPRINT=$fpv; MATCHAPACK_LOG_DIR=$fp_log wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
  [ $rc = 0 ] && grep -q 'matchapack: added' "$work/out" && [ "$(last_log_in "$fp_log" "r['outcome'], r['inject']['cloned'], r['inject']['fingerprint']")" = "installed $n_small $fp_new" ] \
    && same "$ref_small" "$p" \
    && ok "fingerprint gate: MATCHAPACK_NPM_FINGERPRINT=${fpv:0:20}… takes it: $n_small cloned, tree identical" || bad "fingerprint gate, override ${fpv:0:20}: rc=$rc"
done
p=$(project fp-other small)
(PATH=$fpc/bin:$PATH; export MATCHAPACK_NPM_FINGERPRINT=$npm_fp; MATCHAPACK_LOG_DIR=$fp_log wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
[ $rc = 0 ] && grep -q "running plain npm instead (npm .* has pacote code this matchapack was not tested with" "$work/out" \
  && ok "fingerprint gate: a MATCHAPACK_NPM_FINGERPRINT naming another fingerprint does not take it" || bad "fingerprint gate, other override: rc=$rc"
p=$(project fp-bad small)
MATCHAPACK_NPM_FINGERPRINT=${fp_new:0:16} "$bin" install "$p" -- --no-audit --no-fund > "$work/out" 2>&1; rc=$?
[ $rc != 0 ] && grep -q "MATCHAPACK_NPM_FINGERPRINT: ${fp_new:0:16} is not a fingerprint" "$work/out" && [ ! -e "$p/node_modules" ] \
  && ok "fingerprint gate: a malformed MATCHAPACK_NPM_FINGERPRINT is refused before npm starts" || bad "fingerprint gate, malformed: rc=$rc"

# 10d. The trap on the hook's result: an npm whose caller reads a key of
#      extract's result other than resolved / integrity / from (here pacote's
#      exported extract, which arborist calls, reads `files`): matchapack
#      treats the install as broken, logs the key, and through the shell
#      function npm ci rebuilds node_modules (npm's "added" line twice: the
#      hooked run, then the plain one); `matchapack install` stops with an
#      error.
trc=$(npm_copy trap-npm)
python3 - "$trc/npm/node_modules/pacote/lib/index.js" <<'PY' || bad "trap: could not change pacote's extract"
import sys
p = sys.argv[1]; s = open(p).read()
old = "extract: (spec, dest, opts) => get(spec, opts).extract(dest),"
assert s.count(old) == 1
s = s.replace(old, "extract: (spec, dest, opts) => get(spec, opts).extract(dest).then(r => { void r.files; return r }),")
open(p, "w").write(s)
PY
tr_log=$work/logs-trap; rm -rf "${tr_log:?}"
p=$(project trap small)
(PATH=$trc/bin:$PATH; MATCHAPACK_LOG_DIR=$tr_log wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
[ $rc = 0 ] && grep -q "matchapack: failed while npm was running (npm read \`files\` ([0-9]*×) of what matchapack's hook returned" "$work/out" \
  && grep -q 'running npm ci to rebuild node_modules' "$work/out" && [ "$(grep -c 'added [0-9]* packages' "$work/out")" = 2 ] \
  && [ "$(last_log_in "$tr_log" "r['outcome'], '\`files\`' in r['reason']")" = "broke True" ] && same "$ref_small" "$p" \
  && ok "trap: npm reading another key of the hook's result is a broken install; npm ci rebuilt it, logged with the key, tree identical" \
  || bad "trap, shell function: rc=$rc ($(grep '^matchapack' "$work/out" | head -3))"
MATCHAPACK_LOG_DIR=$tr_log "$bin" report > "$work/report" 2>&1 \
  && grep -q "inject engine: in 1 runs npm read an unexpected key of the hook's result, and npm ci rebuilt node_modules" "$work/report" \
  && ok "trap: matchapack report counts the run" || bad "trap: report ($(grep 'unexpected key' "$work/report"))"
p=$(project trap-direct small)
(PATH=$trc/bin:$PATH; install "$p"); rc=$?
[ $rc != 0 ] && grep -q "npm read \`files\` ([0-9]*×) of what matchapack's hook returned" "$work/out" \
  && ok "trap: matchapack install stops with the error (status $rc)" || bad "trap, matchapack install: rc=$rc"

# 10e. npm's pacote has the file on the list but not the shape the hook
#      needs (here: its exports lack RegistryFetcher, which npm itself does
#      not use). Found only when npm has loaded pacote, so npm is running: no
#      hook, npm extracts every package itself, said once and logged; no
#      "unseen" warning (the line says why).
shc=$(npm_copy shape-npm)
python3 - "$shc/npm/node_modules/pacote/lib/index.js" <<'PY' || bad "shape: could not change pacote's exports"
import sys
p = sys.argv[1]; s = open(p).read()
old = "  RegistryFetcher,\n"
assert s.count(old) == 1
open(p, "w").write(s.replace(old, ""))
PY
p=$(project shape small)
(PATH=$shc/bin:$PATH; wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
[ $rc = 0 ] && grep -q "matchapack: no clones from the store, npm extracted the packages itself (the hook was not installed: this npm's pacote has no FetcherBase.prototype.extract" "$work/out" \
  && ! grep -q "went through matchapack's hook" "$work/out" && [ "$(grep -c '^added [0-9]* packages' "$work/out")" = 1 ] \
  && grep -q "matchapack: added 0 packages from the store in .* (inject engine; npm extracted $n_small)" "$work/out" \
  && [ "$(last_log "r['outcome'], r['inject']['cloned'], r['inject']['hook'], r['inject']['unseen'] == r['inject']['installed']")" = "installed 0 not-installed True" ] \
  && same "$ref_small" "$p" \
  && ok "pacote of another shape, found when npm loads it: no hook, npm extracts every package, said and logged, tree identical" \
  || bad "shape found late: rc=$rc ($(grep '^matchapack' "$work/out" | head -2))"

# 10f. npm's install root cannot be read (here: an npm whose Npm class has
#      no exec on its prototype): no root, so the server refuses every
#      request (`no-root`); npm extracts every package itself, said and
#      logged, tree identical.
nxc=$(npm_copy noexec-npm)
python3 - "$nxc/npm/lib/npm.js" <<'PY' || bad "no exec: could not change npm's lib/npm.js"
import sys
p = sys.argv[1]; s = open(p).read()
old = "\nmodule.exports = Npm\n"
assert s.count(old) == 1
s = s.replace(old, "\nconst npmExecImpl = Npm.prototype.exec\ndelete Npm.prototype.exec\nmodule.exports = class extends Npm {\n  constructor (...a) {\n    super(...a)\n    this.exec = npmExecImpl\n  }\n}\n")
open(p, "w").write(s)
PY
p=$(project noexec small)
(PATH=$nxc/bin:$PATH; wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
[ $rc = 0 ] && grep -q "matchapack: no clones from the store, npm extracted the packages itself (could not learn npm's install root: npm's lib/npm.js has no Npm.prototype.exec)" "$work/out" \
  && [ "$(last_log "r['outcome'], r['inject']['cloned'], r['inject']['fallbacks'].get('no-root'), r['inject']['root']")" = "installed 0 $n_small None" ] \
  && same "$ref_small" "$p" \
  && ok "npm's root cannot be read: every request refused (no-root), npm extracts all $n_small, said and logged, tree identical" \
  || bad "no exec: rc=$rc ($(grep '^matchapack' "$work/out" | head -2); $(last_log "r['inject'].get('fallbacks')"))"

p=$(project failscript bundled "d['scripts']={'postinstall':'exit 7'}")
(cd "$p" && npm ci --no-audit --no-fund > "$work/plain.out" 2>&1); want=$?
install "$p"; rc=$?
[ $want != 0 ] && [ $rc = $want ] && [ "$(cloned)" = 3 ] && ok "a failing root script: npm's status ($rc) after cloning" || bad "failing root script: rc=$rc, npm ci gives $want"

# 11. Through the shell function npm's arguments are npm's: a flag that
#     changes the tree is handled, and logged.
p=$(project omit small)
wrapped "$p" "npm ci --omit=dev --no-audit --no-fund"; rc=$?
[ $rc = 0 ] && grep -q 'matchapack: added' "$work/out" && [ ! -d "$p/node_modules/jest" ] && [ -d "$p/node_modules/lodash" ] \
  && [ "$(last_log "r['outcome'], r['engine'], r['inject']['cloned'] > 0, r['inject']['cloned'] < $n_small, '--omit=dev' in r['args'], r['inject']['installed'] == r['inject']['cloned'], r['inject']['unseen']")" = "installed inject True True True True 0" ] \
  && ok "npm ci --omit=dev through the shell function: cloned, dev dependencies absent, logged with the counts" || bad "omit wrapper: rc=$rc"

# 11a. The install root is npm's own (#36): with --prefix npm installs into
#      that directory, and matchapack clones there.
mkdir -p "$work/elsewhere"; cp "$here/fixtures/bundled/package.json" "$here/fixtures/bundled/package-lock.json" "$work/elsewhere/"
wrapped "$p" "npm ci --prefix '$work/elsewhere' --no-audit --no-fund"; rc=$?
[ $rc = 0 ] && grep -q 'matchapack: added' "$work/out" \
  && [ "$(last_log "r['outcome'], r['inject']['cloned'], r['inject']['root'] == '$work/elsewhere', r['inject']['command']")" = "installed 3 True ci" ] \
  && same "$ref_local" "$work/elsewhere" \
  && ok "npm ci --prefix <dir>: the root npm reports is that directory, 3 cloned there, tree identical" || bad "prefix: rc=$rc ($(last_log "r['outcome'], r.get('reason'), (r.get('inject') or {}).get('root')"))"

# 11c. Declines decided on the root npm reports, after npm loaded its config
#      and before its command ran: the project is unchanged, plain npm runs
#      once (its status is the answer), npm's exit handler stays silent, and
#      the run is logged as declined with the reason.
#      plain_status <dir> <env...> -- <npm args>: plain npm's status there.
plain_status() { local d=$1; shift; local e=(); while [ "$1" != -- ]; do e+=("$1"); shift; done; shift
  (cd "$d" && env ${e[@]+"${e[@]}"} npm "$@" > "$work/plain.out" 2>&1); echo $?; }
decline_case() { # name dir want-reason want-status [env...] -- npm args
  local name=$1 d=$2 reason=$3 want=$4; shift 4
  local e=(); while [ "$1" != -- ]; do e+=("$1"); shift; done; shift
  (cd "$d" && env ${e[@]+"${e[@]}"} PATH="$(dirname "$bin"):$PATH" zsh -c ". '$here/shell/matchapack.sh'; npm $*" > "$work/out" 2>&1); local rc=$?
  if [ $rc = "$want" ] && grep -qF "matchapack: running plain npm instead ($reason" "$work/out" && ! grep -q 'Exit handler never called' "$work/out" \
    && [ "$(grep -c 'matchapack: running plain npm instead' "$work/out")" = 1 ] && ! grep -q 'matchapack: added' "$work/out" \
    && [ "$(last_log "r['outcome']")" = declined ] && last_log "r['reason']" | grep -qF "$reason"; then
    ok "$name: declined on npm's own root ($reason), plain npm once, status $rc"
  else
    bad "$name: rc=$rc (plain npm: $want), $(grep '^matchapack' "$work/out" | head -2 | tr '\n' ' ')"
  fi
}
p=$(project global-env small); mkdir -p "$p/node_modules/sentinel"
want=$(plain_status "$p" npm_config_global=true -- ci --no-audit --no-fund)
decline_case "global from the environment" "$p" "npm installs globally" "$want" npm_config_global=true -- ci --no-audit --no-fund
[ -d "$p/node_modules/sentinel" ] && ok "global from the environment: node_modules untouched" || bad "global env: node_modules changed"
p=$(project global-npmrc small); echo 'global=true' > "$p/.npmrc"; mkdir -p "$p/node_modules/sentinel"
want=$(plain_status "$p" -- ci --no-audit --no-fund)
decline_case "global from the project's .npmrc" "$p" "npm installs globally" "$want" -- ci --no-audit --no-fund
(cd "$p" && PATH="$(dirname "$bin"):$PATH" MATCHAPACK_NO_FALLBACK=1 zsh -c ". '$here/shell/matchapack.sh'; npm ci --no-audit --no-fund" > "$work/out" 2>&1); rc=$?
[ $rc = 3 ] && grep -q 'not handled, left unchanged (npm installs globally)' "$work/out" && [ -d "$p/node_modules/sentinel" ] && ! grep -q 'Exit handler never called' "$work/out" \
  && ok "MATCHAPACK_NO_FALLBACK=1 and a decline on npm's root: status 3, node_modules untouched" || bad "no fallback on npm's root: rc=$rc"
p=$(project prefix-out small); mkdir -p "$work/elsewhere-out"; cp "$here/fixtures/bundled/package.json" "$here/fixtures/bundled/package-lock.json" "$work/elsewhere-out/"
decline_case "start directory in scope, --prefix root out of scope" "$p" "outside the trial scope" 0 MATCHAPACK_PATHS="$p" -- ci --prefix "'$work/elsewhere-out'" --no-audit --no-fund
[ -d "$work/elsewhere-out/node_modules/lc-s" ] && same "$ref_local" "$work/elsewhere-out" && ok "root out of scope: plain npm installed there" || bad "root out of scope: plain npm did not install"
p=$(project prefix-missing small)
want=$(plain_status "$p" -- ci --prefix "$work/not-there-plain" --no-audit --no-fund)
decline_case "a --prefix root that does not exist" "$p" "the install root $work/not-there does not exist yet" "$want" -- ci --prefix "'$work/not-there'" --no-audit --no-fund

wrapped "$p" "npm ci --dry-run --no-audit --no-fund"; rc=$?
[ $rc = 0 ] && grep -q 'running plain npm instead (option --dry-run installs nothing' "$work/out" && ! grep -q 'matchapack: added' "$work/out" \
  && ok "npm ci --dry-run: plain npm (not an install)" || bad "dry-run: rc=$rc"

# 11b. A workspace directory that is a symlink to a directory outside the
#      project: npm installs the workspace's own dependency there, as npm ci
#      does; matchapack refuses to clone outside the project (rejected-dest),
#      so that package is npm's own extraction.
p=$work/wslink; rm -rf "${p:?}" "${work:?}/outside-ws"; mkdir -p "$p/packages" "$work/outside-ws"
echo '{"name":"wsroot","version":"1.0.0","private":true,"workspaces":["packages/*"],"dependencies":{"lc-s":"http://127.0.0.1:48321/lc-s-1.0.0.tgz"}}' > "$p/package.json"
echo '{"name":"ws","version":"1.0.0","dependencies":{"lc-s":"http://127.0.0.1:48321/lc-s-2.0.0.tgz"}}' > "$work/outside-ws/package.json"
ln -s "$work/outside-ws" "$p/packages/ws"
(cd "$p" && npm install --package-lock-only --ignore-scripts --no-audit --no-fund > "$work/out" 2>&1)
install "$p"; rc=$?
[ $rc = 0 ] && [ "$(cloned)" = 1 ] && grep -q 'rejected-dest 1' "$work/out" && grep -q 'outside the project' "$work/out" \
  && ! grep -q "went through matchapack's hook" "$work/out" \
  && grep -q '"version":"2.0.0"' "$work/outside-ws/node_modules/lc-s/package.json" && grep -q '"version":"1.0.0"' "$p/node_modules/lc-s/package.json" \
  && ok "a workspace symlinked out of the project: its dependency is refused by the clone server and extracted by npm" || bad "symlinked workspace: rc=$rc, cloned $(cloned)"

# 11b. `npm ci` inside a workspace of a monorepo: npm installs into the
#      root's node_modules (the workspace's subset). npm reports that root,
#      and matchapack clones there (#36; until 2026-11-15 it declined).
ws=$work/ws-src; rm -rf "${ws:?}"; mkdir -p "$ws/packages/a"
printf '{"name":"ws-root","version":"1.0.0","private":true,"workspaces":["packages/*"],"dependencies":{"lc-r":"http://127.0.0.1:48321/lc-r-1.0.0.tgz"}}\n' > "$ws/package.json"
printf '{"name":"a","version":"1.0.0","dependencies":{"lc-s":"http://127.0.0.1:48321/lc-s-1.0.0.tgz"}}\n' > "$ws/packages/a/package.json"
(cd "$ws" && npm install --package-lock-only --no-audit --no-fund > "$work/ref.log" 2>&1) || bad "workspace lockfile"
wscopy() { local d=$work/$1; rm -rf "${d:?}"; cp -R "$ws" "$d"; echo "$d"; }
ref_ws=$(wscopy ws-ref); (cd "$ref_ws/packages/a" && npm ci --no-audit --no-fund > "$work/ref.log" 2>&1) || bad "npm ci in a workspace"
p=$(wscopy ws-member)
wrapped "$p/packages/a" "npm ci --no-audit --no-fund"; rc=$?
[ $rc = 0 ] && grep -q 'matchapack: added' "$work/out" \
  && [ "$(last_log "r['outcome'], r['inject']['cloned'] > 0, r['inject']['root'] == '$p', r['inject']['unseen']")" = "installed True True 0" ] \
  && same "$ref_ws" "$p" \
  && ok "npm ci in a workspace directory: the root is the workspace root, $(last_log "r['inject']['cloned']") cloned there, tree identical" \
  || bad "workspace member: rc=$rc ($(last_log "r['outcome'], r.get('reason'), (r.get('inject') or {}).get('root'), (r.get('inject') or {}).get('fallbacks')"))"

# 12. The report counts the inject runs.
"$bin" report > "$work/out" 2>&1 && grep -Eq 'inject engine: [0-9]+ runs, [0-9]+ packages cloned' "$work/out" && grep -q 'npm failed 1' "$work/out" \
  && ok "report: inject runs, clones and npm failures are counted" || bad "report"
want_tar=$(node -p "require(require.resolve('tar/package.json', { paths: [require('path').dirname(require('path').dirname(require.resolve('pacote', { paths: ['$npm_dir/node_modules/@npmcli/arborist'] })))] })).version")
grep -Eq "inject engine: tar code loaded, by fingerprint: ${want_tar_fp:0:16}… \(tar $want_tar\) [0-9]+ runs" "$work/out" \
  && ok "report: the tar code the runs loaded, by fingerprint (${want_tar_fp:0:16}…, tar $want_tar)" || bad "report: tar fingerprint ($(grep 'tar code' "$work/out"))"
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
  && grep -Eq "inject engine: [0-9]+ store entries compared with npm's extraction, differed and replaced: extractor 2, pacote 1" "$work/out" \
  && grep -q "2 differing entries made by the classic engine's extractor (expected for odd tarballs); first: sha512-aa" "$work/out" \
  && grep -q "1 differing entries made under other extraction code" "$work/out" \
  && ok "report: entries compared with npm's extraction, and the differing ones by kind" || bad "report: store checks ($(grep 'compared with' "$work/out"))"

# 14. npm's own flags on a workspaces project (fixtures/workspaces): the
#     inject engine passes every flag to npm, so workspace selection, omit
#     and the install strategies are live paths. Each: plain npm ci and the
#     inject engine with the same flags on fresh copies, compared with
#     `diff --meta --normalize-roots` (times written during each run may
#     differ; everything else, mtimes outside the runs, uid, gid and xattr
#     names included, must be equal), and the same exit status. The clone
#     count and npm's own extractions are printed with each case.
ns() { python3 -c 'import time;print(time.time_ns())'; }
wsproj() { local d=$work/$1; rm -rf "${d:?}"; cp -R -p "$here/fixtures/workspaces" "$d"; echo "$d"; }
ws_case() { # name flags [VAR=value]  (sets ws_cloned, ws_by_npm)
  local name=$1 flags=$2 envs=${3-} a b ta tb tc wa wb
  a=$(wsproj "ws-$name-npm"); b=$(wsproj "ws-$name-inject")
  ta=$(ns); (cd "$a" && env $envs npm ci --no-audit --no-fund $flags > "$work/ws-ref.out" 2>&1); wa=$?
  tb=$(ns); env $envs "$bin" install "$b" -- --no-audit --no-fund $flags > "$work/out" 2>&1; wb=$?
  tc=$(ns)
  ws_cloned=$(cloned); ws_by_npm=$(by_npm)
  "$bin" diff --meta --normalize-roots --a-run "$ta:$tb" --b-run "$tb:$tc" "$a" "$b" > "$work/meta" 2>&1
  if [ $wa = $wb ] && grep -q '^RESULT: IDENTICAL' "$work/meta"; then
    ok "workspaces, npm ci $flags${envs:+ ($envs)}: identical to plain npm ci with --meta (status $wa; cloned ${ws_cloned:-none}, extracted by npm ${ws_by_npm:-none})"
  else
    bad "workspaces, npm ci $flags${envs:+ ($envs)}: status npm $wa, inject $wb; $(grep -E '^(only|differ|RESULT)|: [1-9]' "$work/meta" | tr '\n' ' ')"
  fi
}
# The root: every package cloned, npm extracting none (the fixture's ten
# package directories: two nested in a workspace, one in a dependency).
ws_case root ""
[ "$ws_cloned" = 10 ] && [ "$ws_by_npm" = 0 ] && ok "workspaces root: all 10 packages cloned, none extracted by npm" || bad "workspaces root: cloned $ws_cloned, by npm $ws_by_npm"
ws_case w-one "-w ws-app"
ws_case w-two "--workspace=ws-app --workspace=ws-lib"
ws_case workspaces "--workspaces"
ws_case workspaces-noroot "--workspaces --include-workspace-root=false"
ws_case omit-dev "--omit=dev"
ws_case node-env-prod "" NODE_ENV=production
ws_case nested "--install-strategy=nested"
ws_case shallow "--install-strategy=shallow"
ws_case linked "--install-strategy=linked"

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

# 15. npm 12: a package patched by `patchedDependencies` (npm patch). npm
#     changes the package's files right after extract returns; the store
#     entry must stay pacote's (unpatched), the project the same as npm ci's
#     (patched), with an empty store and with the filled one.
if [ "$npm_major" -ge 12 ]; then
  pcopy() { local d=$work/$1; rm -rf "${d:?}"; cp -R "$here/fixtures/npm12-patch" "$d"; echo "$d"; }
  ref_patch=$(pcopy patch-ref); (cd "$ref_patch" && npm ci --no-audit --no-fund > "$work/ref.log" 2>&1) || bad "npm ci on fixtures/npm12-patch"
  n_patch=$(python3 -c "import json;print(sum(1 for k,v in json.load(open('$ref_patch/node_modules/.package-lock.json'))['packages'].items() if k))")
  ps=$work/store-patch; rm -rf "${ps:?}"
  for round in empty filled; do
    p=$(pcopy "patch-$round")
    MATCHAPACK_STORE=$ps install "$p" && [ "$(cloned)" = "$n_patch" ] && [ "$(by_npm)" = 0 ] && same "$ref_patch" "$p" \
      && grep -q 'patched by gl-patch-fixture' "$p/node_modules/is-number/index.js" \
      && ! grep -rqs 'patched by gl-patch-fixture' "$ps/v2" \
      && ok "npm $npm_major patchedDependencies, $round store: $n_patch cloned, the patch applied in the project, not in the store, tree identical" \
      || bad "patchedDependencies, $round store: cloned $(cloned) of $n_patch, by npm $(by_npm)"
  done
else
  echo "SKIP patchedDependencies: npm $npm_major has no npm patch (npm 12 does)"
fi

# Every run of the shell function above that read an unexpected key of the
# hook's result was rebuilt by plain npm and so looks fine: it must still
# fail the suite (the deliberate one, 10d, logs elsewhere). `matchapack
# install` stops on it, so those cases fail by themselves.
python3 - "$work/logs/log.jsonl" <<'PY' && ok "no install read an unexpected key of the hook's result" || bad "an install read an unexpected key of the hook's result"
import json, sys
bad = []
for line in open(sys.argv[1]):
    r = json.loads(line)
    if "of what matchapack's hook returned" in str(r.get("reason", "")) or (r.get("inject") or {}).get("result_unexpected"):
        bad.append(r.get("reason") or r["inject"]["result_unexpected"])
print("\n".join(map(str, bad[:3])))
sys.exit(1 if bad else 0)
PY

echo "== $pass passed, $fail failed"
[ "$fail" = 0 ]
