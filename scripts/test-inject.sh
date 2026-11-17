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

# The local fixtures install from this run's own tarball server, on a free
# port; $FX is the copy of fixtures/ whose URLs name it (fixture-server.sh).
. "$here/scripts/fixture-server.sh"; fixtures_serve "$work"
trap '[ -n "$FX_PID" ] && kill "$FX_PID" 2>/dev/null' EXIT
# Its port choice, with fake servers: 48321 itself (the placeholder, inside
# Linux's ephemeral range) and a port without five digits are given back.
python3 "$here/scripts/fixture-server.py" --self-test > "$work/out" 2>&1 \
  && ok "fixture server: a free port that is the placeholder 48321 or not five digits is given back, the next one taken" \
  || bad "fixture server port choice"

project() { # name fixture [python statement on the package.json dict d]
  local d=$work/$1; rm -rf "${d:?}"; mkdir -p "$d"
  cp "$FX/$2/package.json" "$FX/$2/package-lock.json" "$d/"
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
gproject() { local d=$gp/$1; rm -rf "${d:?}"; mkdir -p "$d"; cp "$FX/small/package.json" "$FX/small/package-lock.json" "$d/"; echo "$d"; }
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
    fproj() { local d=$fd/$1; mkdir -p "$d"; cp "$FX/small/package.json" "$FX/small/package-lock.json" "$d/"; echo "$d"; }
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
#     MATCHAPACK_NPM_FINGERPRINT=any). Its code is recorded on every entry
#     but lodash's (as if earlier installs had used it), so those are cloned
#     at once and lodash is compared: the comparison sees files older than
#     the install after clones were placed. Through the shell function the
#     run counts as broken, plain npm ci rebuilds node_modules, and the code
#     goes on the store's decline list. The next install under it leaves
#     every package to npm.
km=$(npm_copy npm-key-mtime)
python3 - "$km/npm/node_modules/pacote/lib/fetcher.js" <<'PY' || bad "key: could not turn pacote's noMtime off"
import sys
p = sys.argv[1]; s = open(p).read()
assert s.count("noMtime: true,") == 1
open(p, "w").write(s.replace("noMtime: true,", "noMtime: false,"))
PY
mt_key=$(node "$here/src/inject.js" --key "$km/npm" | head -1)
ref_mt=$(project ref-mtime small); (cd "$ref_mt" && PATH=$km/bin:$PATH npm ci --no-audit --no-fund > "$work/ref.log" 2>&1) || bad "npm ci with the noMtime-off npm"
for f in "$ks"/made-by/sha512/*/*; do echo "$mt_key" >> "$f"; done
klentry=${lhex:0:2}/${lhex:2}
grep -vxF "$mt_key" "$ks/made-by/sha512/$klentry" > "$work/rec" && cp "$work/rec" "$ks/made-by/sha512/$klentry" || bad "key: lodash's record"
mt_log=$work/logs-mtime; rm -rf "${mt_log:?}"
p=$(project key-mtime small)
(PATH=$km/bin:$PATH; export MATCHAPACK_NPM_FINGERPRINT=any MATCHAPACK_STORE=$ks MATCHAPACK_LOG_DIR=$mt_log MATCHAPACK_INJECT_CHECK=1000000; wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
[ $rc = 0 ] && grep -q "running npm ci to rebuild node_modules" "$work/out" && grep -q "not what matchapack's clones reproduce (.*before this install began): node_modules may not be what npm alone would have made; .*delete $ks/declined/[0-9a-f]* to try it again" "$work/out" \
  && [ "$(last_log_in "$mt_log" "r['outcome']")" = broke ] && [ "$(declined_keys "$ks")" = "$mt_key" ] && same "$ref_mt" "$p" \
  && ok "key: old file times seen after clones were placed: the run is rebuilt by plain npm ci, the code declined in this store, tree identical" \
  || bad "key: old file times: rc=$rc, $(last_log_in "$mt_log" "r['outcome'], r.get('reason')")"
p=$(project key-mtime2 small)
(PATH=$km/bin:$PATH; export MATCHAPACK_NPM_FINGERPRINT=any MATCHAPACK_STORE=$ks MATCHAPACK_LOG_DIR=$mt_log MATCHAPACK_INJECT_CHECK=1000000; wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
[ $rc = 0 ] && grep -q "no clones from the store (this npm's extraction code is on the store's decline list: .*; delete $ks/declined/[0-9a-f]* to try it again)" "$work/out" \
  && [ "$(last_log_in "$mt_log" "r['outcome'], r['inject']['cloned'], r['inject']['fallbacks'].get('key-declined', 0) > 0")" = "installed 0 True" ] && same "$ref_mt" "$p" \
  && ok "key: the next install under the declined code: npm extracts every package, the file to delete named, tree identical" \
  || bad "key: declined code, second install: rc=$rc, $(last_log_in "$mt_log" "r['outcome'], (r.get('inject') or {}).get('cloned')")"
MATCHAPACK_LOG_DIR=$mt_log "$bin" report > "$work/report" 2>&1 && grep -q "in 1 runs pacote's extraction was not what the clones reproduce (1 rebuilt by npm ci, 0 stopped with the repair advice, 0 had cloned nothing" "$work/report" \
  && ok "key: matchapack report counts the rebuilt run" || bad "key: report ($(grep reproduce "$work/report"))"

# 4j. The same npm with an empty store: pacote's extractions would become the
#     entries (fills, nothing to compare with). They are checked the same
#     way first, before anything is cloned: node_modules is npm's own, so
#     the install stands (no second npm ci), says why, and declines the
#     code; no entry is left recorded as this code's. The next install there
#     is npm's own. Removing the key's file clears the decline: the install
#     after that sees the old times again, and declines the code again.
es=$work/store-mtime-empty; rm -rf "${es:?}"
mt_log2=$work/logs-mtime-empty; rm -rf "${mt_log2:?}"
mt_install() { (PATH=$km/bin:$PATH; export MATCHAPACK_NPM_FINGERPRINT=any MATCHAPACK_STORE=$es MATCHAPACK_LOG_DIR=$mt_log2; wrapped "$1" "npm ci --no-audit --no-fund"); }
p=$(project key-mtime-empty small)
mt_install "$p"; rc=$?
[ $rc = 0 ] && ! grep -q "rebuild node_modules" "$work/out" \
  && grep -q "not what matchapack's clones reproduce (.*before this install began); nothing had been cloned, so node_modules is npm's own; .*delete $es/declined/[0-9a-f]* to try it again" "$work/out" \
  && [ "$(last_log_in "$mt_log2" "r['outcome'], r['inject']['cloned'], r['inject']['extraction_unexpected'] is not None")" = "installed 0 True" ] \
  && [ "$(declined_keys "$es")" = "$mt_key" ] && [ -z "$(cat "$es"/made-by/sha512/*/* 2>/dev/null | grep -xF "$mt_key")" ] && same "$ref_mt" "$p" \
  && ok "key: the same with an empty store: the fills are checked first, nothing cloned, so the install stands (no second npm ci), the code declined, nothing recorded under it, tree identical" \
  || bad "key: old file times, empty store: rc=$rc, $(last_log_in "$mt_log2" "r['outcome'], r.get('reason'), (r.get('inject') or {}).get('cloned')")"
p=$(project key-mtime-empty2 small)
mt_install "$p"; rc=$?
[ $rc = 0 ] && grep -q "this npm's extraction code is on the store's decline list" "$work/out" \
  && [ "$(last_log_in "$mt_log2" "r['outcome'], r['inject']['cloned'], r['inject']['extraction_unexpected']")" = "installed 0 None" ] && same "$ref_mt" "$p" \
  && ok "key: the next install there: npm extracts every package, tree identical" || bad "key: empty store, second install: rc=$rc"
