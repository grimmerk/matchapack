#!/usr/bin/env bash
# Store maintenance tests: project records, `store gc`, `store verify`, the
# store lock and the tmp cleanup, end to end with the release binary.
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
# The odd-tarballs case installs from the fixtures' tarball server.
server=
if ! curl -sf -o /dev/null http://127.0.0.1:48321/lc-q-1.0.0.tgz; then
  python3 -m http.server 48321 --bind 127.0.0.1 --directory "$here/fixtures/optional-upward/tarballs" >/dev/null 2>&1 &
  server=$!
  for _ in $(seq 100); do curl -sf -o /dev/null http://127.0.0.1:48321/lc-q-1.0.0.tgz && break; sleep 0.1; done
fi
trap '[ -n "$server" ] && kill "$server" 2>/dev/null' EXIT
pass=0; fail=0
ok() { pass=$((pass+1)); echo "PASS $1"; }
bad() { fail=$((fail+1)); echo "FAIL $1"; [ -f "$work/out" ] && sed 's/^/    /' "$work/out" | tail -5; }

project() { local d=$work/$1; rm -rf "${d:?}"; mkdir -p "$d"; cp "$here/fixtures/small/package.json" "$here/fixtures/small/package-lock.json" "$d/"; echo "$d"; }
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
# Every entry is now known to this pacote/tar: a difference is a change.
made="changed after this pacote/tar had made or checked it"
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
#     is recorded as known to this pacote.
if curl -sf -o /dev/null http://127.0.0.1:48321/lc-dup-1.0.0.tgz; then
  os=$work/store-odd; rm -rf "${os:?}"
  po=$work/odd; rm -rf "${po:?}"; mkdir -p "$po"; cp "$here/fixtures/odd-tarballs/package.json" "$here/fixtures/odd-tarballs/package-lock.json" "$po/"
  MATCHAPACK_STORE=$os "$bin" install "$po" --ignore-scripts -- --no-audit --no-fund > "$work/out" 2>&1 || bad "odd-tarballs: install"
  classic_entries "$os"
  MATCHAPACK_STORE=$os "$bin" store verify > "$work/out" 2>&1; rc=$?
  [ $rc = 1 ] && grep -q "2 entries compared.*: 1 match, 1 differ" "$work/out" && grep -q "run.sh: mode 644, the tarball gives 755 (made by the classic engine's extractor)" "$work/out" \
    && grep -q '1 matching entries recorded as known to pacote@' "$work/out" \
    && ok "verify: the classic engine's entry for a path listed twice differs from npm's extraction; the other is recorded" || bad "verify odd-tarballs (rc=$rc)"
  MATCHAPACK_STORE=$os "$bin" store verify --fix > "$work/out" 2>&1 && MATCHAPACK_STORE=$os "$bin" store verify > "$work/out" 2>&1 && grep -q '2 match, 0 differ' "$work/out" \
    && ok "verify --fix: replaced by npm's extraction, then both match" || bad "verify --fix odd-tarballs"
else
  echo "SKIP verify odd-tarballs: the fixtures' tarball server is not running (127.0.0.1:48321)"
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
"$here/scripts/verify.sh" "$here/fixtures/small" "$vw" > "$work/out" 2>&1; rc=$?
copies=$(grep -l "\"$vw/" "$store"/projects/*.json 2>/dev/null | wc -l | tr -d ' ')
[ $rc = 0 ] && grep -q 'RESULT: IDENTICAL' "$work/out" && [ "$copies" = 0 ] && [ "$(ls "$store/projects" | sort)" = "$before" ] \
  && ok "verify.sh: no project record left for its copies; the other records kept" || bad "verify.sh records: rc=$rc, $copies left for its copies, records $(ls "$store/projects" | wc -l | tr -d ' ') (before $(echo "$before" | wc -l | tr -d ' '))"

echo "== $pass passed, $fail failed"
[ "$fail" = 0 ]
