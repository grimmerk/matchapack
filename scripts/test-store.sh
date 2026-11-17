#!/usr/bin/env bash
# Store maintenance tests: project records, `store gc`, `store verify`, the
# store lock, the tmp cleanup and `uninstall`, end to end with the release
# binary.
#   test-store.sh <work-dir>
# Uses its own store and log directory, never the real ones.
# `store verify` extracts with the pacote of the npm on PATH.
set -uo pipefail
work=$1; here=$(cd "$(dirname "$0")/.." && pwd); bin=$here/target/release/matchapack
mkdir -p "$work"; work=$(cd "$work" && pwd -P)
# Projects in a directory of the user's own group: under one of a group the
# user is not in (macOS /tmp: wheel), the inject engine leaves merge
# placements to npm (#41), and the entry counts below would differ.
chgrp "$(id -g)" "$work"
export MATCHAPACK_NPM_VERSIONS=any MATCHAPACK_STORE=$work/store MATCHAPACK_LOG_DIR=$work/logs
unset MATCHAPACK_ENGINE MATCHAPACK_INJECT_HOOK MATCHAPACK
store=$work/store
# moved-entry: left by the last case; a third run over the same work
# directory failed on it (the `mv` into it found a non-empty directory).
rm -rf "${store:?}" "${work:?}/logs" "${work:?}/moved-entry" "${work:?}/store-odd"
# The odd-tarballs case installs from this run's own tarball server, on a
# free port; $FX is the copy of fixtures/ whose URLs name it
# (fixture-server.sh).
. "$here/scripts/fixture-server.sh"; fixtures_serve "$work"
trap '[ -n "$FX_PID" ] && kill "$FX_PID" 2>/dev/null' EXIT
pass=0; fail=0
ok() { pass=$((pass+1)); echo "PASS $1"; }
bad() { fail=$((fail+1)); echo "FAIL $1"; [ -f "$work/out" ] && sed 's/^/    /' "$work/out" | tail -5; }

project() { local d=$work/$1; rm -rf "${d:?}"; mkdir -p "$d"; cp "$FX/small/package.json" "$FX/small/package-lock.json" "$d/"; echo "$d"; }
# install: no scripts; full_install: scripts too.
install() { "$bin" install "$1" --ignore-scripts -- --no-audit --no-fund > "$work/out" 2>&1; }
full_install() { "$bin" install "$1" -- --no-audit --no-fund > "$work/out" 2>&1; }
# classic_entries <store>: make the store's entries what the classic engine
# (removed in PR C2) left in stores it filled: no made-by record, and lc-dup's
# run.sh (listed twice in its tarball) with mode 644 where npm's extraction
# gives 755. Stores filled before PR C2 still hold such entries.
classic_entries() {
  rm -rf "${1:?}/made-by"
  local f; f=$(find "$1/v2" -path '*/run.sh' -type f | head -1)
  [ -z "$f" ] || chmod 644 "$f"
}
entries() { find "$store/v2" -mindepth 3 -maxdepth 3 -type d | wc -l | tr -d ' '; }
records() { find "$store/projects" -name '*.json' 2>/dev/null | wc -l | tr -d ' '; }
# Entries extracted by these tests are minutes old; gc keeps entries younger
# than an hour, so age them to test what gc removes.
age_entries() { find "$store/v2" -mindepth 3 -maxdepth 3 -type d -exec touch -t 202001010000 {} +; }
gc() { "$bin" store gc "$@" > "$work/out" 2>&1; }
# hold <path> <ex|sh>: a background process takes a flock on <path> (the
# store's lock file, or a directory) and keeps it until `release`. Returns
# once the lock is taken, so no case depends on timing.
hold() {
  rm -f "$work/ready" "$work/release"
  python3 -c '
import fcntl, os, sys, time
p, mode, w = sys.argv[1:]
fd = os.open(p, os.O_RDONLY) if os.path.isdir(p) else os.open(p, os.O_RDWR | os.O_CREAT)
fcntl.flock(fd, fcntl.LOCK_EX if mode == "ex" else fcntl.LOCK_SH)
open(w + "/ready", "w").close()
for _ in range(1200):
    if os.path.exists(w + "/release"):
        break
    time.sleep(0.05)
' "$1" "$2" "$work" & holder=$!
  for _ in $(seq 400); do [ -f "$work/ready" ] && return 0; sleep 0.05; done
  echo "hold: lock on $1 not taken"; return 1
}
release() { touch "$work/release"; wait "$holder"; }