dk=$(grep -lxF "$mt_key" "$es"/declined/* 2>/dev/null)
[ -n "$dk" ] && rm -f "$dk" && [ -z "$(declined_keys "$es")" ] || bad "key: clearing a decline (${dk:-no file for the key})"
p=$(project key-mtime-empty3 small)
mt_install "$p"; rc=$?
[ $rc = 0 ] && [ -n "$dk" ] && ! grep -q "on the store's decline list" "$work/out" \
  && [ "$(last_log_in "$mt_log2" "r['outcome'], r['inject']['cloned'], r['inject']['extraction_unexpected'] is not None")" = "installed 0 True" ] \
  && [ "$(declined_keys "$es")" = "$mt_key" ] && same "$ref_mt" "$p" \
  && ok "key: after its file is removed, the code is tried again: the check fires again and declines it again" || bad "key: after clearing: rc=$rc"
# A decline another matchapack build recorded is void: the next install
# tries the code again, the check fires again, and the file is rewritten
# with this build.
dk=$(grep -lxF "$mt_key" "$es"/declined/* 2>/dev/null)
python3 - "$dk" <<'PY' || bad "key: could not make the decline another build's"
import sys
p = sys.argv[1]; lines = open(p).read().split("\n")
assert lines[2].startswith("build: "), lines
lines[2] = "build: 0.0.0 (another-build)"
open(p, "w").write("\n".join(lines))
PY
this_build=$("$bin" --version | sed 's/^matchapack //')
p=$(project key-mtime-empty4 small)
mt_install "$p"; rc=$?
[ $rc = 0 ] && ! grep -q "on the store's decline list" "$work/out" \
  && [ "$(last_log_in "$mt_log2" "r['outcome'], r['inject']['cloned'], r['inject']['extraction_unexpected'] is not None")" = "installed 0 True" ] \
  && [[ "$(sed -n 3p "$dk")" == "build: $this_build"* ]] && same "$ref_mt" "$p" \
  && ok "key: a decline recorded by another build is void: tried again, declined again, now by this build ($this_build)" || bad "key: another build's decline: rc=$rc, $(sed -n 3p "$dk")"
MATCHAPACK_STORE=$es MATCHAPACK_LOG_DIR=$mt_log2 "$bin" report > "$work/report" 2>&1 && grep -q "in 3 runs pacote's extraction was not what the clones reproduce (0 rebuilt by npm ci, 0 stopped with the repair advice, 3 had cloned nothing" "$work/report" \
  && ok "key: matchapack report counts the runs that cloned nothing" || bad "key: report, empty store ($(grep reproduce "$work/report"))"
grep -qF "  $mt_key: declined " "$work/report" && grep -qF "by $this_build" "$work/report" && grep -qF "file $dk" "$work/report" \
  && grep -q "to clear one: delete its file; a new matchapack build tries a declined code again by itself" "$work/report" \
  && ok "key: matchapack report lists the declined code with its build and file, and how to clear it" || bad "key: report, declines ($(grep -A2 'declined extraction code' "$work/report"))"

# 4k. The same extraction check during the install family (the owner's Q5):
#     after clones were placed, `npm install` stops loudly with the repair
#     advice and npm is not run again; the advice comes from npm's own
#     settings (here `save` false from the environment, which the typed
#     words do not show). When nothing had been cloned the install stands,
#     as for npm ci. Both decline the code again (their decline files
#     removed first, as a user would to try it again).
fam_log=$work/logs-mtime-family; rm -rf "${fam_log:?}"
for f in "$ks"/declined/* "$es"/declined/*; do [ -f "$f" ] && rm -f "$f"; done
p=$(project key-mtime-family small)
(PATH=$km/bin:$PATH; export MATCHAPACK_NPM_FINGERPRINT=any MATCHAPACK_STORE=$ks MATCHAPACK_LOG_DIR=$fam_log MATCHAPACK_INJECT_CHECK=1000000 npm_config_save=false; wrapped "$p" "npm install --no-audit --no-fund"); rc=$?
[ $rc = 1 ] && ! grep -q "running npm ci to rebuild" "$work/out" && [ "$(grep -c 'added [0-9]* package' "$work/out")" = 1 ] \
  && grep -q "not what matchapack's clones reproduce (.*before this install began): node_modules may not be what npm alone would have made; .*delete $ks/declined/[0-9a-f]* to try it again" "$work/out" \
  && grep -qF "This run saved neither package.json nor package-lock.json (--no-save), so npm ci would rebuild without what it added" "$work/out" \
  && grep -qF "MATCHAPACK=0 npm install --no-audit --no-fund" "$work/out" && grep -qF "; node_modules may not be what npm alone would have written. " "$work/out" \
  && [ "$(last_log_in "$fam_log" "r['outcome'], r['repair'], r['command'], r['inject']['command'], r['inject']['save']")" = "broke stopped install install False" ] \
  && [ "$(declined_keys "$ks")" = "$mt_key" ] \
  && ok "key, npm install: old file times seen after clones were placed: stopped loudly (status 1), npm not run again, the advice from npm's settings (save=false from the environment), logged, the code declined" \
  || bad "key, npm install after clones: rc=$rc, $(last_log_in "$fam_log" "r['outcome'], r.get('repair'), r.get('reason')" | cut -c1-200)"
p=$(project key-mtime-family-empty small)
(PATH=$km/bin:$PATH; export MATCHAPACK_NPM_FINGERPRINT=any MATCHAPACK_STORE=$es MATCHAPACK_LOG_DIR=$fam_log; wrapped "$p" "npm install --no-audit --no-fund"); rc=$?
[ $rc = 0 ] && ! grep -q 'rebuild\|git diff' "$work/out" \
  && grep -q "not what matchapack's clones reproduce (.*before this install began); nothing had been cloned, so node_modules is npm's own" "$work/out" \
  && [ "$(last_log_in "$fam_log" "r['outcome'], r['command'], r['inject']['cloned'], r['inject']['extraction_unexpected'] is not None")" = "installed install 0 True" ] \
  && [ "$(declined_keys "$es")" = "$mt_key" ] && [ -d "$p/node_modules/lodash" ] \
  && ok "key, npm install with nothing cloned: the install stands (status 0, no advice), said, the code declined" \
  || bad "key, npm install, nothing cloned: rc=$rc, $(last_log_in "$fam_log" "r['outcome'], r.get('reason')" | cut -c1-200)"
MATCHAPACK_LOG_DIR=$fam_log "$bin" report > "$work/report" 2>&1 && grep -q "in 2 runs pacote's extraction was not what the clones reproduce (0 rebuilt by npm ci, 1 stopped with the repair advice, 1 had cloned nothing" "$work/report" \
  && ok "key: matchapack report counts the stopped run apart" || bad "key: report, family ($(grep reproduce "$work/report"))"

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
mkdir -p "$work/elsewhere"; cp "$FX/bundled/package.json" "$FX/bundled/package-lock.json" "$work/elsewhere/"
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
p=$(project prefix-out small); mkdir -p "$work/elsewhere-out"; cp "$FX/bundled/package.json" "$FX/bundled/package-lock.json" "$work/elsewhere-out/"
decline_case "start directory in scope, --prefix root out of scope" "$p" "outside the trial scope" 0 MATCHAPACK_PATHS="$p" -- ci --prefix "'$work/elsewhere-out'" --no-audit --no-fund
[ -d "$work/elsewhere-out/node_modules/lc-s" ] && same "$ref_local" "$work/elsewhere-out" && ok "root out of scope: plain npm installed there" || bad "root out of scope: plain npm did not install"
p=$(project prefix-missing small)
want=$(plain_status "$p" -- ci --prefix "$work/not-there-plain" --no-audit --no-fund)
decline_case "a --prefix root that does not exist" "$p" "the install root $work/not-there does not exist yet" "$want" -- ci --prefix "'$work/not-there'" --no-audit --no-fund

wrapped "$p" "npm ci --dry-run --no-audit --no-fund"; rc=$?
[ $rc = 0 ] && grep -q 'running plain npm instead (option --dry-run installs nothing' "$work/out" && ! grep -q 'matchapack: added' "$work/out" \
  && ok "npm ci --dry-run: plain npm (not an install)" || bad "dry-run: rc=$rc"

# 11d. The install family logs three kinds of decline without a line (the
#      owner's decision, 2026-11-15): a global install, a project outside
#      the trial scope, a run that installs nothing. Each is decided both
#      before node starts (a typed flag, the start directory) and on npm's
#      own root (an npmrc or environment setting, a --prefix root). Plain
#      npm runs once, with its status; the run is logged as declined with
#      the reason. npm ci with the same reasons prints the line (11c above,
#      and the three cases after these), and so does the family for any
#      other reason. Global installs go to a scratch prefix; every package
#      comes from the local tarball server.
gp=$work/global-prefix; rm -rf "${gp:?}"; mkdir -p "$gp"
tgz=$FX_URL/lc-s-1.0.0.tgz
quiet_case() { # name dir want-reason [env...] -- npm args
  local name=$1 d=$2 reason=$3; shift 3
  local e=(); while [ "$1" != -- ]; do e+=("$1"); shift; done; shift
  (cd "$d" && env ${e[@]+"${e[@]}"} PATH="$(dirname "$bin"):$PATH" zsh -c ". '$here/shell/matchapack.sh'; npm $*" > "$work/out" 2>&1); local rc=$?
  if [ $rc = 0 ] && ! grep -q '^matchapack' "$work/out" && ! grep -q 'Exit handler never called' "$work/out" \
    && [ "$(last_log "r['outcome']")" = declined ] && last_log "r['reason']" | grep -qF "$reason"; then
    ok "$name: no line, plain npm, logged as declined ($reason)"
  else
    bad "$name: rc=$rc, logged $(last_log "r['outcome'], r.get('reason')" 2>&1 | cut -c1-120)"
  fi
}
p=$(project fam-quiet-g bundled)
quiet_case "npm install -g <tarball>" "$p" "option -g makes npm install globally" npm_config_prefix="$gp" -- install -g "$tgz" --no-audit --no-fund
[ -f "$gp/lib/node_modules/lc-s/package.json" ] && [ ! -e "$p/node_modules" ] && ok "npm install -g: plain npm installed into the (scratch) global prefix" || bad "npm install -g: not installed globally"
p=$(project fam-quiet-npmrc bundled); echo 'global=true' > "$p/.npmrc"
quiet_case "npm install with global=true in the project's .npmrc" "$p" "npm installs globally" npm_config_prefix="$gp" -- install "$tgz" --no-audit --no-fund
p=$(project fam-quiet-dry bundled)
quiet_case "npm update --dry-run" "$p" "option --dry-run installs nothing" -- update --dry-run --no-audit --no-fund
quiet_case "npm uninstall with npm_config_dry_run=true" "$p" "this run installs nothing (--dry-run)" npm_config_dry_run=true -- uninstall lc-s --no-audit --no-fund
[ ! -e "$p/node_modules" ] && cmp -s "$p/package.json" "$FX/bundled/package.json" && ok "dry runs: nothing changed" || bad "dry runs changed the project"
p=$(project fam-quiet-scope bundled); mkdir -p "$work/scope-elsewhere"
quiet_case "npm install outside the trial scope" "$p" "outside the trial scope" MATCHAPACK_PATHS="$work/scope-elsewhere" -- install --no-audit --no-fund
[ -f "$p/node_modules/lc-s/package.json" ] && ok "outside the trial scope: plain npm installed" || bad "outside the trial scope: not installed"
p=$(project fam-quiet-prefix bundled); q=$(project fam-quiet-elsewhere bundled)
quiet_case "npm install --prefix <root outside the trial scope>" "$p" "outside the trial scope" MATCHAPACK_PATHS="$p" -- install --prefix "'$q'" --no-audit --no-fund
[ -f "$q/node_modules/lc-s/package.json" ] && [ ! -e "$p/node_modules" ] && ok "--prefix root out of scope: plain npm installed there" || bad "--prefix root out of scope: not installed there"
# npm ci with the reasons 11c does not cover: the line is printed.
p=$(project ci-quiet-g bundled)
want=$(plain_status "$p" npm_config_prefix="$gp" -- ci -g --no-audit --no-fund)
decline_case "npm ci -g" "$p" "option -g makes npm install globally" "$want" npm_config_prefix="$gp" -- ci -g --no-audit --no-fund
p=$(project ci-quiet-dry bundled)
decline_case "npm ci with npm_config_dry_run=true" "$p" "this run installs nothing (--dry-run)" 0 npm_config_dry_run=true -- ci --no-audit --no-fund
p=$(project ci-quiet-scope bundled)
decline_case "npm ci outside the trial scope" "$p" "outside the trial scope" 0 MATCHAPACK_PATHS="$work/scope-elsewhere" -- ci --no-audit --no-fund
# A scope that can never match is not the silent case: one whose entries
# all can never match (under a directory that does not exist; a paths file
# with comments only), or one entry that can never match (`$HOME/x` typed
# literally), has its own reason, printed for the family. A prefix that is
# no directory itself, or a directory removed since, is a good entry: a
# project outside it declines in silence, and one under it is in scope.
p=$(project fam-scope-none bundled)
decline_case "npm install, the scope has no entry that can match" "$p" "outside the trial scope: MATCHAPACK_PATHS lists no entry that can match" 0 MATCHAPACK_PATHS="$work/no-parent/x" -- install --no-audit --no-fund
# A relative entry never matches, also `.` (resolved against the current
# directory it would cover every project; `../fam-scope-rel` would cover
# that project, started in it): from the environment and from a paths file.
p=$(project fam-scope-dot bundled)
decline_case "npm install, MATCHAPACK_PATHS=." "$p" "outside the trial scope: MATCHAPACK_PATHS lists no entry that can match" 0 MATCHAPACK_PATHS=. -- install --no-audit --no-fund
mkdir -p "$work/scope-cfg-rel"; printf '../fam-scope-rel\n' > "$work/scope-cfg-rel/paths"
p=$(project fam-scope-rel bundled)
decline_case "npm install, a relative entry in the paths file" "$p" "outside the trial scope: ~/.config/matchapack/paths lists no entry that can match" 0 -u MATCHAPACK_PATHS MATCHAPACK_CONFIG_DIR="$work/scope-cfg-rel" -- install --no-audit --no-fund
p=$(project fam-scope-stale bundled)
quiet_case "npm install, one good entry and one removed since" "$p" "outside the trial scope (" MATCHAPACK_PATHS="$work/scope-elsewhere:$work/removed-long-ago" -- install --no-audit --no-fund
p=$(project fam-scope-prefix bundled)
quiet_case "npm install, a prefix that is no directory itself, another project" "$p" "outside the trial scope (" MATCHAPACK_PATHS="$work/scope-pre" -- install --no-audit --no-fund
p=$(project scope-prefix-covered bundled)
wrapped "$p" "export MATCHAPACK_PATHS='$work/scope-pre'; npm install --no-audit --no-fund"; rc=$?
[ $rc = 0 ] && ! grep -q 'running plain npm' "$work/out" && [ "$(last_log "r['outcome']")" = installed ] \
  && ok "a prefix that is no directory itself covers a project whose path starts with it (scope-prefix-covered under scope-pre…)" || bad "scope prefix: rc=$rc, $(grep '^matchapack' "$work/out" | head -1)"
p=$(project fam-scope-tmp bundled)
tmpw=$(echo "$work" | sed -n 's#^/private/tmp/#/tmp/#p')
if [ -n "$tmpw" ]; then
  wrapped "$p" "export MATCHAPACK_PATHS='$tmpw/fam-scope-t'; npm install --no-audit --no-fund"; rc=$?
  [ $rc = 0 ] && ! grep -q 'running plain npm' "$work/out" && [ "$(last_log "r['outcome']")" = installed ] \
    && ok "a prefix written with /tmp matches the project under /private/tmp (the parent's symlinks resolved)" || bad "scope /tmp prefix: rc=$rc, $(grep '^matchapack' "$work/out" | head -1)"
fi
mkdir -p "$work/scope-cfg"; printf '# nothing yet\n\n' > "$work/scope-cfg/paths"
p=$(project fam-scope-comments bundled)
decline_case "npm install, the paths file has comments only" "$p" "outside the trial scope: ~/.config/matchapack/paths lists no entry that can match" 0 -u MATCHAPACK_PATHS MATCHAPACK_CONFIG_DIR="$work/scope-cfg" -- install --no-audit --no-fund
p=$(project fam-scope-var bundled)
decline_case "npm install, a scope entry that is not an absolute directory" "$p" "outside the trial scope: MATCHAPACK_PATHS lists \"\$HOME/x\", which can never match (not an absolute path)" 0 "MATCHAPACK_PATHS=$work/scope-elsewhere:\$HOME/x" -- install --no-audit --no-fund
# The family with another reason: the line is printed.
p=$(project fam-loud bundled); rm -rf "${work:?}/fam-loud-root" "${work:?}/fam-loud-plain"
want=$(plain_status "$p" -- install --prefix "$work/fam-loud-plain" --no-audit --no-fund)
decline_case "npm install with a --prefix root that does not exist" "$p" "the install root $work/fam-loud-root does not exist yet" "$want" -- install --prefix "'$work/fam-loud-root'" --no-audit --no-fund

# 11b. A workspace directory that is a symlink to a directory outside the
#      project: npm installs the workspace's own dependency there, as npm ci
#      does; matchapack refuses to clone outside the project (rejected-dest),
#      so that package is npm's own extraction.
p=$work/wslink; rm -rf "${p:?}" "${work:?}/outside-ws"; mkdir -p "$p/packages" "$work/outside-ws"
echo '{"name":"wsroot","version":"1.0.0","private":true,"workspaces":["packages/*"],"dependencies":{"lc-s":"'"$FX_URL"'/lc-s-1.0.0.tgz"}}' > "$p/package.json"
echo '{"name":"ws","version":"1.0.0","dependencies":{"lc-s":"'"$FX_URL"'/lc-s-2.0.0.tgz"}}' > "$work/outside-ws/package.json"
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
printf '{"name":"ws-root","version":"1.0.0","private":true,"workspaces":["packages/*"],"dependencies":{"lc-r":"'"$FX_URL"'/lc-r-1.0.0.tgz"}}\n' > "$ws/package.json"
printf '{"name":"a","version":"1.0.0","dependencies":{"lc-s":"'"$FX_URL"'/lc-s-1.0.0.tgz"}}\n' > "$ws/packages/a/package.json"
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
wsproj() { local d=$work/$1; rm -rf "${d:?}"; cp -R -p "$FX/workspaces" "$d"; echo "$d"; }
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
  pcopy() { local d=$work/$1; rm -rf "${d:?}"; cp -R "$FX/npm12-patch" "$d"; echo "$d"; }
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

# 16. The install family (npm install / update / uninstall) through the
#     shell function: npm's own command with the hook, and the bookkeeping
#     of an incremental install (NOTES "The install family"). The trees
#     themselves are compared with plain npm by scripts/verify-steps.sh.
rec_of() { python3 -c "
import glob, json, sys
for f in glob.glob(sys.argv[1] + '/projects/*.json'):
    r = json.load(open(f))
    if r.get('path') == sys.argv[2]:
        print(len(r['entries'])); break
else:
    print('-')" "$store" "$1"; }
p=$(project fam small)
wrapped "$p" "npm ci --no-audit --no-fund" || bad "family: npm ci"
n_rec=$(rec_of "$p")
# Nothing to do: no matchapack line at all (Q6), unseen 0, record unchanged.
wrapped "$p" "npm install --no-audit --no-fund"; rc=$?
[ $rc = 0 ] && ! grep -q '^matchapack' "$work/out" && [ "$n_rec" -gt 0 ] && [ "$(rec_of "$p")" = "$n_rec" ] \
  && [ "$(last_log "r['outcome'], r['command'], r['root'] == '$p', r['inject']['cloned'], r['inject']['installed'], r['inject']['unseen']")" = "installed install True 0 0 0" ] \
  && ok "npm install with nothing to do: silent, unseen 0 (no false alarm), the gc record keeps its $n_rec entries" \
  || bad "family noop: rc=$rc, record $(rec_of "$p") (was $n_rec), $(last_log "r['outcome'], r.get('command'), (r.get('inject') or {}).get('unseen')")"
# One package added: one line, cloned, the record grows by one, gc keeps it.
wrapped "$p" "npm install left-pad@1.3.0 --no-audit --no-fund"; rc=$?
[ $rc = 0 ] && grep -qx 'matchapack: 1 package cloned from the store, npm extracted 0 (inject engine; MATCHAPACK=0 npm install for plain npm)' "$work/out" \
  && [ "$(grep -c '^matchapack' "$work/out")" = 1 ] && [ "$(rec_of "$p")" = $((n_rec + 1)) ] && grep -q '"left-pad": "^1.3.0"' "$p/package.json" \
  && [ "$(last_log "r['outcome'], r['inject']['cloned'], r['inject']['installed'], r['inject']['unseen']")" = "installed 1 1 0" ] \
  && find "$store/v2" -mindepth 3 -maxdepth 3 -type d -exec touch -t 202001010000 {} + \
  && "$bin" store gc --dry-run > "$work/gc" 2>&1 && grep -q 'would remove 0 of' "$work/gc" \
  && ok "npm install left-pad@1.3.0: 1 cloned, said in one line, the record grows to $((n_rec + 1)) entries, gc would remove nothing" \
  || bad "family add: rc=$rc, record $(rec_of "$p"), $(grep '^matchapack' "$work/out" | head -2), $(grep 'would remove' "$work/gc")"
# Removed again: nothing placed (silent), the record drops the entry no
# package has any more.
wrapped "$p" "npm uninstall left-pad --no-audit --no-fund"; rc=$?
[ $rc = 0 ] && ! grep -q '^matchapack' "$work/out" && [ "$(rec_of "$p")" = "$n_rec" ] && [ ! -e "$p/node_modules/left-pad" ] \
  && [ "$(last_log "r['outcome'], r['command'], r['inject']['unseen']")" = "installed uninstall 0" ] \
  && ok "npm uninstall left-pad: silent, the record is back to the $n_rec entries of npm ci" || bad "family uninstall: rc=$rc, record $(rec_of "$p")"
# A project npm installed by itself (no gc record yet): after one routed
# npm install its record holds every entry of its hidden lockfile (what gc's
# fallback through the trial log keeps for it), not only this run's.
p=$(project fam-plain small)
(cd "$p" && npm ci --no-audit --no-fund > "$work/plain.out" 2>&1) || bad "family plain: npm ci"
[ "$(rec_of "$p")" = - ] || bad "family plain: a record before matchapack ran"
wrapped "$p" "npm install left-pad@1.3.0 --no-audit --no-fund"; rc=$?
[ $rc = 0 ] && [ "$(rec_of "$p")" = $((d_small + 1)) ] \
  && ok "a project without a gc record: after npm install left-pad@1.3.0 its record holds all $((d_small + 1)) entries of its tree" \
  || bad "family plain: rc=$rc, record $(rec_of "$p") (want $((d_small + 1)))"
# A record behind the tree (matchapack stopped before it wrote one): the next
# routed command, even one that places nothing, completes it from the
# hidden lockfile.
python3 - "$store" "$p" <<'PY' || bad "family behind: could not truncate the record"
import glob, json, sys
for f in glob.glob(sys.argv[1] + "/projects/*.json"):
    r = json.load(open(f))
    if r.get("path") == sys.argv[2]:
        r["entries"] = r["entries"][:1]
        json.dump(r, open(f, "w"))
PY
[ "$(rec_of "$p")" = 1 ] || bad "family behind: record not truncated ($(rec_of "$p"))"
wrapped "$p" "npm install --no-audit --no-fund"; rc=$?
[ $rc = 0 ] && [ "$(rec_of "$p")" = $((d_small + 1)) ] \
  && ok "a gc record behind the tree (1 entry) is completed by a no-op npm install: $((d_small + 1)) entries" \
  || bad "family behind: rc=$rc, record $(rec_of "$p") (want $((d_small + 1)))"
# No package-lock.json (package-lock=false), and node_modules/.cache made
# afterwards (as a tool would), so npm stops trusting the hidden lockfile:
# the packages npm keeps then have neither an integrity nor a resolved in
# it. The record must not drop their entries.
p=$work/fam-nolock-cache; rm -rf "${p:?}"; mkdir -p "$p"
echo '{"name":"nolock-cache","version":"1.0.0","dependencies":{"chalk":"2.4.2","ms":"2.1.3","is-number":"7.0.0"}}' > "$p/package.json"
echo 'package-lock=false' > "$p/.npmrc"
wrapped "$p" "npm install --no-audit --no-fund" || bad "family nolock-cache: first install"
sleep 1; mkdir "$p/node_modules/.cache"
r1=$(rec_of "$p")
wrapped "$p" "npm install left-pad@1.3.0 --no-audit --no-fund"; rc=$?
[ $rc = 0 ] && [ "$r1" -ge 9 ] && [ "$(rec_of "$p")" = $((r1 + 1)) ] \
  && ok "no lockfile and an untrusted hidden lockfile: the record keeps its $r1 entries and adds left-pad ($(rec_of "$p"))" \
  || bad "family nolock-cache: rc=$rc, record $r1 -> $(rec_of "$p")"
# npm ci twice in a workspaces project whose member has its own
# node_modules (npm before 10.8.1 removes only the root's in npm ci, and
# keeps the member's packages without extracting them): the record keeps
# the entries of every installed package.
p=$work/ws-ci-twice; rm -rf "${p:?}"; cp -R "$FX/workspaces" "$p"
wrapped "$p" "npm ci --no-audit --no-fund" || bad "ws ci twice: first"
wrapped "$p" "npm ci --no-audit --no-fund"; rc=$?
want=$(python3 -c "
import json,sys
print(len({v['integrity'] for k,v in json.load(open(sys.argv[1]+'/node_modules/.package-lock.json'))['packages'].items() if v.get('integrity')}))" "$p")
[ $rc = 0 ] && [ "$(rec_of "$p")" = "$want" ] \
  && ok "npm ci twice in a workspaces project: the record holds every installed package's entry ($want)" \
  || bad "ws ci twice: rc=$rc, record $(rec_of "$p"), want $want"
# The hook not installed (what an npm that no longer calls the method looks
# like): the package npm added is counted as unseen, and said.
p=$(project fam-nohook small)
wrapped "$p" "npm ci --no-audit --no-fund" || bad "family: npm ci (no hook)"
MATCHAPACK_INJECT_HOOK=none wrapped "$p" "npm install left-pad@1.3.0 --no-audit --no-fund"; rc=$?
[ $rc = 0 ] && grep -q "matchapack: npm installed 1 packages in this run, but 1 of them did not go through matchapack's hook" "$work/out" \
  && [ "$(last_log "r['outcome'], r['inject']['cloned'], r['inject']['installed'], r['inject']['unseen']")" = "installed 0 1 1" ] \
  && ok "the hook not installed: npm install left-pad@1.3.0 reports the 1 package the hook never saw" || bad "family no hook: rc=$rc, $(grep '^matchapack' "$work/out" | head -2)"
# npm's own failure: its status once, logged, not retried.
(cd "$p" && npm install does-not-exist-matchapack-xyz --no-audit --no-fund > "$work/plain.out" 2>&1); want=$?
wrapped "$p" "npm install does-not-exist-matchapack-xyz --no-audit --no-fund"; rc=$?
[ $want != 0 ] && [ $rc = $want ] && [ "$(grep -Ec 'npm (error|ERR!) code E404' "$work/out")" = 1 ] && ! grep -q '^matchapack' "$work/out" \
  && [ "$(last_log "r['outcome'], r['command'], r['code']")" = "npm_failed install $want" ] \
  && ok "npm install of a package that does not exist: npm's error and status ($rc), once, logged as npm_failed" || bad "family npm failure: rc=$rc, plain npm $want"
# A decline on npm's root with MATCHAPACK_NO_FALLBACK=1: status 3.
p=$(project fam-global small); echo 'global=true' > "$p/.npmrc"
(cd "$p" && PATH="$(dirname "$bin"):$PATH" MATCHAPACK_NO_FALLBACK=1 zsh -c ". '$here/shell/matchapack.sh'; npm install left-pad@1.3.0" > "$work/out" 2>&1); rc=$?
[ $rc = 3 ] && grep -q 'not handled, left unchanged (npm installs globally)' "$work/out" && ! grep -q 'Exit handler never called' "$work/out" && [ ! -e "$p/node_modules" ] \
  && ok "npm install with global=true in the project's .npmrc, MATCHAPACK_NO_FALLBACK=1: declined on npm's root, status 3, nothing installed" || bad "family no fallback: rc=$rc"
# The result trap during the install family (the doctored npm of 10d): no
# automatic repair (Q5). The run stops loudly with npm's files left as npm
# wrote them, the key named and the repair command given; npm is not run
# again; logged as broke (stopped), counted by `matchapack report`.
p=$(project fam-trap small)
(PATH=$trc/bin:$PATH; MATCHAPACK_LOG_DIR=$tr_log wrapped "$p" "npm ci --no-audit --no-fund") || bad "family trap: npm ci"
(PATH=$trc/bin:$PATH; MATCHAPACK_LOG_DIR=$tr_log wrapped "$p" "npm install left-pad@1.3.0 --no-audit --no-fund"); rc=$?
[ $rc = 1 ] && grep -q "matchapack: npm read \`files\` (1×) of what matchapack's hook returned" "$work/out" \
  && grep -qF 'Check `git diff` of package.json and package-lock.json, then rebuild with plain npm:' "$work/out" \
  && ! grep -q '^  rm -rf' "$work/out" && grep -qxF '  MATCHAPACK=0 npm ci left-pad@1.3.0 --no-audit --no-fund' "$work/out" \
  && ! grep -q 'running npm ci to rebuild' "$work/out" && [ "$(grep -c 'added [0-9]* package' "$work/out")" = 1 ] \
  && [ "$(last_log_in "$tr_log" "r['outcome'], r['repair'], r['command'], '\`files\`' in r['reason']")" = "broke stopped install True" ] \
  && ok "trap during npm install: stopped loudly (status 1), the key and the repair command said, npm not run again, logged" \
  || bad "family trap: rc=$rc ($(grep '^matchapack' "$work/out" | head -2))"
# With --no-package-lock npm writes no lockfile in the run, so npm ci has
# nothing current to rebuild from: the repair is the same command with
# plain npm on an emptied node_modules.
p=$(project fam-trap-nolock small)
(PATH=$trc/bin:$PATH; MATCHAPACK_LOG_DIR=$work/logs-trap-nolock wrapped "$p" "npm ci --no-audit --no-fund") || bad "family trap, no lockfile: npm ci"
(PATH=$trc/bin:$PATH; MATCHAPACK_LOG_DIR=$work/logs-trap-nolock wrapped "$p" "npm install --no-package-lock left-pad@1.3.0 --no-audit --no-fund"); rc=$?
[ $rc = 1 ] && grep -qF "There is no package-lock.json from this run for npm ci to rebuild from: check \`git diff\` of package.json, then remove node_modules and run the command again with plain npm:" "$work/out" \
  && grep -qxF "  rm -rf $p/node_modules" "$work/out" && grep -qxF "  MATCHAPACK=0 npm install --no-package-lock left-pad@1.3.0 --no-audit --no-fund" "$work/out" \
  && ok "trap during npm install --no-package-lock: the repair is the same command with plain npm on an emptied node_modules, not npm ci" \
  || bad "family trap, no lockfile: rc=$rc ($(grep '^matchapack' "$work/out" | head -2))"
# --no-save: the lockfile does not have what the run added, so npm ci is
# not the repair either.
(PATH=$trc/bin:$PATH; MATCHAPACK_LOG_DIR=$work/logs-trap-nolock wrapped "$p" "npm install --no-save semver@7.6.0 --no-audit --no-fund"); rc=$?
[ $rc = 1 ] && grep -qF "This run saved neither package.json nor package-lock.json (--no-save), so npm ci would rebuild without what it added: check \`git diff\` of package.json, then remove node_modules and run the command again with plain npm:" "$work/out" \
  && grep -qxF "  MATCHAPACK=0 npm install --no-save semver@7.6.0 --no-audit --no-fund" "$work/out" \
  && ok "trap during npm install --no-save: the repair is the same command with plain npm, not npm ci" \
  || bad "family trap, --no-save: rc=$rc ($(grep '^matchapack' "$work/out" | head -2))"
# A command filtered to one workspace (started inside a member): npm ci
# with the filter would remove the other workspaces' packages; the repair
# rebuilds the whole project from its root.
p=$work/fam-trap-ws; rm -rf "${p:?}"; cp -R "$FX/workspaces" "$p"
(PATH=$trc/bin:$PATH; MATCHAPACK_LOG_DIR=$work/logs-trap-nolock wrapped "$p" "npm ci --no-audit --no-fund") || bad "family trap, workspace: npm ci"
(PATH=$trc/bin:$PATH; MATCHAPACK_LOG_DIR=$work/logs-trap-nolock wrapped "$p/packages/app" "npm install left-pad@1.3.0 --no-audit --no-fund"); rc=$?
[ $rc = 1 ] && grep -qF "The command was filtered to some workspaces, and npm ci with the filter removes the packages of the others: check \`git diff\` of package.json and package-lock.json, then rebuild the whole project with plain npm." "$work/out" \
  && ! grep -qxF "  rm -rf $p/node_modules" "$work/out" && grep -qxF "  MATCHAPACK=0 npm ci --prefix $p --audit=false --fund=false" "$work/out" \
  && ! grep -q 'npmrc sets a workspace filter' "$work/out" \
  && ok "trap during npm install inside a workspace member: the repair is npm ci of the whole project, from its root" \
  || bad "family trap, workspace member: rc=$rc ($(grep '^matchapack' "$work/out" | head -2))"
# A broken run (here a lost child status, MATCHAPACK_TEST_BROKE) carries
# npm's settings too: a command filtered by an exported
# npm_config_workspace gets the whole-project rebuild with that variable
# unset; npm is not run again. npm ci is rebuilt with plain npm ci.
(export npm_config_workspace=ws-app MATCHAPACK_TEST_BROKE=1; MATCHAPACK_LOG_DIR=$work/logs-broke wrapped "$p" "npm install left-pad@1.3.0 --no-audit --no-fund"); rc=$?
[ $rc = 1 ] && grep -qF "matchapack: waiting for npm: a lost child status (MATCHAPACK_TEST_BROKE); npm may not have finished as it would have alone. The command was filtered to some workspaces" "$work/out" \
  && grep -qxF "  env -u npm_config_workspace MATCHAPACK=0 npm ci --prefix $p --audit=false --fund=false" "$work/out" && [ "$(grep -c 'added [0-9]* package\|up to date' "$work/out")" = 1 ] \
  && [ "$(last_log_in "$work/logs-broke" "r['outcome'], r['repair'], r['root'] == '$p', r['inject']['workspace_filter']")" = "broke stopped True True" ] \
  && ok "a broken npm install filtered by npm_config_workspace: stopped, the whole-project rebuild with that variable unset, npm not run again" \
  || bad "broke, family: rc=$rc ($(grep '^matchapack' "$work/out" | head -2))"
# A panic (MATCHAPACK_TEST_BROKE=panic) loses the run's stats; npm's root
# report is kept apart, so the advice still fits: inside a member, with
# --no-save, the whole project is reinstalled from npm's root (not the
# start directory) and then the command runs again.
(export MATCHAPACK_TEST_BROKE=panic; MATCHAPACK_LOG_DIR=$work/logs-broke wrapped "$p/packages/app" "npm install --no-save left-pad@1.3.0 --no-audit --no-fund"); rc=$?
[ $rc = 1 ] && grep -qF "matchapack: internal error (panic); npm may not have finished as it would have alone. This run saved neither package.json nor package-lock.json (--no-save), so npm ci would rebuild without what it added, and the command was filtered to some workspaces: check \`git diff\` of package.json, then remove node_modules, reinstall the whole project with plain npm, and run the command again." "$work/out" \
  && grep -qxF "  rm -rf $p/node_modules" "$work/out" && grep -qxF "  MATCHAPACK=0 npm install --prefix $p --save=false --audit=false --fund=false" "$work/out" \
  && grep -qxF "  MATCHAPACK=0 npm install --no-save left-pad@1.3.0 --no-audit --no-fund" "$work/out" \
  && [ "$(last_log_in "$work/logs-broke" "r['outcome'], r['repair'], r['root'] == '$p'")" = "broke stopped True" ] \
  && ! grep -q 'npmrc sets a workspace filter' "$work/out" \
  && ok "a panic during npm install --no-save inside a member: npm's settings kept, the whole project reinstalled from its root, then the command" \
  || bad "broke, panic: rc=$rc ($(grep '^matchapack' "$work/out" | head -2))"
# Options npm's own parser does not read back the same (a list option
# given `false`): the whole-project command has none of them, and the
# advice says so first, naming the ones that change what an install does;
# logged as `checked: false`. Started through a symlink to the project: the
# printed root is the canonical one.
ln -s "$p" "$work/fam-trap-ws-link" 2>/dev/null
(export MATCHAPACK_TEST_BROKE=1; MATCHAPACK_LOG_DIR=$work/logs-broke wrapped "$work/fam-trap-ws-link" "npm install --include false left-pad@1.3.0 -w ws-app --ignore-scripts --no-audit --no-fund"); rc=$?
[ $rc = 1 ] && grep -qF "rebuild the whole project with plain npm. matchapack could not repeat your options safely, so the whole-project command below has none of them: add the ones you need (you used --ignore-scripts, --audit=false, --fund=false: add them)." "$work/out" \
  && grep -qxF "  MATCHAPACK=0 npm ci --prefix $p" "$work/out" \
  && [ "$(last_log_in "$work/logs-broke" "r['inject']['repair']['checked'], r['inject']['repair']['unsure']")" = "False True" ] \
  && ok "options npm's parser does not read back the same: the whole-project command without them, said, logged; the root canonical through a symlink" \
  || bad "broke, unsure: rc=$rc ($(grep '^matchapack' "$work/out" | head -2))"
(export MATCHAPACK_TEST_BROKE=1; MATCHAPACK_LOG_DIR=$work/logs-broke wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
[ $rc = 0 ] && grep -q 'running npm ci to rebuild node_modules' "$work/out" && [ "$(last_log_in "$work/logs-broke" "r['outcome'], r['repair']")" = "broke npm ci" ] \
  && ok "a broken npm ci: rebuilt with plain npm ci, as before" || bad "broke, ci: rc=$rc ($(grep '^matchapack' "$work/out" | head -2))"
# Following the advice rebuilds what matchapack placed outside the root's
# node_modules, a workspace member's own packages (npm ci before 10.8.1
# keeps what it finds there, and an install keeps it too): packages/app's
# node_modules is removed before the broken run, so the run places y1-b
# there; a marker then put into it must be gone after running exactly the
# commands the advice printed (the lines two spaces in), with plain npm.
# The forms: the default (npm ci), filtered (the whole project), --no-save
# (the command again).
# follow_advice <dir>: run those lines of $work/out in <dir>.
follow_advice() {
  awk '/^matchapack: .*git diff/ {on = 1; next} on && /^  / {print substr($0, 3); next} on {exit}' "$work/out" > "$work/advice.sh"
  [ -s "$work/advice.sh" ] && (cd "$1" && sh -e "$work/advice.sh" > "$work/follow.out" 2>&1)
}
p=$work/fam-follow; rm -rf "${p:?}"; cp -R "$FX/workspaces" "$p"
wrapped "$p" "npm ci --no-audit --no-fund" || bad "follow the advice: npm ci"
mk=$p/packages/app/node_modules/y1-b/MARKER
for c in "npm install --no-audit --no-fund" "npm install -w ws-app --no-audit --no-fund" "npm install --no-save --no-audit --no-fund"; do
  rm -rf "${p:?}/packages/app/node_modules"
  (export MATCHAPACK_TEST_BROKE=1; MATCHAPACK_LOG_DIR=$work/logs-broke wrapped "$p" "$c"); rc=$?
  [ -d "$(dirname "$mk")" ] && echo broken > "$mk"
  if [ $rc = 1 ] && [ -f "$mk" ] && grep -qxF "  rm -rf $p/packages/app/node_modules/y1-b" "$work/out" && follow_advice "$p" && [ ! -e "$mk" ] \
    && [ -f "$p/packages/app/node_modules/y1-b/package.json" ] && [ -f "$p/node_modules/y1-a/package.json" ]; then
    ok "following the advice of a broken $c rebuilds the member package it placed ($(grep -c . "$work/advice.sh") commands)"
  else
    bad "follow the advice of $c: rc=$rc, marker $([ -e "$mk" ] && echo still there || echo gone), advice: $(tr '\n' '|' < "$work/advice.sh" 2>/dev/null)"
  fi
done
# The automatic plain npm ci after a broken npm ci removes them first too
# (npm ci does it itself from 10.8.1 on), and says how many.
rm -rf "${p:?}/packages/app/node_modules"
(export MATCHAPACK_TEST_BROKE=1; MATCHAPACK_LOG_DIR=$work/logs-broke wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
[ $rc = 0 ] && grep -Eq 'running npm ci to rebuild node_modules, after removing [0-9]+ package director(y|ies) matchapack placed outside node_modules' "$work/out" \
  && [ -f "$p/packages/app/node_modules/y1-b/package.json" ] \
  && ok "a broken npm ci: the member package it placed is removed before the automatic plain npm ci, which puts it back" \
  || bad "broke, ci, member package: rc=$rc, $(grep '^matchapack' "$work/out" | head -2)"
# Nothing outside the project is removed: a workspace member that is a
# symlink to a directory elsewhere has its node_modules there (npm
# extracts into it; matchapack never clones there, so it never lists it).
# The advice does not name it, and following it keeps a marker there; the
# automatic npm ci rebuild leaves it too (npm ci from 10.8.1 removes a
# member's node_modules itself, through the link, so the marker is checked
# before that only).
p=$work/fam-symws; o=$work/fam-symws-outside; rm -rf "${p:?}" "${o:?}"; mkdir -p "$p/packages" "$o"
echo '{"name":"symws","version":"1.0.0","private":true,"workspaces":["packages/*"],"dependencies":{"lc-s":"'"$FX_URL"'/lc-s-1.0.0.tgz"}}' > "$p/package.json"
echo '{"name":"ws-ext","version":"1.0.0","dependencies":{"lc-s":"'"$FX_URL"'/lc-s-2.0.0.tgz"}}' > "$o/package.json"
ln -s "$o" "$p/packages/ext"
(cd "$p" && npm install --package-lock-only --ignore-scripts --no-audit --no-fund > "$work/out" 2>&1) || bad "symlinked member: lockfile"
wrapped "$p" "npm ci --no-audit --no-fund" || bad "symlinked member: npm ci"
[ -d "$o/node_modules/lc-s" ] && echo outside > "$o/node_modules/MARKER"
(export MATCHAPACK_TEST_BROKE=1; MATCHAPACK_LOG_DIR=$work/logs-broke wrapped "$p" "npm install --no-save --no-audit --no-fund"); rc=$?
if [ $rc = 1 ] && ! grep '^  rm -rf' "$work/out" | grep -q "packages/ext\|fam-symws-outside" && follow_advice "$p" && [ -f "$o/node_modules/MARKER" ]; then
  ok "a member symlinked to outside the project: the advice does not name its node_modules; following it keeps a marker there"
else
  bad "symlinked member, advice: rc=$rc, marker $([ -f "$o/node_modules/MARKER" ] && echo kept || echo gone), $(grep '^  rm' "$work/out")"
fi
# version_lt <a> <b>: whether version a is before b (major, minor, patch).
version_lt() { node -e 'const p = (v) => v.split("-")[0].split(".").map(Number); const [a, b] = [p(process.argv[1]), p(process.argv[2])]; for (let i = 0; i < 3; i++) { if ((a[i] || 0) !== (b[i] || 0)) process.exit((a[i] || 0) < (b[i] || 0) ? 0 : 1) } process.exit(1)' "$1" "$2"; }
version_lt 10.8.0 10.8.1 && version_lt 10.5.2 10.8.1 && ! version_lt 10.8.1 10.8.1 && ! version_lt 10.9.0 10.8.1 && ! version_lt 11.0.0 10.8.1 \
  && ok "version_lt: 10.8.0 and 10.5.2 are before 10.8.1; 10.8.1, 10.9.0 and 11.0.0 are not" || bad "version_lt compares wrongly"
npm_old_ci=0; version_lt "$(npm -v)" 10.8.1 && npm_old_ci=1
(export MATCHAPACK_TEST_BROKE=1; MATCHAPACK_LOG_DIR=$work/logs-broke wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
if [ $rc = 0 ] && grep -q 'running npm ci to rebuild node_modules' "$work/out" && ! grep -q 'could not remove' "$work/out" \
  && { [ "$npm_old_ci" = 0 ] || [ -f "$o/node_modules/MARKER" ]; }; then
  ok "a broken npm ci with a member symlinked to outside: the automatic rebuild removes nothing there$([ "$npm_old_ci" = 0 ] && echo " (npm $(npm -v) removes it itself; the marker is checked before 10.8.1)")"
else
  bad "symlinked member, automatic npm ci: rc=$rc, marker $([ -f "$o/node_modules/MARKER" ] && echo kept || echo gone)"
fi
# After a broken npm ci matchapack removes only what it cloned in that run
# outside the root's node_modules, then runs plain npm ci: the result must
# be plain npm ci's on an identical copy, in each case below (after the
# independent review's round 8). Each case is built twice by plain npm
# (`plain`, `mp`); every real node_modules not inside another gets an
# extended attribute and a tool's cache (`.keep-cache/data`); then plain
# `npm ci` in one, a broken `npm ci` through matchapack (MATCHAPACK_TEST_BROKE)
# in the other. Compared: every path, and for each such node_modules
# whether it is the same directory as before and keeps the attribute.
xa=com.example.matchapack-keep
cl_nm() { find "$1" -type d -name node_modules -prune | LC_ALL=C sort; }
cl_ino() { python3 -c 'import os,sys; print(os.stat(sys.argv[1]).st_ino)' "$1"; }
# The attribute: `xattr` on macOS; Python's os.setxattr (user.*) on Linux.
cl_xset() { if command -v xattr > /dev/null; then xattr -w "$xa" keep "$1"; else python3 -c 'import os,sys; os.setxattr(sys.argv[1], "user.matchapack-keep", b"keep")' "$1" 2>/dev/null; fi; }
cl_xget() { if command -v xattr > /dev/null; then xattr -p "$xa" "$1" 2>/dev/null || echo no-xattr; else python3 -c 'import os,sys; print(os.getxattr(sys.argv[1], "user.matchapack-keep").decode())' "$1" 2>/dev/null || echo no-xattr; fi; }
cl_state() { # base -> listing with each file's content hash, then one line per node_modules
  (cd "$1" && find . -print | LC_ALL=C sort)
  python3 -I - "$1" <<'PY'
import hashlib, os, sys
base = sys.argv[1]
for root, dirs, files in sorted(os.walk(base)):
    dirs.sort()
    for n in sorted(files):
        p = os.path.join(root, n)
        if n != ".ino" and not os.path.islink(p):
            print(hashlib.sha1(open(p, "rb").read()).hexdigest(), os.path.relpath(p, base))
PY
  while IFS= read -r d; do
    printf '%s %s %s\n' "${d#$1/}" "$([ "$(cl_ino "$d")" = "$(cat "$d/.ino" 2>/dev/null)" ] && echo same || echo new)" "$(cl_xget "$d")"
  done < <(cl_nm "$1")
}
cl_pj() { mkdir -p "$(dirname "$1")"; echo "$2" > "$1"; }
cl_ws() { # a workspaces project: lc-s 1.0.0 at the root; member b wants 2.0.0 (its own node_modules)
  cl_pj "$1/proj/package.json" '{"name":"cl","version":"1.0.0","private":true,"workspaces":["packages/*"],"dependencies":{"lc-s":"'"$FX_URL"'/lc-s-1.0.0.tgz"}}'
  cl_pj "$1/proj/packages/a/package.json" '{"name":"ws-a","version":"1.0.0","dependencies":{"lc-bundler":"'"$FX_URL"'/lc-bundler-1.0.0.tgz"}}'
  cl_pj "$1/proj/packages/b/package.json" '{"name":"ws-b","version":"1.0.0","dependencies":{"lc-s":"'"$FX_URL"'/lc-s-2.0.0.tgz"}}'
}
cl_member() { cl_ws "$1"; }
cl_linkdep() { # a file: directory dependency inside the root, with its own node_modules
  cl_pj "$1/proj/package.json" '{"name":"cl","version":"1.0.0","dependencies":{"lc-s":"'"$FX_URL"'/lc-s-1.0.0.tgz","loc":"file:./loc"}}'
  cl_pj "$1/proj/loc/package.json" '{"name":"loc","version":"1.0.0","dependencies":{"lc-s":"'"$FX_URL"'/lc-s-2.0.0.tgz"}}'
}
cl_stale() { cl_ws "$1"; }
cl_stale_after() { # packages/b leaves the workspaces; its own install stays
  cl_pj "$1/proj/package.json" '{"name":"cl","version":"1.0.0","private":true,"workspaces":["packages/a"],"dependencies":{"lc-s":"'"$FX_URL"'/lc-s-1.0.0.tgz"}}'
  (cd "$1/proj/packages/b" && MATCHAPACK=0 npm install --no-package-lock --no-audit --no-fund > /dev/null 2>&1)
}
cl_fresh_after() { # the member's own node_modules gone: the broken run places its package
  rm -rf "${1:?}/proj/packages/b/node_modules"
}
cl_sync() { cl_ws "$1"; }
cl_sync_after() { # package.json asks for what the lockfile does not have: npm ci refuses
  cl_pj "$1/proj/package.json" '{"name":"cl","version":"1.0.0","private":true,"workspaces":["packages/*"],"dependencies":{"lc-s":"'"$FX_URL"'/lc-s-2.0.0.tgz"}}'
}
cl_case() { # name build [after] -- npm ci args
  local name=$1 build=$2 after=$3; shift 4
  local how base
  for how in plain mp; do
    base=$work/cl-$name/$how; rm -rf "${base:?}"; mkdir -p "$base"; base=$(cd "$base" && pwd -P)
    "$build" "$base"
    (cd "$base/proj" && MATCHAPACK=0 npm install --no-audit --no-fund > "$work/cl.out" 2>&1) || { bad "cleanup $name: plain npm install"; return; }
    [ "$after" = - ] || "$after" "$base"
    while IFS= read -r d; do
      cl_xset "$d"; mkdir -p "$d/.keep-cache"; echo cache > "$d/.keep-cache/data"; cl_ino "$d" > "$d/.ino"
    done < <(cl_nm "$base")
    if [ $how = plain ]; then
      (cd "$base/proj" && MATCHAPACK=0 npm ci "$@" > "$work/cl-$how.out" 2>&1); echo $? > "$work/cl-$how.rc"
    else
      (export MATCHAPACK_TEST_BROKE=1; MATCHAPACK_LOG_DIR=$work/logs-cl wrapped "$base/proj" "npm ci $*"); echo $? > "$work/cl-$how.rc"; cp "$work/out" "$work/cl-$how.out"
    fi
    cl_state "$base" > "$work/cl-$name-$how.state"
  done
  if [ "$(cat "$work/cl-plain.rc")" = "$(cat "$work/cl-mp.rc")" ] && cmp -s "$work/cl-$name-plain.state" "$work/cl-$name-mp.state" \
    && { grep -q 'running npm ci to rebuild node_modules' "$work/cl-mp.out" || [ "$(cat "$work/cl-mp.rc")" != 0 ]; }; then
    ok "cleanup before the automatic npm ci ($name, npm ci $*): the same tree as plain npm ci's, node_modules directories, attributes and caches included"
  else
    bad "cleanup $name: status $(cat "$work/cl-plain.rc") vs $(cat "$work/cl-mp.rc"); $(diff "$work/cl-$name-plain.state" "$work/cl-$name-mp.state" | grep '^[<>]' | head -4 | tr '\n' '|')"
  fi
}
cl_case member cl_member - -- --no-audit --no-fund
cl_case member-filtered cl_member - -- -w ws-a --no-audit --no-fund
cl_case member-placed cl_member cl_fresh_after -- --no-audit --no-fund
cl_case link-dep cl_linkdep - -- --no-audit --no-fund
cl_case stale-key cl_stale cl_stale_after -- --no-audit --no-fund
cl_case out-of-sync cl_sync cl_sync_after -- --no-audit --no-fund
# A member package npm must replace (another version on disk) holding a
# nested package npm keeps and moves back under it: matchapack removes what
# its clone brought, never the nested package (a marker in it stays, as
# with plain npm ci). Registry packages, as in the independent review.
cl_nested() {
  cl_pj "$1/proj/package.json" '{"name":"cl","version":"1.0.0","private":true,"workspaces":["packages/*"],"dependencies":{"is-number":"7.0.0","is-odd":"2.0.0"}}'
  cl_pj "$1/proj/packages/b/package.json" '{"name":"ws-b","version":"1.0.0","dependencies":{"is-number":"5.0.0","is-odd":"3.0.1"}}'
}
cl_nested_after() {
  local d=$1/proj/packages/b/node_modules/is-odd
  python3 -c 'import json,sys; p=sys.argv[1]; j=json.load(open(p)); j["version"] += "-older"; json.dump(j, open(p, "w"), indent=2)' "$d/package.json"
  [ -f "$d/node_modules/is-number/index.js" ] && echo '// marker' >> "$d/node_modules/is-number/index.js"
}
cl_case nested-kept cl_nested cl_nested_after -- --no-audit --no-fund
# npm rolls back after a failed install script and puts its own directory
# back where matchapack had placed a clone: matchapack removes only a
# directory that is still the one it placed (its device and inode).
cl_flag=$work/cl-fail-now
cl_rollback() {
  rm -f "$cl_flag"; cl_ws "$1"
  cl_pj "$1/failing-src/package.json" '{"name":"failing","version":"1.0.0","scripts":{"install":"node check.js"}}'
  cl_pj "$1/failing-src/check.js" "process.exit(require('fs').existsSync('$cl_flag') ? 1 : 0)"
  (cd "$1/failing-src" && MATCHAPACK=0 npm pack --pack-destination "$1" > /dev/null 2>&1)
  cl_pj "$1/proj/package.json" '{"name":"cl","version":"1.0.0","private":true,"workspaces":["packages/*"],"dependencies":{"lc-s":"'"$FX_URL"'/lc-s-1.0.0.tgz","failing":"file:../failing-1.0.0.tgz"},"allowScripts":{"failing":true}}'
  # npm 11.19 and 12 run a dependency's install scripts only by policy; an
  # npmrc is what both honour for a file: tarball (older npm ignore it).
  echo 'dangerously-allow-all-scripts=true' > "$1/proj/.npmrc"
}
cl_rollback_after() {
  local d=$1/proj/packages/b/node_modules/lc-s
  python3 -c 'import json,sys; p=sys.argv[1]; j=json.load(open(p)); j["version"] += "-older"; json.dump(j, open(p, "w"), indent=2)' "$d/package.json"
  echo marker > "$d/MARKER"; : > "$cl_flag"
}
cl_case rollback cl_rollback cl_rollback_after -- --no-audit --no-fund
# The case tests something only if plain npm ci failed there from the
# install script (a script npm skipped would pass it trivially).
[ "$(cat "$work/cl-plain.rc")" != 0 ] && grep -q 'node check.js' "$work/cl-plain.out" \
  && ok "the rollback case's plain npm ci failed from the install script (npm rolled back)" \
  || bad "rollback case: plain npm ci did not fail from check.js (status $(cat "$work/cl-plain.rc"))"
rm -f "$cl_flag"
# And the clones matchapack placed outside the root's node_modules are what
# it removes: in the member-placed case (the member's node_modules removed
# first, so the broken run places its package), its log record lists
# packages/b/node_modules/lc-s, and the line says it removed one.
python3 - "$work/logs-cl/log.jsonl" <<'PY' && ok "the broken npm ci's log lists what it placed outside node_modules (packages/b/node_modules/lc-s)" || bad "cleanup: placed_outside not in the log"
import json, sys
recs = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
sys.exit(0 if any(r.get("root", "").endswith("cl-member-placed/mp/proj") and "packages/b/node_modules/lc-s" in (r.get("inject") or {}).get("placed_outside", []) for r in recs if r.get("outcome") == "broke") else 1)
PY
# The clone server's thread cannot start (MATCHAPACK_TEST_BROKE=no-server):
# npm is stopped before matchapack answers, so the automatic plain npm ci
# is the only npm that finishes (one "added" line), and the tree is its.
p=$(project no-server bundled)
(export MATCHAPACK_TEST_BROKE=no-server; MATCHAPACK_LOG_DIR=$work/logs-broke wrapped "$p" "npm ci --no-audit --no-fund"); rc=$?
[ $rc = 0 ] && grep -q 'could not start the clone server' "$work/out" && grep -q 'running npm ci to rebuild node_modules' "$work/out" \
  && [ "$(grep -Ec '^added [0-9]+ package' "$work/out")" = 1 ] && [ -f "$p/node_modules/lc-s/package.json" ] \
  && [ "$(last_log_in "$work/logs-broke" "r['outcome'], r['repair']")" = "broke npm ci" ] \
  && ok "the clone server's thread cannot start: npm is stopped, then plain npm ci alone rebuilds" \
  || bad "no server thread: rc=$rc, added $(grep -Ec '^added [0-9]+ package' "$work/out"), $(grep '^matchapack' "$work/out" | head -2)"
# npm 11 warns about an abbreviation (`--prod`) once; the repair options'
# second parse must not say it again.
p=$(project prod-warn bundled)
(cd "$p" && npm ci --prod --no-audit --no-fund > "$work/plain.out" 2>&1)
want=$(grep -c 'Expanding --prod' "$work/plain.out")
wrapped "$p" "npm ci --prod --no-audit --no-fund"
[ "$(grep -c 'Expanding --prod' "$work/out")" = "$want" ] && ok "npm ci --prod: npm's abbreviation warning as often as with plain npm ($want)" \
  || bad "npm ci --prod: the warning $(grep -c 'Expanding --prod' "$work/out") times, plain npm $want"
MATCHAPACK_LOG_DIR=$tr_log "$bin" report > "$work/report" 2>&1 \
  && grep -q 'failed and stopped 1' "$work/report" \
  && grep -q "in 1 runs npm read an unexpected key of the hook's result, and the run stopped with the repair command" "$work/report" \
  && grep -q 'commands: ci 2, install 1' "$work/report" \
  && ok "trap during npm install: matchapack report counts it apart, and the runs per command" || bad "family trap report: $(grep -E 'runs|commands|unexpected' "$work/report")"

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
