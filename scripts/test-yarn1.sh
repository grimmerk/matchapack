#!/usr/bin/env bash
# Yarn 1 harness tests:   test-yarn1.sh <work-dir>
# Plain Yarn 1 against plain Yarn 1 on the yarn1-* fixtures must come out
# identical under `matchapack diff --meta`, install outputs must be
# classified rather than reported, and --meta must catch each difference it
# exists for: a gid, a sub-millisecond mtime on a package file, an extra xattr
# name, a directory mode, a .yarn-integrity byte, and the two differences
# between plain Yarn and the study's prototype (an empty file among the first
# copies; the file group under Node >= 22). Needs the release binary, node and
# Yarn 1 (MATCHAPACK_YARN_NODE / MATCHAPACK_YARN_JS as in verify-yarn1.sh).
# Installs from the local tarball server only; never touches the real Yarn
# cache or ~/.yarnrc (checked).
set -uo pipefail
work=${1:?usage: test-yarn1.sh <work-dir>}
here=$(cd "$(dirname "$0")/.." && pwd); bin=$here/target/release/matchapack
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 2; }
mkdir -p "$work"; work=$(cd "$work" && pwd -P)
v=$here/scripts/verify-yarn1.sh
node=${MATCHAPACK_YARN_NODE:-$(command -v node)}
yarnjs=${MATCHAPACK_YARN_JS:-$(python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$(command -v yarn)")}
export MATCHAPACK_YARN_NODE=$node MATCHAPACK_YARN_JS=$yarnjs
node_major=$("$node" -p 'process.versions.node.split(".")[0]')
pass=0; fail=0
ok() { pass=$((pass+1)); echo "PASS $1"; }
bad() { fail=$((fail+1)); echo "FAIL $1"; [ -f "$work/out" ] && sed 's/^/    /' "$work/out" | tail -8; }
yarnrc_before=$(ls -l ~/.yarnrc 2>/dev/null || echo absent)

server=
if ! curl -sf -o /dev/null http://127.0.0.1:48321/y1-b-1.0.0.tgz; then
  python3 -m http.server 48321 --bind 127.0.0.1 --directory "$here/fixtures/optional-upward/tarballs" >/dev/null 2>&1 &
  server=$!
  for _ in $(seq 100); do curl -sf -o /dev/null http://127.0.0.1:48321/y1-b-1.0.0.tgz && break; sleep 0.1; done
fi
trap '[ -n "$server" ] && kill "$server" 2>/dev/null' EXIT

# verify <case> [verify-yarn1 options...] <fixture or dir>: output in $work/out, exit in $rc.
verify() { local c=$1; shift; "$v" --work "$work/$c" "$@" > "$work/out" 2>&1; rc=$?; }
# field <case> <name>: the "<start>:<end>" or cache of that run in <case>/runs.
window() { awk -v n="$2" '$1 == n { print $2 }' "$work/$1/runs"; }
cachedir() { awk -v n="$2" '$1 == n { print $3 }' "$work/$1/runs"; }
# meta <case> <B dir>: diff --meta of <case>'s plain1 against <B dir>, which
# ran in <case>'s plain2 window; output in $work/out, exit in $rc.
meta() {
  "$bin" diff --meta "$work/$1/plain1" "$2" --a-run "$(window "$1" plain1)" --b-run "$(window "$1" plain2)" \
    --a-cache "$(cachedir "$1" plain1)" --b-cache "$(cachedir "$1" plain2)" > "$work/out" 2>&1; rc=$?
}
# meta_self <case> <B dir>: <case>'s plain1 against <B dir>, a changed copy
# of plain1, both sides with plain1's window and cache (independent of
# whether the two plain runs agree).
meta_self() {
  "$bin" diff --meta "$work/$1/plain1" "$2" --a-run "$(window "$1" plain1)" --b-run "$(window "$1" plain1)" \
    --a-cache "$(cachedir "$1" plain1)" --b-cache "$(cachedir "$1" plain1)" > "$work/out" 2>&1; rc=$?
}
count() { grep -E "^  $1: " "$work/out" | awk '{print $NF}'; }
classified() { grep -Eo "$1 [0-9]+" "$work/out" | grep -Eo '[0-9]+$'; }
# copy2 <case> <name>: a copy of <case>'s plain2 (times, modes, xattrs kept).
# Not a clone (cp -c): a clone takes the directory's group, not the file's.
if [ "$(uname)" = Darwin ]; then cpp() { cp -p -R "$1" "$2"; }; else cpp() { cp -a "$1" "$2"; }; fi
copy2() { local d=$work/$1-$2; rm -rf "${d:?}"; cpp "$work/$1/${3:-plain2}" "$d"; echo "$d"; }
set_ns() { python3 -c 'import os,sys; t=int(sys.argv[2]); os.utime(sys.argv[1], ns=(t, t), follow_symlinks=False)' "$1" "$2"; }
mtime_of() { python3 -c 'import os,sys; print(os.lstat(sys.argv[1]).st_mtime_ns)' "$1"; }
# has_xattr <file> <name>: the macOS xattr tool; os.listxattr on Linux.
if [ "$(uname)" = Darwin ]; then has_xattr() { xattr "$1" 2>/dev/null | grep -qx "$2"; }
else has_xattr() { python3 -c 'import os,sys; sys.exit(0 if sys.argv[2] in os.listxattr(sys.argv[1], follow_symlinks=False) else 1)' "$1" "$2"; }; fi
# probe_only <case> <B dir> <B window> <B cache>: does <case>'s plain1
# differ from <B dir> ONLY the way Yarn's time probe makes two plain runs
# differ (NOTES "Yarn 1 harness")? Mtime differences only (at least one),
# every one on a file of the y1-empty package; no other class, no path only
# on one side, every .yarn-integrity byte-identical. Sets $why when not.
probe_only() {
  local args=(--meta "$work/$1/plain1" "$2" --a-run "$(window "$1" plain1)" --b-run "$3" --a-cache "$(cachedir "$1" plain1)" --b-cache "$4")
  why=
  "$bin" diff "${args[@]}" > "$work/probe.txt" 2>&1; local r=$?
  "$bin" diff "${args[@]}" --paths > "$work/probe-paths.txt" 2>/dev/null
  [ $r = 1 ] || { why="diff exit $r"; return 1; }
  grep -qx 'differ: 0' "$work/probe.txt" && grep -qx 'only in A: 0' "$work/probe.txt" && grep -qx 'only in B: 0' "$work/probe.txt" \
    || { why="paths or contents differ"; return 1; }
  local c n
  for c in type content file-mode dir-mode symlink-target uid gid xattr-names; do
    n=$(grep -E "^  $c: " "$work/probe.txt" | awk '{print $NF}')
    [ "$n" = 0 ] || { why="class $c: ${n:-missing}"; return 1; }
  done
  grep -q 'yarn integrity byte-identical.*: false' "$work/probe.txt" && { why=".yarn-integrity differs"; return 1; }
  n=$(grep -E '^  mtime: ' "$work/probe.txt" | awk '{print $NF}')
  [ "${n:-0}" -gt 0 ] || { why="no mtime difference"; return 1; }
  local q
  while IFS= read -r q; do
    case $q in node_modules/y1-empty/*/*) why="mtime outside the package's top level: $q"; return 1 ;; node_modules/y1-empty/?*) ;; *) why="mtime outside the package: $q"; return 1 ;; esac
    [ -f "$work/$1/plain1/$q" ] && [ ! -L "$work/$1/plain1/$q" ] || { why="not a package file: $q"; return 1; }
  done < "$work/probe-paths.txt"
  return 0
}
groups_other() { # a group the user is in, other than the one of $1
  local g; g=$(stat -f %g "$1" 2>/dev/null || stat -c %g "$1")
  for x in $(id -G); do [ "$x" != "$g" ] && { echo "$x"; return; }; done
}

# --- Plain against plain -----------------------------------------------------
verify small "$here/fixtures/yarn1-small"
n=$(classified 'written during both runs')
if [ $rc = 0 ] && grep -q 'plain vs plain: IDENTICAL' "$work/out" && grep -q 'yarn integrity byte-identical (node_modules/.yarn-integrity): true' "$work/out" && [ "${n:-0}" -gt 0 ]; then
  ok "yarn1-small, warm cache: plain vs plain identical; $n in-run paths classified, not reported"
else bad "yarn1-small, warm cache (exit $rc)"; fi

verify small-empty --cache empty "$here/fixtures/yarn1-small"
n=$(classified "from each run's own cache fill")
if [ $rc = 0 ] && [ "${n:-0}" -gt 0 ]; then
  ok "yarn1-small, empty caches: identical; $n package files kept their own run's cache-fill time"
else bad "yarn1-small, empty caches (exit $rc)"; fi

verify script "$here/fixtures/yarn1-scriptpkg"
if [ $rc = 0 ] && [ -f "$work/script/plain1/node_modules/y1-script/built.txt" ] \
  && grep -q 'written during both runs: node_modules/y1-script/built.txt' "$work/script/diff-plain.txt"; then
  ok "yarn1-scriptpkg: the install script's output is classified 'written during both runs'"
else bad "yarn1-scriptpkg (exit $rc)"; fi

# Under Node >= 22 plain Yarn's own result on this fixture depends on a race
# (NOTES "Yarn 1 harness"): mostly identical, sometimes "plain varies". A
# varying run passes only when it varies the way the race does (probe_only);
# its evidence is kept as emptyfirst-varies-<case>.txt.
for c in empty empty-empty; do
  if [ $c = empty ]; then verify $c "$here/fixtures/yarn1-emptyfirst"; else verify $c --cache empty "$here/fixtures/yarn1-emptyfirst"; fi
  if [ $rc = 0 ]; then ok "yarn1-emptyfirst ($c), node $node_major: plain vs plain identical"
  elif [ $rc = 3 ] && [ "$node_major" -ge 22 ]; then
    cp "$work/$c/diff-plain.txt" "$work/emptyfirst-varies-$c.txt"
    if probe_only "$c" "$work/$c/plain2" "$(window "$c" plain2)" "$(cachedir "$c" plain2)"; then
      ok "yarn1-emptyfirst ($c), node $node_major: PLAIN VARIES, mtimes of package files only (the probe race; $work/emptyfirst-varies-$c.txt)"
    else
      bad "yarn1-emptyfirst ($c), node $node_major: plain varies beyond the probe race ($why)"; grep '^    ' "$work/probe.txt" | head -20
    fi
  else bad "yarn1-emptyfirst ($c, exit $rc)"; fi
done
# The probe_only check itself, on changed copies of the empty case's plain1.
if [ -d "$work/empty/plain1" ]; then
  pw=$(window empty plain1); pc=$(cachedir empty plain1)
  d=$(copy2 empty probe-a plain1); f=$d/node_modules/y1-empty/data00.txt; t=$(mtime_of "$f")
  if [ $(( t % 1000000 )) -ge 500000 ]; then set_ns "$f" $(( t - 1000 )); else set_ns "$f" $(( t + 1000 )); fi
  if probe_only empty "$d" "$pw" "$pc"; then ok "probe_only accepts an mtime-only difference on a package file"; else bad "probe_only rejected an mtime-only difference ($why)"; fi
  if [ -n "$(groups_other "$f")" ]; then
    chgrp -h "$(groups_other "$f")" "$d/node_modules/y1-empty/data01.txt"
    if ! probe_only empty "$d" "$pw" "$pc" && [ "${why#class gid}" != "$why" ]; then ok "probe_only rejects a gid difference next to it"; else bad "probe_only accepted a gid difference (${why:-accepted})"; fi
  fi
  d=$(copy2 empty probe-b plain1); f=$d/node_modules/.yarn-integrity; set_ns "$f" 1700000000123456789
  if ! probe_only empty "$d" "$pw" "$pc"; then ok "probe_only rejects an mtime difference outside the package ($why)"; else bad "probe_only accepted an mtime difference on .yarn-integrity"; fi
  d=$(copy2 empty probe-c plain1); f=$d/node_modules/y1-empty/data02.txt; t=$(mtime_of "$f"); printf 'y' >> "$f"; set_ns "$f" "$t"
  set_ns "$d/node_modules/y1-empty/data00.txt" $(( $(mtime_of "$d/node_modules/y1-empty/data00.txt") + 1000 ))
  if ! probe_only empty "$d" "$pw" "$pc"; then ok "probe_only rejects a content difference ($why)"; else bad "probe_only accepted a content difference"; fi
  # (the directory keeps its time, so only the missing file tells)
  d=$(copy2 empty probe-d plain1); t=$(mtime_of "$d/node_modules/y1-empty"); rm "$d/node_modules/y1-empty/data03.txt"; set_ns "$d/node_modules/y1-empty" "$t"
  set_ns "$d/node_modules/y1-empty/data00.txt" $(( $(mtime_of "$d/node_modules/y1-empty/data00.txt") + 1000 ))
  if ! probe_only empty "$d" "$pw" "$pc"; then ok "probe_only rejects a package file missing on one side ($why)"; else bad "probe_only accepted a missing package file"; fi
fi

# An extra xattr on a cache file: Node's copy drops it, so plain is identical
# (a clone would keep it; PR 2's engine must remove it).
verify xattr --cache-xattr "$here/fixtures/yarn1-small"
tagged=$(grep -o 'cache xattr: user.matchapack.verify on .*' "$work/out" | sed 's/.* on //')
pkg=${tagged#*/node_modules/}   # <name>/package.json inside the cache entry
if [ $rc = 0 ] && [ -n "$tagged" ] && has_xattr "$work/xattr/cache/$tagged" user.matchapack.verify \
  && ! has_xattr "$work/xattr/plain1/node_modules/$pkg" user.matchapack.verify; then
  ok "cache file with an extra xattr: plain copies drop it, plain vs plain identical"
else bad "cache xattr case (exit $rc, tagged '$tagged')"; fi

# --- What --meta must catch, on copies of yarn1-small's plain2 -----------------
d=$(copy2 small control); meta small "$d"
if [ $rc = 0 ]; then ok "control: an unchanged copy of plain2 passes"; else bad "control copy differs"; fi

other=$(groups_other "$work/small/plain2/node_modules/y1-a/package.json")
if [ -n "$other" ]; then
  d=$(copy2 small gid); chgrp -h "$other" "$d/node_modules/y1-a/package.json"; meta small "$d"
  if [ $rc = 1 ] && [ "$(count gid)" = 1 ]; then ok "a gid difference is reported (class gid)"; else bad "gid difference (exit $rc)"; fi
else echo "SKIP gid difference: the user is in one group only"; fi

# 1 µs, inside the same millisecond (Yarn's times sit just below a
# millisecond boundary, so +1 µs would cross it).
d=$(copy2 small mtime); f=$d/node_modules/y1-c/index.js; t=$(mtime_of "$f")
if [ $(( t % 1000000 )) -ge 500000 ]; then t=$(( t - 1000 )); else t=$(( t + 1000 )); fi
set_ns "$f" "$t"; meta small "$d"
if [ $rc = 1 ] && [ "$(count mtime)" = 1 ]; then ok "a 1 µs mtime difference on a package file is reported (class mtime)"; else bad "sub-millisecond mtime (exit $rc)"; fi

d=$(copy2 small xattr); xattr -w user.matchapack.test 1 "$d/node_modules/y1-c/index.js" 2>/dev/null \
  || python3 -c 'import os,sys; os.setxattr(sys.argv[1], "user.matchapack.test", b"1")' "$d/node_modules/y1-c/index.js"
meta small "$d"
if [ $rc = 1 ] && [ "$(count xattr-names)" = 1 ]; then ok "an extra xattr name is reported (class xattr-names)"; else bad "xattr name (exit $rc)"; fi

d=$(copy2 small dirmode)
# Toggle the group-write bit, so the mode changes whatever the umask made it.
python3 -c 'import os,sys; p=sys.argv[1]; os.chmod(p, (os.stat(p).st_mode & 0o7777) ^ 0o020)' "$d/node_modules/y1-a/lib"
meta small "$d"
if [ $rc = 1 ] && [ "$(count dir-mode)" = 1 ]; then ok "a directory mode difference is reported (class dir-mode)"; else bad "directory mode (exit $rc)"; fi

d=$(copy2 small integrity); f=$d/node_modules/.yarn-integrity; t=$(mtime_of "$f"); printf ' ' >> "$f"; set_ns "$f" "$t"; meta small "$d"
if [ $rc = 1 ] && grep -q 'yarn integrity byte-identical (node_modules/.yarn-integrity): false' "$work/out" && [ "$(count content)" = 1 ] && [ "$(count mtime)" = 0 ]; then
  ok "a .yarn-integrity byte difference is reported (content, and its own line)"
else bad ".yarn-integrity byte (exit $rc)"; fi

# --- The two differences the study's comparison missed -------------------------
# 1. Empty file among the first copies: the study's hook left index.js and
# package.json with the cache time where plain Yarn under Node >= 22 leaves
# the copy time (under Node 20 the other way round). Swap the two on a copy
# of plain1: --meta must report both files, with a warm cache and with each
# run filling its own cache (the times are then all inside the runs).
for c in empty empty-empty; do
  [ -d "$work/$c/plain1" ] || continue
  d=$(copy2 "$c" swap plain1); w=$(window "$c" plain1); cache=$(cachedir "$c" plain1)
  for f in index.js package.json; do
    p=$d/node_modules/y1-empty/$f
    src=$(find "$cache" -path "*/node_modules/y1-empty/$f" | head -1)
    t=$(mtime_of "$p"); s=${w%:*}; e=${w#*:}
    if [ "$t" -ge "$s" ] && [ "$t" -le "$e" ] && [ "$t" != "$(mtime_of "$src")" ]; then set_ns "$p" "$(mtime_of "$src")"; else set_ns "$p" $(( (s + e) / 2 )); fi
  done
  meta_self "$c" "$d"
  if [ $rc = 1 ] && [ "$(count mtime)" = 2 ]; then ok "$c: cache time vs copy time on index.js and package.json is reported (class mtime)"
  else bad "$c: cache time vs copy time (exit $rc)"; fi
done

# 2. File group: plain Yarn under Node >= 22 gives a copied file the CACHE
# file's group; the prototype's clone got the directory's. Install once more
# from a regrouped copy of the same cache (same times) and compare with
# plain1: under Node >= 22 every package file differs in gid, under Node 20
# none does.
if [ -n "$other" ]; then
  g=$work/small-regrouped; rm -rf "${g:?}" "${g:?}-cache"
  cpp "$work/small/cache" "$g-cache"
  chgrp -R "$other" "$g-cache"
  cpp "$here/fixtures/yarn1-small" "$g"
  s=$(python3 -c 'import time; print(time.time_ns())')
  (cd "$g" && env -u GITHUB_TOKEN YARN_DISABLE_SELF_UPDATE_CHECK=true "$node" "$yarnjs" install --frozen-lockfile --non-interactive --cache-folder "$g-cache") > "$g.log" 2>&1
  e=$(python3 -c 'import time; print(time.time_ns())')
  "$bin" diff --meta "$work/small/plain1" "$g" --a-run "$(window small plain1)" --b-run "$s:$e" \
    --a-cache "$work/small/cache" --b-cache "$g-cache" > "$work/out" 2>&1; rc=$?
  n=$(count gid)
  if [ "$node_major" -ge 22 ]; then
    if [ $rc = 1 ] && [ "${n:-0}" -ge 10 ] && [ "$(count mtime)" = 0 ]; then ok "node $node_major, regrouped cache: $n files take the cache's group, reported (class gid)"
    else bad "regrouped cache under node $node_major (exit $rc, gid ${n:-?})"; fi
  else
    if [ $rc = 0 ]; then ok "node $node_major, regrouped cache: copies keep the directory's group, identical"
    else bad "regrouped cache under node $node_major (exit $rc)"; fi
  fi
fi

# --- The harness's own outcomes ------------------------------------------------
# An error is exit 2, never 1 ("they differ"): verify-yarn1.sh must not turn
# an unreadable tree into "plain varies".
"$bin" diff "$work/no-such-a" "$work/no-such-b" > "$work/out" 2>&1; r1=$?
"$bin" diff --meta "$work/small/plain1" "$work/small/plain2" --a-run 2:1 --b-run 1:2 > "$work/out" 2>&1; r2=$?
if [ $r1 = 2 ] && [ $r2 = 2 ]; then ok "matchapack diff errors exit 2 (missing tree, bad run window)"; else bad "diff errors: exit $r1 and $r2, want 2"; fi
# A build output that differs on every run: "plain varies", with the path.
p=$work/varies-src; rm -rf "${p:?}"; mkdir -p "$p"
cp "$here/fixtures/yarn1-scriptpkg/yarn.lock" "$p/"
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); d["scripts"]={"postinstall":"node -e \"require(\x27fs\x27).writeFileSync(\x27node_modules/random.txt\x27, String(Math.random()))\""}; json.dump(d, open(sys.argv[2],"w"), indent=2)' \
  "$here/fixtures/yarn1-scriptpkg/package.json" "$p/package.json"
verify varies "$p"
if [ $rc = 3 ] && grep -q 'PLAIN VARIES' "$work/out" && grep -q 'node_modules/random.txt' "$work/out"; then ok "a build output that differs between plain runs: exit 3, PLAIN VARIES, the path shown"
else bad "plain varies (exit $rc)"; fi

verify engine --engine yarn1 "$here/fixtures/yarn1-small"
if [ $rc = 2 ] && grep -q 'not built yet' "$work/out"; then ok "--engine: refused, no engine exists yet"; else bad "--engine (exit $rc)"; fi
mkdir -p "$work/withnm"; cp "$here/fixtures/yarn1-small/package.json" "$here/fixtures/yarn1-small/yarn.lock" "$work/withnm/"; mkdir -p "$work/withnm/node_modules"
verify withnm "$work/withnm"
if [ $rc = 2 ]; then ok "a source with node_modules is refused"; else bad "source with node_modules (exit $rc)"; fi

if [ "$(ls -l ~/.yarnrc 2>/dev/null || echo absent)" = "$yarnrc_before" ]; then ok "~/.yarnrc untouched"; else bad "~/.yarnrc changed"; fi
echo "passed $pass, failed $fail"
[ $fail = 0 ]