# 1. An install records the project and its entries.
p1=$(project p1)
install "$p1" && n=$(entries) && [ "$(records)" = 1 ] \
  && [ "$(python3 -c "import json,glob;print(len(json.load(open(glob.glob('$store/projects/*.json')[0]))['entries']))")" = "$n" ] \
  && ok "record: one record listing all $n entries" || bad "record: install or record missing (records $(records))"

# 2. Nothing to collect while the project exists, even when the entries are old.
age_entries
gc && grep -q 'removed 0 of' "$work/out" && [ "$(entries)" = "$n" ] && ok "gc: keeps every entry a project uses" || bad "gc: removed entries in use"

# 3. A second project with the same lockfile, then deleted: its record goes,
#    the entries stay (p1 uses them).
p2=$(project p2); install "$p2"; rm -rf "${p2:?}"
gc && grep -q 'dropped: 1' "$work/out" && [ "$(records)" = 1 ] && [ "$(entries)" = "$n" ] && ok "gc: a removed project's record is dropped, shared entries kept" || bad "gc: record or shared entries handling"

# 4. Dry run, then the real thing, once no project uses the entries.
rm -rf "${p1:?}"
gc --dry-run && grep -q "would remove $n of $n" "$work/out" && [ "$(entries)" = "$n" ] && ok "gc --dry-run: reports $n, removes nothing" || bad "gc --dry-run"
gc && grep -q "removed $n of $n" "$work/out" && [ "$(entries)" = 0 ] && [ "$(records)" = 0 ] && [ -z "$(ls "$store/tmp")" ] && ok "gc: removes every unused entry, leaves nothing in tmp" || bad "gc: unused entries left ($(entries))"

# 5. Fresh entries are kept by the grace period.
p1=$(project p1); install "$p1"; rm -rf "${p1:?}"
gc && grep -q "removed 0 of $n" "$work/out" && grep -q "kept $n unused entries extracted in the last hour" "$work/out" && ok "gc: entries younger than an hour kept" || bad "gc: grace period"

# 6. A project installed by a build without records (only in the trial log)
#    keeps the entries its hidden lockfile lists.
# The hidden lockfile is written by npm's reify, scripts or not.
p3=$(project p3); full_install "$p3"; rm -f "$store"/projects/*.json; age_entries
mkdir -p "$work/logs"
python3 -c "import json,time;open('$work/logs/log.jsonl','a').write(json.dumps({'outcome':'installed','dir':'$p3','ts':time.time()})+'\\n')"
listed=$(python3 -c "import json;print(len({v['integrity'] for v in json.load(open('$p3/node_modules/.package-lock.json'))['packages'].values() if 'integrity' in v}))")
gc --dry-run && grep -q '1 from the trial log' "$work/out" && grep -q "would remove $((n - listed)) of $n" "$work/out" && ok "gc: trial-log project keeps the $listed entries its hidden lockfile lists" || bad "gc: trial-log fallback (listed $listed of $n)"

# 7. gc is refused while an install holds the store.
hold "$store/lock" sh
gc; rc=$?
release
[ $rc != 0 ] && grep -q 'in use by a running install' "$work/out" && ok "gc: refused while an install holds the store" || bad "gc: ran during an install (rc=$rc)"

# 8. verify (with npm's own pacote): all match; a changed file and a
#    changed directory mode are found and named with who made the entry;
#    --fix replaces the entry with npm's extraction.
verify() { "$bin" store verify "$@" > "$work/out" 2>&1; }
verify && grep -q "$n match, 0 differ" "$work/out" \
  && ok "verify: $n entries match npm's extraction" || bad "verify: clean store"
# Every entry is now known to this extraction code: a difference is a change.
made="changed after this extraction code had made or checked it"
victim=$(find "$store/v2" -path '*/lodash.js' -type f | head -1)
printf 'x' >> "$victim"
verify; rc=$?
[ $rc = 1 ] && grep -q "lodash.js: contents differ ($made)" "$work/out" && ok "verify: a changed file is reported (exit 1), with who made the entry" || bad "verify: changed file not found (rc=$rc)"
vdir=$(dirname "$victim"); chmod 700 "$vdir"
verify; rc=$?
[ $rc = 1 ] && grep -q "directory mode 700" "$work/out" && ok "verify: a changed directory mode is reported" || bad "verify: directory mode (rc=$rc)"
verify --fix && grep -q 'replaced 1 entries with npm' "$work/out" && [ "$(entries)" = "$n" ] && ! grep -q '^x' "$victim" && [ "$(tail -c 1 "$victim")" != x ] \
  && [ "$(python3 -c 'import os,sys; print(format(os.stat(sys.argv[1]).st_mode & 0o7777, "o"))' "$vdir")" = 755 ] && ok "verify --fix: the entry is replaced by npm's extraction" || bad "verify --fix"
