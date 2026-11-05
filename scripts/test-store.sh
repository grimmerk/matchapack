#!/usr/bin/env bash
# Store maintenance tests: project records, `store gc`, `store verify`, the
# store lock and the tmp cleanup, end to end with the release binary.
#   test-store.sh <work-dir>
# Uses its own store and log directory, never the real ones.
set -uo pipefail
work=$1; here=$(cd "$(dirname "$0")/.." && pwd); bin=$here/target/release/thinclone
mkdir -p "$work"; work=$(cd "$work" && pwd -P)
export THINCLONE_NPM_VERSIONS=any THINCLONE_STORE=$work/store THINCLONE_LOG_DIR=$work/logs
store=$work/store
rm -rf "${store:?}" "${work:?}/logs"
pass=0; fail=0
ok() { pass=$((pass+1)); echo "PASS $1"; }
bad() { fail=$((fail+1)); echo "FAIL $1"; [ -f "$work/out" ] && sed 's/^/    /' "$work/out" | tail -5; }

project() { local d=$work/$1; rm -rf "${d:?}"; mkdir -p "$d"; cp "$here/fixtures/small/package.json" "$here/fixtures/small/package-lock.json" "$d/"; echo "$d"; }
install() { "$bin" install "$1" --no-rebuild > "$work/out" 2>&1; }
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
# The hidden lockfile is written by the post-install step: no --no-rebuild here.
p3=$(project p3); "$bin" install "$p3" > "$work/out" 2>&1; rm -f "$store"/projects/*.json; age_entries
mkdir -p "$work/logs"
python3 -c "import json,time;open('$work/logs/log.jsonl','a').write(json.dumps({'outcome':'installed','dir':'$p3','ts':time.time()})+'\\n')"
listed=$(python3 -c "import json;print(len({v['integrity'] for v in json.load(open('$p3/node_modules/.package-lock.json'))['packages'].values() if 'integrity' in v}))")
gc --dry-run && grep -q '1 from the trial log' "$work/out" && grep -q "would remove $((n - listed)) of $n" "$work/out" && ok "gc: trial-log project keeps the $listed entries its hidden lockfile lists" || bad "gc: trial-log fallback (listed $listed of $n)"

# 7. gc is refused while an install holds the store.
hold "$store/lock" sh
gc; rc=$?
release
[ $rc != 0 ] && grep -q 'in use by a running install' "$work/out" && ok "gc: refused while an install holds the store" || bad "gc: ran during an install (rc=$rc)"

# 8. verify: all match; a changed file is found, --fix removes the entry,
#    the next install puts it back.
"$bin" store verify > "$work/out" 2>&1 && grep -q "$n match" "$work/out" && ok "verify: $n entries match their tarballs" || bad "verify: clean store"
victim=$(find "$store/v2" -path '*/lodash.js' -type f | head -1)
printf 'x' >> "$victim"
"$bin" store verify > "$work/out" 2>&1; rc=$?
[ $rc = 1 ] && grep -q 'lodash.js: contents differ' "$work/out" && ok "verify: a changed file is reported (exit 1)" || bad "verify: changed file not found (rc=$rc)"
"$bin" store verify --fix > "$work/out" 2>&1 && grep -q 'removed 1 entries' "$work/out" && [ "$(entries)" = $((n - 1)) ] && ok "verify --fix: the entry is removed" || bad "verify --fix"
p4=$(project p4); install "$p4" && "$bin" store verify > "$work/out" 2>&1 && grep -q "$n match" "$work/out" && ok "verify: the next install re-extracts it" || bad "verify: re-extract after --fix"

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
"$bin" install "$p5" > "$work/out" 2>&1 && grep -q 'in use by a running install' "$p5/gc.txt" && ok "lock: held through the root scripts (a gc from postinstall is refused)" || bad "lock: gc ran inside an install ($(cat "$p5/gc.txt" 2>/dev/null | head -1))"

# 11. An install started while a gc holds the store waits for it: it is
#     still running when the lock is released, then completes.
p6=$(project p6)
hold "$store/lock" ex
"$bin" install "$p6" --no-rebuild > "$work/out" 2>&1 & inst=$!
for _ in $(seq 1200); do grep -q 'waiting for a `thinclone store` command' "$work/out" 2>/dev/null && break; sleep 0.05; done
sleep 1   # time it would need to get past the lock if it did not wait
kill -0 "$inst" 2>/dev/null && waited=yes || waited=no
release
wait "$inst"; rc=$?
[ $rc = 0 ] && [ $waited = yes ] && grep -q 'waiting for a `thinclone store` command' "$work/out" && [ -d "$p6/node_modules/lodash" ] && ok "lock: an install waits for a running gc, then completes" || bad "lock: install during gc (rc=$rc, still running at release: $waited)"

# 12. An entry replaced by a symlink: verify reports it, --fix removes it,
#     and an install never clones it (it goes to npm instead of placing a link).
# The lodash entry: lodash.js sits at the package root, four levels down.
victim=$(dirname "$(find "$store/v2" -mindepth 4 -maxdepth 4 -name lodash.js | head -1)")
mv "$victim" "$work/moved-entry"; ln -s "$work/moved-entry" "$victim"
p7=$(project p7); install "$p7"; irc=$?
"$bin" store verify > "$work/out" 2>&1; rc=$?
[ $irc != 0 ] && [ $rc = 1 ] && grep -q 'not a directory' "$work/out" && "$bin" store verify --fix > "$work/out" 2>&1 && [ ! -L "$victim" ] && [ -d "$work/moved-entry" ] \
  && ok "verify: a symlinked entry is reported and removed (the link only); an install refuses it" || bad "verify: symlinked entry (install rc=$irc, verify rc=$rc)"

echo "== $pass passed, $fail failed"
[ "$fail" = 0 ]