verify && grep -q "$n match, 0 differ" "$work/out" && ok "verify: after --fix, every entry matches" || bad "verify: after --fix"

# 8b. Entries made by the classic engine's extractor (no record; in stores
#     filled before PR C2) are judged by npm's extraction too: odd-tarballs'
#     lc-dup (a path listed twice) differs and is replaced; the matching one
#     is recorded as known to this extraction code, under the key an
#     install uses: the next install compares nothing.
if curl -sf -o /dev/null "$FX_URL/lc-dup-1.0.0.tgz"; then
  os=$work/store-odd; rm -rf "${os:?}"
  po=$work/odd; rm -rf "${po:?}"; mkdir -p "$po"; cp "$FX/odd-tarballs/package.json" "$FX/odd-tarballs/package-lock.json" "$po/"
  MATCHAPACK_STORE=$os "$bin" install "$po" --ignore-scripts -- --no-audit --no-fund > "$work/out" 2>&1 || bad "odd-tarballs: install"
  classic_entries "$os"
  MATCHAPACK_STORE=$os "$bin" store verify > "$work/out" 2>&1; rc=$?
  [ $rc = 1 ] && grep -q "2 entries compared.*: 1 match, 1 differ" "$work/out" && grep -q "run.sh: mode 644, the tarball gives 755 (made by the classic engine's extractor)" "$work/out" \
    && grep -q '1 matching entries recorded as known to fetcher:[0-9a-f]\{32\} tar:[0-9a-f]\{32\} uv:' "$work/out" \
    && ok "verify: the classic engine's entry for a path listed twice differs from npm's extraction; the other is recorded" || bad "verify odd-tarballs (rc=$rc)"
  MATCHAPACK_STORE=$os "$bin" store verify --fix > "$work/out" 2>&1 && MATCHAPACK_STORE=$os "$bin" store verify > "$work/out" 2>&1 && grep -q '2 match, 0 differ' "$work/out" \
    && ok "verify --fix: replaced by npm's extraction, then both match" || bad "verify --fix odd-tarballs"
  po2=$work/odd2; rm -rf "${po2:?}"; mkdir -p "$po2"; cp "$FX/odd-tarballs/package.json" "$FX/odd-tarballs/package-lock.json" "$po2/"
  MATCHAPACK_STORE=$os MATCHAPACK_INJECT_CHECK=1000000 "$bin" install "$po2" --ignore-scripts -- --no-audit --no-fund > "$work/out" 2>&1 \
    && grep -q ' 0 checked against pacote' "$work/out" && grep -q 'inject: cloned 2 ' "$work/out" \
    && ok "verify then install: the install compares nothing (verify recorded the key the install uses)" || bad "verify then install: $(grep -Eo '[0-9]+ checked against pacote' "$work/out")"
else
  echo "SKIP verify odd-tarballs: the fixtures' tarball server does not answer ($FX_URL)"
fi

# 9. tmp cleanup: an old directory nobody holds goes; one a process holds stays.
mkdir -p "$store/tmp/1-left/sub" "$store/tmp/1-held"; touch -t 202001010000 "$store/tmp/1-left" "$store/tmp/1-held"
hold "$store/tmp/1-held" ex
gc --dry-run
release
[ ! -e "$store/tmp/1-left" ] && [ -d "$store/tmp/1-held" ] && ok "tmp: leftover removed, held directory kept" || bad "tmp: cleanup ($(ls "$store/tmp" | tr '\n' ' '))"

# 10. A real install holds the store until it ends: its root postinstall
#     (run by the post-install step, the last one) tries a gc, which is refused.
p5=$(project p5)
python3 -c "import json;p='$p5/package.json';d=json.load(open(p));d['scripts']['postinstall']='\"$bin\" store gc > gc.txt 2>&1; true';json.dump(d,open(p,'w'))"
full_install "$p5" && grep -q 'in use by a running install' "$p5/gc.txt" && ok "lock: held through the root scripts (a gc from postinstall is refused)" || bad "lock: gc ran inside an install ($(cat "$p5/gc.txt" 2>/dev/null | head -1))"

# 11. An install started while a gc holds the store waits for it: it is
#     still running when the lock is released, then completes.
p6=$(project p6)
hold "$store/lock" ex
install "$p6" & inst=$!
for _ in $(seq 1200); do grep -q 'waiting for a `matchapack store` command' "$work/out" 2>/dev/null && break; sleep 0.05; done
sleep 1   # time it would need to get past the lock if it did not wait
kill -0 "$inst" 2>/dev/null && waited=yes || waited=no
release
wait "$inst"; rc=$?
[ $rc = 0 ] && [ $waited = yes ] && grep -q 'waiting for a `matchapack store` command' "$work/out" && [ -d "$p6/node_modules/lodash" ] && ok "lock: an install waits for a running gc, then completes" || bad "lock: install during gc (rc=$rc, still running at release: $waited)"

# 12. An entry replaced by a symlink: verify reports it, --fix removes it,
#     and an install never clones it (npm extracts that package itself
#     instead of placing a link).
# The lodash entry: lodash.js sits at the package root, four levels down.
victim=$(dirname "$(find "$store/v2" -mindepth 4 -maxdepth 4 -name lodash.js | head -1)")
mv "$victim" "$work/moved-entry"; ln -s "$work/moved-entry" "$victim"
p7=$(project p7); install "$p7"; irc=$?
[ $irc = 0 ] && grep -q 'rejected-source' "$work/out" && [ ! -L "$p7/node_modules/lodash" ] && [ -f "$p7/node_modules/lodash/lodash.js" ]; iok=$?
"$bin" store verify > "$work/out" 2>&1; rc=$?
[ $iok = 0 ] && [ $rc = 1 ] && grep -q 'not a directory' "$work/out" && "$bin" store verify --fix > "$work/out" 2>&1 && [ ! -L "$victim" ] && [ -d "$work/moved-entry" ] \
  && ok "verify: a symlinked entry is reported and removed (the link only); an install refuses it" || bad "verify: symlinked entry (install rc=$irc, verify rc=$rc)"

# 13. scripts/verify.sh (verify-sample.sh runs it on real projects against
#     the real store) leaves no project record for its copies: on exit it
#     removes the records pointing into its work directory, and only those.
#     Planted: a record for a project elsewhere, and one whose path only
#     starts with the work directory's name (a sibling directory).
vw=$work/vs; rm -rf "${vw:?}" "${work:?}/vs-other"; mkdir -p "$vw" "$store/projects"
plant() { printf '{"path":"%s","ts":0,"build":"test","entries":[]}\n' "$2" > "$store/projects/$1.json"; }
plant 0123456789abcdef0123456789abcdef "$work/elsewhere-project"
plant fedcba9876543210fedcba9876543210 "$work/vs-other/B"
before=$(ls "$store/projects" | sort)
"$here/scripts/verify.sh" "$FX/small" "$vw" > "$work/out" 2>&1; rc=$?
copies=$(grep -l "\"$vw/" "$store"/projects/*.json 2>/dev/null | wc -l | tr -d ' ')
[ $rc = 0 ] && grep -q 'RESULT: IDENTICAL' "$work/out" && [ "$copies" = 0 ] && [ "$(ls "$store/projects" | sort)" = "$before" ] \
  && ok "verify.sh: no project record left for its copies; the other records kept" || bad "verify.sh records: rc=$rc, $copies left for its copies, records $(ls "$store/projects" | wc -l | tr -d ' ') (before $(echo "$before" | wc -l | tr -d ' '))"

# 14. `store adopt` (#30), on a store of its own: projects installed before
#     records existed. A (odd-tarballs, 2 entries) is under the scanned
#     directory; B (fixtures/small) is outside it, reached only through
#     symlinks, a symlinked node_modules and a copy of its hidden lockfile
#     inside a package; a copy of it in a node_modules nobody can search
#     (reported, not taken for "no hidden lockfile"). Adopting records A
#     only; gc then keeps A's entries,
#     removes B's, and B's files still read the same (clones are
#     independent of the store entries they were cloned from).
if curl -sf -o /dev/null "$FX_URL/lc-dup-1.0.0.tgz"; then
  store=$work/store-adopt; export MATCHAPACK_STORE=$store MATCHAPACK_LOG_DIR=$work/logs-adopt
  ar=$work/adopt-root; ao=$work/adopt-other
  [ -d "$ar/locked" ] && chmod 755 "$ar/locked"
  [ -d "$ar/e/node_modules" ] && chmod 755 "$ar/e/node_modules"
  rm -rf "${store:?}" "${work:?}/logs-adopt" "${ar:?}" "${ao:?}"
  mkdir -p "$ar/a" "$ao/b"
  cp "$FX/odd-tarballs/package.json" "$FX/odd-tarballs/package-lock.json" "$ar/a/"
  cp "$FX/small/package.json" "$FX/small/package-lock.json" "$ao/b/"
  install "$ar/a" && na=$(entries) && MATCHAPACK_VERBOSE=1 install "$ao/b" && grep -q 'inject: cloned [1-9]' "$work/out" && nb=$(( $(entries) - na )) \
    || bad "adopt: setup installs"
  rm -f "$store"/projects/*.json; age_entries
  ln -s "$ao" "$ar/out"; ln -s "$ar" "$ar/a/loop"
  mkdir -p "$ar/c" "$ar/d/node_modules/pkg/node_modules"; ln -s "$ao/b/node_modules" "$ar/c/node_modules"
  cp "$ao/b/node_modules/.package-lock.json" "$ar/d/node_modules/pkg/node_modules/"
  mkdir -p "$ar/locked"; chmod 000 "$ar/locked"
  mkdir -p "$ar/e/node_modules"; cp "$ao/b/node_modules/.package-lock.json" "$ar/e/node_modules/"; chmod 000 "$ar/e/node_modules"
  # Root (CAP_DAC_OVERRIDE, a Docker run) reads mode-000 directories: then
  # there is nothing unreadable to report, and e would be a project using
  # B's entries, so it is removed.
  skips="1 node_modules is a symlink"
  if [ -e "$ar/e/node_modules/.package-lock.json" ]; then
    chmod 755 "$ar/e/node_modules"; rm -r "${ar:?}/e"
  else
    skips="1 hidden lockfile unreadable, $skips"
  fi
  ls "$ar/locked" >/dev/null 2>&1 || skips="$skips, 1 unreadable"
  adopt() { "$bin" store adopt "$@" > "$work/out" 2>&1; }
  # 14a. Dry run: lists A, writes nothing.
  adopt --dry-run "$ar" && grep -q ': 1 npm projects' "$work/out" && grep -q 'records to write: 1 new' "$work/out" \
    && grep -q "^  $ar/a: would record, $na store entries" "$work/out" && [ "$(records)" = 0 ] \
    && ok "adopt --dry-run: lists the one project, writes no record" || bad "adopt --dry-run (records $(records))"
  # 14b. The real run: one record, A's entries; nothing behind a symlink or
  #      inside a node_modules; what it cannot read is reported.
  adopt "$ar" && [ "$(records)" = 1 ] && grep -q "$ar/a\"" "$store"/projects/*.json \
    && grep -q "now kept by these projects: $na of $((na + nb))" "$work/out" \
    && grep -q "skipped: $skips (" "$work/out" \
    && ok "adopt: records the project, follows no symlink, skips node_modules contents and what it cannot read" || bad "adopt: records $(records)"
  # 14c. B's files share blocks with the store before gc (they are clones);
  #      gc removes B's entries; B's files read the same afterwards.
  sum() { python3 -c '
import hashlib, os, sys
h = hashlib.sha256()
for d, ds, fs in sorted(os.walk(sys.argv[1])):
    ds.sort()
    for f in sorted(fs):
        p = os.path.join(d, f)
        h.update(p.encode())
        if not os.path.islink(p):
            h.update(open(p, "rb").read())
print(h.hexdigest())' "$1"; }
  before=$(sum "$ao/b/node_modules")
  shared=$(python3 "$here/scripts/shared-blocks.py" "$ao/b" "$store/v2" 2>/dev/null)
  gc && grep -q "removed $nb of $((na + nb))" "$work/out" && [ "$(entries)" = "$na" ] \
    && ok "gc after adopt: keeps the adopted project's $na entries, removes the $nb only an unadopted project uses" || bad "gc after adopt ($(entries) left)"
  case $shared in shared\ 0\ *|'') shared_ok=no ;; *) shared_ok=yes ;; esac
  [ "$shared_ok" = yes ] && [ "$(sum "$ao/b/node_modules")" = "$before" ] && (cd "$ao/b" && node -e 'require("lodash")') \
    && ok "gc: an unadopted project's files read the same after its entries are removed (before: $shared)" || bad "gc: B's files after removal (before: $shared)"
  chmod 755 "$ar/locked"; [ -d "$ar/e/node_modules" ] && chmod 755 "$ar/e/node_modules"
else
  echo "SKIP adopt: the fixtures' tarball server does not answer ($FX_URL)"
fi

# 15. `matchapack uninstall`, on a home, store, log and settings directory
#     of its own (every path under $work; checked before each run): the dry
#     run changes nothing; --yes is refused while an install holds the store
#     lock; then it removes the three directories, leaves the startup file
#     alone (naming its line), and the project's files read the same and
#     still load.
if curl -sf -o /dev/null http://127.0.0.1:48321/lc-q-1.0.0.tgz; then
  uh=$work/uninstall-home; ust=$uh/.cache/matchapack/store
  [ -d "$uh" ] && chmod -R u+w "$uh"
  rm -rf "${uh:?}" "${work:?}/uninstall-project"
  mkdir -p "$uh/.config/matchapack" "$uh/.cargo/bin"
  printf 'export PATH="$HOME/.cargo/bin:$PATH"\ncommand -v matchapack >/dev/null && eval "$(matchapack init zsh --alias mp)"\n' > "$uh/.zshrc"
  echo "$work" > "$uh/.config/matchapack/paths"
  # Every location uninstall resolves, from this home only.
  uenv() { env -u ZDOTDIR -u XDG_CONFIG_HOME -u CARGO_HOME -u MATCHAPACK_STORE -u MATCHAPACK_LOG_DIR -u MATCHAPACK_CONFIG_DIR -u MATCHAPACK_PATHS HOME="$uh" "$@"; }
  uninstall() {
    case $uh in "$work"/*) ;; *) echo "refusing: $uh is not under $work"; return 99 ;; esac
    uenv "$bin" uninstall "$@" > "$work/out" 2>&1
  }
  snap() { find "$uh" -exec stat -f '%N %HT %z %p %m' {} + 2>/dev/null | sort; }
  up=$(project uninstall-project)
  uenv "$bin" install "$up" --ignore-scripts -- --no-audit --no-fund > "$work/out" 2>&1 && [ -d "$ust/v2" ] && [ -f "$uh/.cache/matchapack/log.jsonl" ] \
    || bad "uninstall: setup install into the scratch home"
  usum() { (cd "$up" && find node_modules -type f -exec shasum {} + | sort | shasum); }
  ubefore=$(usum)
  # 15a. The dry run (the default) changes nothing.
  s0=$(snap)
  uninstall && [ "$(snap)" = "$s0" ] && grep -q "store: $ust: would remove" "$work/out" && grep -q "^  $uh/.zshrc:2: command -v matchapack" "$work/out" \
    && grep -q 'recorded as used by 1 project' "$work/out" \
    && ok "uninstall: the dry run names what it would remove and the rc line, and writes nothing" || bad "uninstall: dry run"
  # 15b. Refused while an install holds the store (a shared lock, as installs take it).
  hold "$ust/lock" sh
  uninstall --yes; rc=$?
  release
  [ $rc = 1 ] && grep -q 'an install is using the store' "$work/out" && [ "$(snap)" = "$s0" ] \
    && ok "uninstall --yes: refused while an install holds the store, nothing removed" || bad "uninstall --yes during an install (rc=$rc)"
  # 15c. The real run.
  uninstall --yes && [ ! -e "$uh/.cache/matchapack" ] && [ ! -e "$uh/.config/matchapack" ] && grep -q 'command -v matchapack' "$uh/.zshrc" \
    && grep -q "free space on the store's volume" "$work/out" && [ "$(usum)" = "$ubefore" ] && (cd "$up" && node -e 'require("lodash")') \
    && ok "uninstall --yes: store, logs and settings removed, the rc file untouched, the project's files the same and loadable" || bad "uninstall --yes"
else
  echo "SKIP uninstall: the fixtures' tarball server is not running (127.0.0.1:48321)"
fi

echo "== $pass passed, $fail failed"
[ "$fail" = 0 ]
