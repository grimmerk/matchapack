#!/usr/bin/env bash
# Yarn 1 harness and engine tests:   test-yarn1.sh <work-dir>
# Plain Yarn 1 against plain Yarn 1 on the yarn1-* fixtures must come out
# identical under `matchapack diff --meta`, install outputs must be
# classified rather than reported, and --meta must catch each difference it
# exists for: a gid, a sub-millisecond mtime on a package file, an extra xattr
# name, a directory mode, a .yarn-integrity byte, and the two differences
# between plain Yarn and the study's prototype (an empty file among the first
# copies; the file group under Node >= 22). Then the Yarn 1 engine
# (`matchapack yarn`, NOTES "Yarn 1 engine"): identical to plain Yarn on
# yarn1-many, yarn1-emptyfirst (at the default thread pool, where plain
# Yarn's own race is tolerated on the engine side too, and strictly with one
# pool thread; NOTES "The probe race", issue grimmerk/matchapack-private#57), a regrouped cache, a cache with extra
# xattrs, refused clones, failed fix-ups, --production, workspaces, an
# incremental install, umask 002 and (MATCHAPACK_YARN_PRIME_NODE=<node>) a
# cache filled by another Node; install flags and workspace layouts
# (--focus, an install from a workspace directory, nohoist,
# --link-duplicates, --flat, --ignore-optional, --no-bin-links,
# --pure-lockfile, --check-files, --force, --offline, --prefer-offline,
# --ignore-engines, NODE_ENV=production; where plain Yarn refuses, the engine
# must fail the same way); its gates, the start-up probe, the report,
# two volumes (a disk image); Homebrew's yarn launcher, and (MATCHAPACK_FISH=
# <fish>) the yarn function under fish; the sampled verification of logged
# runs (verify-sample.sh --yarn, #77). Needs the release binary, node and Yarn 1
# (MATCHAPACK_YARN_NODE / MATCHAPACK_YARN_JS as in verify-yarn1.sh).
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
# A server another checkout left running answers the tarballs, and may not
# have the registry documents (make-tarballs.py) the Yarn command cases name
# packages by.
if curl -sf -o /dev/null http://127.0.0.1:48321/y1-b-1.0.0.tgz && ! curl -sf -o /dev/null http://127.0.0.1:48321/y1-c; then
  echo "the server on 127.0.0.1:48321 has no registry documents: it serves another checkout's fixtures/optional-upward/tarballs; stop it" >&2; exit 2
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
now_ns() { python3 -c 'import time; print(time.time_ns())'; }
copy2() { local d=$work/$1-$2; rm -rf "${d:?}"; cpp "$work/$1/${3:-plain2}" "$d"; echo "$d"; }
set_ns() { python3 -c 'import os,sys; t=int(sys.argv[2]); os.utime(sys.argv[1], ns=(t, t), follow_symlinks=False)' "$1" "$2"; }
mtime_of() { python3 -c 'import os,sys; print(os.lstat(sys.argv[1]).st_mtime_ns)' "$1"; }
# has_xattr <file> <name>: the macOS xattr tool; os.listxattr on Linux.
if [ "$(uname)" = Darwin ]; then has_xattr() { xattr "$1" 2>/dev/null | grep -qx "$2"; }
else has_xattr() { python3 -c 'import os,sys; sys.exit(0 if sys.argv[2] in os.listxattr(sys.argv[1], follow_symlinks=False) else 1)' "$1" "$2"; }; fi
# probe_only <case> <B dir> <B window> <B cache>: does <case>'s plain1
# differ from <B dir> ONLY the way Yarn's time probe makes two plain runs
# differ (NOTES "Yarn 1 harness", "The probe race")? Mtime differences only
# (at least one), every one on a NON-EMPTY top-level file of the y1-empty
# package; no other class, no path only on one side, every .yarn-integrity
# byte-identical. The race: Yarn's one flag "do copies keep times?" is
# written by every fixTimes call that starts before it is set, and the last
# writer wins; a pool thread paused at the wrong moment leaves one data file
# uncorrected, or the flag at "correct", which then corrects index.js and
# package.json too. Node >= 22.12's copy gives an empty file the cache's
# times, so an empty file's time never depends on the flag: an empty file
# that differs is not the race. Sets $why when not.
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
    [ -s "$work/$1/plain1/$q" ] || { why="an empty file: not the race: $q"; return 1; }
  done < "$work/probe-paths.txt"
  return 0
}
groups_other() { # a group the user is in, other than the one of $1
  local g; g=$(stat -f %g "$1" 2>/dev/null || stat -c %g "$1")
  for x in $(id -G); do [ "$x" != "$g" ] && { echo "$x"; return; }; done
}

# TEST_YARN1_PART=engine runs only the engine's tests (mutation runs);
# TEST_YARN1_PART=route only `matchapack yarn` run directly, corepack, the
# shell route and convert-existing.sh (not the engine's fixture cases);
# TEST_YARN1_PART=commands only the Yarn commands that change dependencies
# (add, upgrade, remove, upgrade-interactive, workspace);
# TEST_YARN1_PART=sample only the sampled verification (verify-sample.sh --yarn).
# --- Sampled verification of real runs (verify-sample.sh --yarn, #77) ---------
# TEST_YARN1_PART=sample: only these. A scratch run log, filled by
# `matchapack yarn` run on fixtures (the logged runs), then
# verify-sample.sh --yarn on it: a replay identical to plain Yarn (a root
# with --frozen-lockfile, a workspace directory without it); a damaged engine
# tree reported as DIFFERENT, or RACE-SHAPED where plain Yarn's own probe race
# (#57) could have made it; a `yarn add` that started with no node_modules
# (part 1b), replayed from the files before it (git HEAD), and damaged; the
# skips (directory gone, yarn.lock changed, the run changed the files and
# they are not in git, a hidden argument, an older record, another Yarn, an
# rc file above the project, a relative --use-yarnrc, an unusable cache,
# upgrade-interactive); the npm sampler leaving `matchapack yarn` records
# alone; the report. Every run of it has its own log and cache folder under
# the work directory, and TMPDIR there.
if [ "${TEST_YARN1_PART:-all}" = sample ] || [ "${TEST_YARN1_PART:-all}" = all ]; then
sp=$work/sp; rm -rf "${sp:?}"; mkdir -p "$sp/log" "$sp/cache" "$sp/tmp"
spenv() {
  env -u GITHUB_TOKEN -u MATCHAPACK_ENGINE -u MATCHAPACK_PATHS -u MATCHAPACK YARN_DISABLE_SELF_UPDATE_CHECK=true \
    PATH="$(dirname "$node"):$PATH" MATCHAPACK_YARN_JS="$yarnjs" MATCHAPACK_STORE="$sp/mp/store" \
    MATCHAPACK_LOG_DIR="$sp/log" MATCHAPACK_CONFIG_DIR="$sp/mp/config" "$@"
}
# sp_project <name> <fixture>: a copy of the fixture's files in $sp/<name>.
sp_project() { local d=$sp/$1; rm -rf "${d:?}"; cp -p -R "$here/fixtures/$2" "$d"; echo "$d"; }
# sp_install <dir> [yarn args]: the logged run, `matchapack yarn` in <dir>.
sp_install() { local d=$1; shift; (cd "$d" && spenv "$bin" yarn "$@" --cache-folder "$sp/cache") > "$work/out" 2>&1; rc=$?; }
# sp_sample [env...] -- [verify-sample.sh args]: output in $work/out, exit in $rc.
sp_sample() {
  local e=()
  while [ $# -gt 0 ] && [ "$1" != -- ]; do e+=("$1"); shift; done
  shift
  env ${e[@]+"${e[@]}"} MATCHAPACK_LOG_DIR_REAL="$sp/log" TMPDIR="$sp/tmp" MATCHAPACK_YARN_NODE= MATCHAPACK_YARN_JS= \
    "$here/scripts/verify-sample.sh" --yarn "$@" > "$work/out" 2>&1; rc=$?
}
# vrec <python expression over r, the last line of verify.jsonl>
vrec() { python3 -c 'import json,sys; r=json.loads(open(sys.argv[1]).read().splitlines()[-1]); print(eval(sys.argv[2]))' "$sp/log/verify.jsonl" "$1" 2>/dev/null; }
# lrec <python expression over r, the last line of the run log>
lrec() { python3 -c 'import json,sys; r=json.loads(open(sys.argv[1]).read().splitlines()[-1]); print(eval(sys.argv[2]))' "$sp/log/log.jsonl" "$1" 2>/dev/null; }
# sp_fake <python statements changing r, a copy of the first logged run>:
# appends that record to the run log (a skip case needs no install).
sp_fake() {
  python3 -c '
import json, sys
p = sys.argv[1]
r = json.loads(open(p).read().splitlines()[0])
exec(sys.argv[2])
open(p, "a").write(json.dumps(r) + "\n")' "$sp/log/log.jsonl" "$1"
}
loglines() { wc -l < "$sp/log/log.jsonl" | tr -d ' '; }

# The logged run, and what its record holds for a replay.
pm=$(sp_project many yarn1-many)
sp_install "$pm" install --frozen-lockfile
pm_real=$(cd "$pm" && pwd -P)
if [ $rc = 0 ] && [ "$(lrec 'r["root"]')" = "$pm_real" ] && [ "$(lrec 'r["replay"]["args"]')" = "['--frozen-lockfile']" ] \
  && [ "$(lrec 'r["yarn1"]["yarn_js"]')" = "$yarnjs" ] && [ "$(lrec 'r["yarn1"]["node"]')" = "$("$node" -v)" ] \
  && [ "$(lrec 'r["files_after"]["yarn.lock"]')" = "$(shasum -a 256 "$pm/yarn.lock" | cut -d' ' -f1)" ] && [ "$(lrec 'r["files_before"] == r["files_after"]')" = True ] \
  && [ "$(lrec 'sorted(r["files_before"])')" = "['package.json', 'yarn.lock']" ] && [ "$(lrec 'r["node_modules_before"]')" = absent ] \
  && [ "$(lrec 'r["replay"]["kind"]')" = install ] && [ -x "$(lrec 'r["yarn1"]["node_path"]')" ]; then
  ok "sample: the log record carries the root, the replay's arguments, Yarn's and node's paths, node_modules and the files' hashes before and after"
else bad "sample: the logged run's record (exit $rc): $(tail -1 "$sp/log/log.jsonl" | head -c 600)"; fi

# 1. Replayed: identical, and nothing written to the run log it read.
before=$(loglines)
sp_sample -- 1
if [ $rc = 0 ] && [ "$(vrec 'r["result"]')" = IDENTICAL ] && [ "$(vrec 'r["dir"]')" = "$pm_real" ] && [ "$(vrec 'r["args"]')" = "['--frozen-lockfile']" ] \
  && [ "$(vrec 'r["cloned"] > 0')" = True ] && [ "$(vrec 'r["yarn"]')" = "$("$node" "$yarnjs" --version)" ] && [ "$(loglines)" = "$before" ]; then
  ok "sample: a logged install replayed: IDENTICAL, $(vrec 'r["cloned"]') files cloned, the run log unchanged"
else bad "sample: identical replay (exit $rc, $(vrec 'r.get("result"), r.get("reason")'))"; fi
# (TMPDIR also gets Yarn's own temporary files.)
if [ -z "$(ls -d "$sp/tmp"/matchapack-verify-yarn.* 2>/dev/null)" ]; then ok "sample: its work directory is removed"; else bad "sample: left $(ls -d "$sp/tmp"/matchapack-verify-yarn.*)"; fi

# 2. A run from a workspace directory, the root package.json ahead of
# yarn.lock, with --pure-lockfile (Yarn installs what package.json asks for
# and leaves yarn.lock as it is): replayed in that directory with that flag
# only (a --frozen-lockfile added by the replay would make Yarn refuse; it
# refuses only for the root's package.json, measured: a workspace's ahead of
# yarn.lock passes --frozen-lockfile in Yarn 1.22.22).
pw=$(sp_project ws yarn1-workspaces)
python3 -c 'import json,sys; p=sys.argv[1]; d=json.load(open(p)); d["dependencies"]["y1-many"]="http://127.0.0.1:48321/y1-many-2.0.0.tgz"; json.dump(d, open(p,"w"), indent=2)' "$pw/package.json"
sp_install "$pw/packages/w2" --pure-lockfile
sp_sample -- 1
if [ $rc = 0 ] && [ "$(vrec 'r["result"]')" = IDENTICAL ] && [ "$(vrec 'r["subdir"]')" = packages/w2 ] && [ "$(vrec 'r["args"]')" = "['--pure-lockfile']" ] \
  && [ "$(vrec 'r["manifests_copied"]')" = 4 ] && [ "$(vrec 'r["cloned"] > 0')" = True ]; then
  ok "sample: from a workspace directory, the root package.json ahead of yarn.lock, --pure-lockfile: replayed there with that flag only, IDENTICAL"
else bad "sample: workspace replay (exit $rc, $(vrec 'r.get("result"), r.get("subdir"), r.get("args"), r.get("manifests_copied"), r.get("reason")'))"; fi

# 3. A damaged engine tree: a content byte; an mtime on a tree without empty
# package files; an mtime on a non-empty package file of a tree with empty
# ones (the race's shape under Node >= 22.12; otherwise DIFFERENT).
sp_sample MATCHAPACK_VERIFY_YARN_DAMAGE='printf x >> node_modules/y1-b/index.js' -- "$pm"
if [ "$(vrec 'r["result"]')" = DIFFERENT ] && [ "$(vrec '"content node_modules/y1-b/index.js" in r["differing"]')" = True ]; then
  ok "sample: a content byte changed in the engine's tree: DIFFERENT, the path recorded"
else bad "sample: content damage ($(vrec 'r.get("result"), r.get("differing"), r.get("reason")'))"; fi
sp_sample MATCHAPACK_VERIFY_YARN_DAMAGE='touch -t 200001010000 node_modules/y1-b/index.js' -- "$pm"
if [ "$(vrec 'r["result"]')" = DIFFERENT ] && [ "$(vrec 'r["differing"]')" = "['mtime node_modules/y1-b/index.js']" ]; then
  ok "sample: an mtime changed in a tree without empty package files: DIFFERENT (not the race)"
else bad "sample: mtime damage without empty files ($(vrec 'r.get("result"), r.get("differing"), r.get("reason")'))"; fi
pe=$(sp_project empty yarn1-emptyfirst)
sp_install "$pe" --frozen-lockfile
# One pool thread: plain Yarn's own race cannot happen (NOTES "The probe
# race"), so the damage is the only difference.
sp_sample UV_THREADPOOL_SIZE=1 MATCHAPACK_VERIFY_YARN_DAMAGE='touch -t 200001010000 node_modules/y1-empty/index.js' MATCHAPACK_VERIFY_KEEP=1 -- "$pe"
want=DIFFERENT; "$node" -e 'const [a,b]=process.versions.node.split(".").map(Number); process.exit(a>22||(a===22&&b>=12)?0:1)' && want=RACE-SHAPED
if [ "$(vrec 'r["result"]')" = $want ] && [ "$(vrec '"mtime node_modules/y1-empty/index.js" in r["differing"]')" = True ]; then
  ok "sample: an mtime on a non-empty package file next to empty ones: $want (node $("$node" -v))"
else bad "sample: race-shaped damage, want $want ($(vrec 'r.get("result"), r.get("differing"), r.get("reason")'))"; fi
# The race rule on that kept tree, called directly: an empty file's mtime,
# a content difference, .yarn-integrity, a path outside node_modules, or
# Node before 22.12 is not the race.
kept=$(sed -n 's/^  kept: //p' "$work/out" | tail -1)
if [ -d "$kept/v/engine" ]; then
  r=$(python3 -I -B -c '
import importlib.util, sys
s = importlib.util.spec_from_file_location("vs", sys.argv[1]); vs = importlib.util.module_from_spec(s); s.loader.exec_module(vs)
t = sys.argv[2]
import os
empty = next(os.path.relpath(os.path.join(d, f), t) for d, _, fs in os.walk(os.path.join(t, "node_modules/y1-empty")) for f in fs if os.path.getsize(os.path.join(d, f)) == 0)
cases = [
  ([("mtime", "node_modules/y1-empty/index.js")], "v24.22.1", True),
  ([("mtime", "node_modules/y1-empty/index.js")], "v22.11.0", False),
  ([("mtime", empty)], "v24.22.1", False),
  ([("content", "node_modules/y1-empty/index.js")], "v24.22.1", False),
  ([("mtime", "node_modules/.yarn-integrity")], "v24.22.1", False),
  ([("mtime", "package.json")], "v24.22.1", False),
  ([("mtime", "node_modules/y1-empty/index.js"), ("gid", "node_modules/y1-empty/package.json")], "v24.22.1", False),
  ([], "v24.22.1", False),
]
print(" ".join("%s" % (vs.race_shaped(o, t, n) == want) for o, n, want in cases))' "$here/scripts/verify-sample.py" "$kept/v/engine")
  if [ "$r" = "True True True True True True True True" ]; then ok "sample: the race rule rejects an empty file, other classes, .yarn-integrity, files outside node_modules, Node < 22.12"
  else bad "sample: race rule cases: $r"; fi
  rm -rf "${kept:?}"
else bad "sample: no kept work directory for the race rule ($kept)"; fi

# skip <label> <expected reason text> [verify-sample.sh args]: SKIPPED with that reason.
skip() { # skip <label> <expected reason text> [verify-sample.sh args]
  local label=$1 want=$2; shift 2
  sp_sample -- "$@"
  local got; got=$(vrec 'r["result"] + ": " + r.get("reason", "")')
  case $got in "SKIPPED: "*"$want"*) ok "sample: skipped, $label ($got)" ;; *) bad "sample: $label: got '$got', exit $rc" ;; esac
}
# 3b. A command that started with no node_modules (#77 part 1b): `yarn add`
# in a fresh copy, which changes package.json and yarn.lock; replayed from
# the files before it, which are the project's git HEAD.
pg=$(sp_project gitadd yarn1-small); pg=$(cd "$pg" && pwd -P)
git -C "$pg" init -q && git -C "$pg" add package.json yarn.lock \
  && git -C "$pg" -c user.name=t -c user.email=t@example.invalid -c commit.gpgsign=false commit -q -m fixture
sp_install "$pg" add http://127.0.0.1:48321/y1-many-1.0.0.tgz
lr=$(lrec '(r["command"], r["node_modules_before"], r["replay"], r["files_before"]["package.json"] != r["files_after"]["package.json"])')
sp_sample -- "$pg"
if [ "$lr" = "('add', 'absent', {'kind': 'command', 'args': ['add', 'http://127.0.0.1:48321/y1-many-1.0.0.tgz']}, True)" ] \
  && [ "$(vrec '(r["result"], r["kind"], r["command"], r["replayed_from"])')" = "('IDENTICAL', 'started-empty', 'add', 'git HEAD')" ] && [ "$(vrec 'r["cloned"] > 0')" = True ]; then
  ok "sample: a yarn add that started with no node_modules, replayed from the files before it (git HEAD): IDENTICAL, $(vrec 'r["cloned"]') cloned"
else bad "sample: started-empty yarn add (logged $lr; $(vrec 'r.get("result"), r.get("reason"), r.get("detail")'))"; fi
sp_sample MATCHAPACK_VERIFY_YARN_DAMAGE='printf x >> node_modules/y1-many/package.json' -- "$pg"
if [ "$(vrec '(r["result"], "content node_modules/y1-many/package.json" in r["differing"])')" = "('DIFFERENT', True)" ]; then
  ok "sample: the started-empty yarn add with a damaged engine tree: DIFFERENT"
else bad "sample: damaged started-empty yarn add ($(vrec 'r.get("result"), r.get("differing"), r.get("reason")'))"; fi
# Without git the files before the run are gone: skipped; and a file changed
# after the run is named as such.
pn=$(sp_project nogitadd yarn1-small); pn=$(cd "$pn" && pwd -P)
sp_install "$pn" add http://127.0.0.1:48321/y1-many-1.0.0.tgz
skip "the run changed the files, no git" "the run changed package.json, yarn.lock, and the files before it are not in the project's git HEAD" "$pn"
printf '\n' >> "$pn/package.json"
skip "the project changed since, no git" "package.json changed since the run, and the files before it are not" "$pn"
# git HEAD is not the state before the run (package.json was edited before
# it, not committed): its files do not have the logged hashes.
pd=$(sp_project gitold yarn1-small); pd=$(cd "$pd" && pwd -P)
git -C "$pd" init -q && git -C "$pd" add package.json yarn.lock \
  && git -C "$pd" -c user.name=t -c user.email=t@example.invalid -c commit.gpgsign=false commit -q -m fixture
python3 -c 'import json,sys; p=sys.argv[1]; d=json.load(open(p)); d["description"]="edited"; json.dump(d, open(p,"w"), indent=2)' "$pd/package.json"
sp_install "$pd" add http://127.0.0.1:48321/y1-many-1.0.0.tgz
skip "git HEAD older than the run" "the run changed package.json, yarn.lock, and the files before it are not in the project's git HEAD" "$pd"

# 4. Skips, each recorded with its reason.
printf '\n' >> "$pm/yarn.lock"
skip "yarn.lock changed" "yarn.lock changed since the run" "$pm"
gone=$sp/gone; sp_fake "r['root'] = r['dir'] = '$gone'"
skip "directory gone" "the project directory is gone" "$gone"
pk=$(sp_project secret yarn1-many); pk=$(cd "$pk" && pwd -P)
fake_at() { # fake_at <dir> <python statements>: a record for <dir>, its files' hashes as they are now
  sp_fake "import hashlib
r['root'] = r['dir'] = '$1'
r['files_before'] = r['files_after'] = {n: hashlib.sha256(open('$1/' + n, 'rb').read()).hexdigest() for n in ('yarn.lock', 'package.json')}
$2"
}
fake_at "$pk" "r['replay']['args'] = ['--otp', '***']"
skip "a hidden argument" "an argument was hidden in the log" "$pk"
fake_at "$pk" "r['replay']['args'] = ['--use-yarnrc', 'rc/x']"
skip "a relative --use-yarnrc" "--use-yarnrc rc/x is relative" "$pk"
fake_at "$pk" "del r['root']"
skip "a record from before replays were recorded" "the log record has no root" "$pk"
fake_at "$pk" "r['yarn'] = '1.22.0'"
skip "another Yarn now" "the run used 1.22.0" "$pk"
fake_at "$pk" "r['yarn1']['node'] = 'v1.0.0'"
skip "another node now" "the run used v1.0.0" "$pk"
fake_at "$pk" "r['yarn1']['cache'] = 'unusable: on another volume'"
skip "an unusable cache" "not usable in the run (on another volume)" "$pk"
mkdir -p "$sp/above"; pa=$sp/above/p; cp -p -R "$here/fixtures/yarn1-many" "$pa"; pa=$(cd "$pa" && pwd -P); echo 'registry=http://127.0.0.1:1/' > "$sp/above/.npmrc"
fake_at "$pa" ""
skip "an .npmrc above the project" "$sp/above/.npmrc, above the project" "$pa"
skip "a directory with no logged install" "no replayable \`matchapack yarn\` run of this directory" "$here/fixtures/yarn1-small"
fake_at "$pk" "r['command'] = 'upgrade-interactive'; r['replay'] = {'kind': 'command', 'args': ['upgrade-interactive']}"
skip "upgrade-interactive" "the choices made in its prompt are not logged" "$pk"

# 5. Picking: N distinct projects, newest first; a command that started
# with node_modules there is not replayed.
sp_fake "r['command'] = 'add'; r['node_modules_before'] = 'present'; r['root'] = r['dir'] = '$sp/added'"
n0=$(wc -l < "$sp/log/verify.jsonl")
sp_sample -- 2
picked=$(tail -n +$((n0 + 1)) "$sp/log/verify.jsonl" | python3 -c 'import json,sys; print(" ".join(json.loads(l)["dir"].rsplit("/", 1)[-1] for l in sys.stdin))')
if [ "$picked" = "secret p" ]; then ok "sample: N=2 picks the two most recent distinct projects with a replayable run, not a command over node_modules ($picked)"
else bad "sample: picked '$picked'"; fi

# 6. The npm sampler leaves `matchapack yarn` records alone (one without a
# command, as Yarn logged them before the commands were routed).
mkdir -p "$sp/npmlog"; head -1 "$sp/log/log.jsonl" | python3 -c 'import json,sys; r=json.load(sys.stdin); r.pop("command"); print(json.dumps(r))' > "$sp/npmlog/log.jsonl"
MATCHAPACK_LOG_DIR_REAL="$sp/npmlog" "$here/scripts/verify-sample.sh" 1 > "$work/out" 2>&1; rc=$?
if [ $rc = 0 ] && grep -q '^nothing to verify' "$work/out" && [ ! -e "$sp/npmlog/verify.jsonl" ]; then ok "sample: the npm sampler does not take a matchapack yarn record for an npm ci"
else bad "sample: npm sampler on a yarn record (exit $rc)"; fi

# 7. The report shows the Yarn samples beside the npm ones.
MATCHAPACK_LOG_DIR="$sp/log" "$bin" report > "$work/out" 2>&1
if grep -q '^verification samples (npm): none yet' "$work/out" && grep -Eq '^verification samples \(yarn\): [0-9]+ run: identical [1-9]' "$work/out" \
  && grep -Eq '^verification samples \(yarn\): .*skipped [1-9]' "$work/out" && grep -Eq '^  .* SKIPPED +/' "$work/out" \
  && grep -Eq '^  .* SKIPPED +/.*\(yarn upgrade-interactive, started empty; ' "$work/out"; then ok "sample: matchapack report shows the Yarn samples"
else bad "sample: report"; fi
fi # TEST_YARN1_PART sample
if [ "${TEST_YARN1_PART:-all}" = sample ]; then
  if [ "$(ls -l ~/.yarnrc 2>/dev/null || echo absent)" = "$yarnrc_before" ]; then ok "~/.yarnrc untouched"; else bad "~/.yarnrc changed"; fi
  echo "passed $pass, failed $fail"; [ $fail = 0 ]; exit
fi

if [ "${TEST_YARN1_PART:-all}" = all ]; then
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
  # The race never moves an empty file's time (Node's copy gives it the
  # cache's): one moved next to the data file's is a real difference.
  d=$(copy2 empty probe-e plain1); f=$d/node_modules/y1-empty/e000.empty; t=$(mtime_of "$f")
  set_ns "$d/node_modules/y1-empty/data00.txt" $(( $(mtime_of "$d/node_modules/y1-empty/data00.txt") + 1000 ))
  set_ns "$f" $(( t + 1000 ))
  if ! probe_only empty "$d" "$pw" "$pc" && [ "${why#an empty file}" != "$why" ]; then ok "probe_only rejects an mtime difference on an empty package file ($why)"
  else bad "probe_only accepted an mtime difference on an empty file (${why:-accepted})"; fi
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
# (a clone would keep it; the engine removes it: its xattr case below).
verify xattr --cache-xattr "$here/fixtures/yarn1-small"
tagged=$(grep -o 'cache xattr: user.matchapack.verify on .*' "$work/out" | sed 's/.*, first //')
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

fi # TEST_YARN1_PART

verify engine-bogus --engine bogus "$here/fixtures/yarn1-small"
if [ $rc = 2 ]; then ok "--engine with an unknown name is refused"; else bad "--engine bogus (exit $rc)"; fi

# --- The Yarn 1 engine (`matchapack yarn`, NOTES "Yarn 1 engine") --------------
# Each case: plain, plain, engine on fresh copies, same cache (verify-yarn1.sh
# --engine yarn1); the engine must have installed and cloned, and differ from
# plain1 nowhere, or only where the two plain runs differ too.
# ejson <case> <python expression over r (the engine's log line), y = r["yarn1"]>
ejson() { python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); y=r.get("yarn1") or {}; print(eval(sys.argv[2]))' "$work/$1/engine.json" "$2" 2>/dev/null; }
copied() { ejson "$1" "y.get('copied', {}).get('$2', 0)"; }
everify() { local c=$1; shift; verify "$c" --engine yarn1 "$@"; }
same() { grep -q 'plain vs engine: IDENTICAL' "$work/out"; }
# first128 <case>: Yarn's first copies were left to Node, all 128 of them
# (Yarn's copy concurrency; the hook reads it from the bundle). Checked by
# count: leaving none to Node changes the tree only in some runs (issue grimmerk/matchapack-private#57,
# NOTES "The probe race"), the count always.
first128() { [ "$(ejson "$1" "y['first']")" = 128 ] && [ "$(copied "$1" first-copies)" = 128 ]; }

if [ "${TEST_YARN1_PART:-all}" != route ]; then
if [ "${TEST_YARN1_PART:-all}" != commands ]; then
everify e-many "$here/fixtures/yarn1-many"
n=$(ejson e-many "y['cloned']")
if [ $rc = 0 ] && same && [ "${n:-0}" -ge 150 ] && first128 e-many \
  && grep -q "engine: shared $n of" "$work/out"; then
  ok "yarn1-many: engine identical to plain (mtimes, gid, xattr names included); $n files cloned and sharing blocks, Yarn's first $(ejson e-many "y['first']") copies left to Node"
else bad "yarn1-many engine (exit $rc, cloned ${n:-?})"; fi

# Yarn hands its lifecycle scripts npm_execpath = its main module; under the
# engine that must still be Yarn's bin/yarn.js (y1-execpath records it and
# runs `node "$npm_execpath" --version`; compared above as file content).
x=$work/e-many/engine/node_modules/y1-execpath/execpath.txt
if [ -f "$x" ] && [ "$(head -1 "$x")" = "$yarnjs" ] && cmp -s "$x" "$work/e-many/plain1/node_modules/y1-execpath/execpath.txt"; then
  ok "lifecycle scripts get npm_execpath = Yarn's own bin/yarn.js, and it runs Yarn"
else bad "npm_execpath under the engine ($(head -1 "$x" 2>/dev/null))"; fi

# An install script that names a file after its own directory, as node-gyp's
# .deps/<absolute path>/... (issue grimmerk/matchapack-private#48): each tree gets another path, so two
# plain runs differ by path when compared as they are. verify-yarn1.sh
# compares with --normalize-roots, so those paths match in all three trees.
# What remains is content: Yarn lists the script's outputs (with the tree's
# path) in .yarn-integrity, which then differs between the plain runs too:
# "differs only where plain varies" (exit 3), never ENGINE DIFFERS.
everify e-rootpath "$here/fixtures/yarn1-rootpath"
raw=$("$bin" diff "$work/e-rootpath/plain1" "$work/e-rootpath/plain2" --paths 2>/dev/null | grep -c '^node_modules/y1-rootpath/built/')
m=$work/e-rootpath/engine/node_modules/y1-rootpath/built$work/e-rootpath/engine/node_modules/y1-rootpath/marker
want=$(printf 'content\tnode_modules/.yarn-integrity')
if [ $rc = 3 ] && grep -q 'plain vs engine: differs only where plain varies' "$work/out" && [ "${raw:-0}" -gt 0 ] && [ -f "$m" ] \
  && [ "$(cat "$work/e-rootpath/classes-plain.txt")" = "$want" ] && [ "$(cat "$work/e-rootpath/classes-engine.txt")" = "$want" ]; then
  ok "a file named after its own tree's path: $raw paths differ between the plain runs as they are, none with the roots normalised; engine as plain (.yarn-integrity content only)"
else bad "file named after its tree's path (exit $rc, raw plain differences ${raw:-?})"; fi

# package.json out of sync with yarn.lock: yarn1-small plus a dependency the
# lockfile lacks (y1-many: enough files that the engine clones some), with
# node_modules/keepme already there. Not y1-empty: its empty files flip
# Yarn's time probe, so two PLAIN runs already differ in mtimes (measured
# here 2026-11-14: 2 of 3 plain pairs; NOTES "Yarn 1 harness"). Plain Yarn
# and `matchapack yarn` on separate copies (cp -p of one source, so files
# outside node_modules keep the same times), one scratch cache.
sy=$work/sync; rm -rf "${sy:?}"; mkdir -p "$sy/src/node_modules/keepme"
cp -p "$here/fixtures/yarn1-small/yarn.lock" "$sy/src/"
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); d["dependencies"]["y1-many"]="http://127.0.0.1:48321/y1-many-1.0.0.tgz"; json.dump(d, open(sys.argv[2],"w"), indent=2)' \
  "$here/fixtures/yarn1-small/package.json" "$sy/src/package.json"
# sync_run <name> plain|engine [yarn args]: a fresh copy $sy/<name>, one
# install; exit status in $rc, START:END in $win, output in $sy/<name>.log.
sync_run() {
  local d=$sy/$1 how=$2 s; shift 2
  cpp "$sy/src" "$d"; s=$(now_ns)
  if [ "$how" = plain ]; then
    (cd "$d" && env -u GITHUB_TOKEN YARN_DISABLE_SELF_UPDATE_CHECK=true \
      "$node" "$yarnjs" install --non-interactive --cache-folder "$sy/cache" "$@") > "$d.log" 2>&1; rc=$?
  else
    (cd "$d" && env -u GITHUB_TOKEN -u MATCHAPACK_ENGINE -u MATCHAPACK_PATHS -u MATCHAPACK \
      YARN_DISABLE_SELF_UPDATE_CHECK=true PATH="$(dirname "$node"):$PATH" MATCHAPACK_YARN_JS="$yarnjs" \
      MATCHAPACK_STORE="$sy/mp/store" MATCHAPACK_LOG_DIR="$sy/mp/log" MATCHAPACK_CONFIG_DIR="$sy/mp/config" MATCHAPACK_NO_FALLBACK=1 \
      "$bin" yarn install --non-interactive --cache-folder "$sy/cache" "$@") > "$d.log" 2>&1; rc=$?
  fi
  win=$s:$(now_ns)
}
# srec <python expression over r (the engine's last log line), y = r["yarn1"]>
srec() { python3 -c 'import json,sys; r=json.loads(open(sys.argv[1]).read().splitlines()[-1]); y=r.get("yarn1") or {}; print(eval(sys.argv[2]))' "$sy/mp/log/log.jsonl" "$1" 2>/dev/null; }
# 1. --frozen-lockfile: Yarn refuses before it fetches or links anything.
sync_run f-plain plain --frozen-lockfile; rp=$rc
sync_run f-engine engine --frozen-lockfile; re=$rc
why=
for t in f-plain f-engine; do
  grep -q 'Your lockfile needs to be updated' "$sy/$t.log" || why="$why $t:no-error"
  cmp -s "$sy/src/yarn.lock" "$sy/$t/yarn.lock" || why="$why $t:lock-changed"
  [ -d "$sy/$t/node_modules/keepme" ] || why="$why $t:keepme-gone"
  [ ! -e "$sy/$t/node_modules/.yarn-integrity" ] || why="$why $t:integrity-written"
done
rec=$(srec '(r["outcome"], r.get("code"), y.get("link_steps"), y.get("cloned"), y.get("linked"))')
if [ "$rp" != 0 ] && [ "$re" = "$rp" ] && [ -z "$why" ] && [ "$rec" = "('yarn_failed', $re, 0, 0, False)" ]; then
  ok "package.json ahead of yarn.lock, --frozen-lockfile: plain and engine both exit $rp with Yarn's error; lock unchanged, node_modules untouched; logged yarn_failed, no link step, nothing cloned"
else bad "out-of-sync --frozen-lockfile (plain $rp, engine $re,$why; record $rec)"; fi
# 2. A plain install: Yarn updates the lockfile and installs. A first run
# fills the cache, so the compared runs both take every file from a warm
# cache and the harness's --meta comparison applies.
sync_run b-prime plain; r0=$rc
sync_run b-plain plain; rp=$rc wp=$win
sync_run b-engine engine; re=$rc we=$win
n=$(srec 'y["cloned"]')
"$bin" diff --meta --normalize-roots "$sy/b-plain" "$sy/b-engine" --a-run "$wp" --b-run "$we" --a-cache "$sy/cache" --b-cache "$sy/cache" > "$sy/b-diff.txt" 2>&1; rd=$?
if [ "$r0" = 0 ] && [ "$rp" = 0 ] && [ "$re" = 0 ] && ! cmp -s "$sy/src/yarn.lock" "$sy/b-plain/yarn.lock" && cmp -s "$sy/b-plain/yarn.lock" "$sy/b-engine/yarn.lock" \
  && grep -q 'y1-many-1.0.0.tgz#' "$sy/b-engine/yarn.lock" && [ "$rd" = 0 ] && [ "$(srec 'r["outcome"]')" = installed ] && [ "${n:-0}" -gt 0 ]; then
  ok "package.json ahead of yarn.lock, plain install: both update yarn.lock the same way; trees identical (diff --meta); $n files cloned"
else bad "out-of-sync install (prime $r0, plain $rp, engine $re, diff $rd, cloned ${n:-?})"; sed 's/^/    /' "$sy/b-diff.txt" | grep -v '^      ' | tail -25; fi
# 3. --pure-lockfile (the cache is warm from 2): Yarn installs what
# package.json asks for and does not write yarn.lock. Yarn's behaviour; the
# engine passes the flag on.
sync_run p-plain plain --pure-lockfile; rp=$rc wp=$win
sync_run p-engine engine --pure-lockfile; re=$rc we=$win
n=$(srec 'y["cloned"]')
"$bin" diff --meta --normalize-roots "$sy/p-plain" "$sy/p-engine" --a-run "$wp" --b-run "$we" --a-cache "$sy/cache" --b-cache "$sy/cache" > "$sy/p-diff.txt" 2>&1; rd=$?
if [ "$rp" = 0 ] && [ "$re" = 0 ] && cmp -s "$sy/src/yarn.lock" "$sy/p-plain/yarn.lock" && cmp -s "$sy/src/yarn.lock" "$sy/p-engine/yarn.lock" \
  && [ -f "$sy/p-engine/node_modules/y1-many/index.js" ] && [ "$rd" = 0 ] && [ "${n:-0}" -gt 0 ]; then
  ok "package.json ahead of yarn.lock, --pure-lockfile: both install y1-many and leave yarn.lock as it was; trees identical (diff --meta); $n files cloned"
else bad "out-of-sync --pure-lockfile (plain $rp, engine $re, diff $rd, cloned ${n:-?})"; sed 's/^/    /' "$sy/p-diff.txt" | grep -v '^      ' | tail -25; fi

# A slow answer (the server sleeps between a clone and its fix-up, longer
# than the hook waits): later copies go to Node, but the slow request is
# answered, never copied over while its clone is being fixed up.
MATCHAPACK_YARN_SLOW_FIXUP_EVERY=40 MATCHAPACK_YARN_SLOW_FIXUP_MS=3000 MATCHAPACK_YARN_TIMEOUT_MS=500 everify e-slow "$here/fixtures/yarn1-many"
# (Yarn's other copies are already in flight when the first slow answer is
# due, so usually none is left for Node: the slow ones themselves must end as
# clones, and the tree as plain's. Copying over them instead leaves the
# late fix-up's times on Node's copies: red.)
n=$(ejson e-slow "y['copied_total'] - y['first']")
if [ $rc = 0 ] && same && [ "$(ejson e-slow "y['cloned']")" -gt 0 ] && [ "$n" = "$(copied e-slow server-slow)" ]; then ok "slow answers: each slow request waits for its own clone, tree identical"
else bad "slow answers (exit $rc, copied beyond the first: ${n:-?})"; fi

# The harness itself: an engine difference is plain's variation only on the
# same path in the same class.
printf 'mtime\tnode_modules/a/x.js\n' > "$work/cp.txt"
printf 'content\tnode_modules/a/x.js\nmtime\tnode_modules/a/x.js\n' > "$work/ce.txt"
own=$("$here/scripts/yarn1-own-diffs.sh" "$work/cp.txt" "$work/ce.txt")
if [ "$own" = "$(printf 'content\tnode_modules/a/x.js')" ]; then ok "a content difference is the engine's even where plain varies by mtime"
else bad "attribution by class (got '$own')"; fi
# An unreadable or missing classes file is an error, never "nothing of its own".
"$here/scripts/yarn1-own-diffs.sh" "$work/no-such-classes.txt" "$work/ce.txt" > "$work/out" 2>&1; r1=$?
cp "$work/cp.txt" "$work/cp-locked.txt"; chmod 000 "$work/cp-locked.txt"
"$here/scripts/yarn1-own-diffs.sh" "$work/cp-locked.txt" "$work/ce.txt" > "$work/out2" 2>&1; r2=$?
chmod 644 "$work/cp-locked.txt"
if [ $r1 = 2 ] && [ $r2 = 2 ]; then ok "attribution: a missing or unreadable classes file fails (exit 2)"
else bad "attribution on an unreadable file (exit $r1, $r2)"; fi
# --engine-min-clones 0 means none.
verify e-min0 --engine yarn1 --engine-min-clones 0 "$here/fixtures/yarn1-many"
if [ $rc = 1 ] && grep -q 'expected none' "$work/out"; then ok "--engine-min-clones 0 fails a run that cloned"; else bad "--engine-min-clones 0 (exit $rc)"; fi
# A relative --cache-dir is the caller's, not one per tree.
(cd "$work" && "$v" --work "$work/e-rel" --cache-dir rel-cache --engine yarn1 "$here/fixtures/yarn1-many" > "$work/out" 2>&1); rc=$?
if [ $rc = 0 ] && [ -d "$work/rel-cache/v6" ] && [ ! -e "$work/e-rel/plain1/rel-cache" ]; then ok "a relative --cache-dir is resolved once"; else bad "relative --cache-dir (exit $rc)"; fi
# A --cache-dir inside the work directory is refused, never cleaned up.
mkdir -p "$work/e-in/cache"; touch "$work/e-in/cache/keep"
verify e-in --cache-dir "$work/e-in/cache" "$here/fixtures/yarn1-many"
if [ $rc = 2 ] && [ -f "$work/e-in/cache/keep" ]; then ok "a --cache-dir inside --work is refused and left alone"; else bad "--cache-dir inside --work (exit $rc)"; fi

# yarn1-emptyfirst at the default thread pool: plain Yarn's own race (NOTES
# "The probe race"; issue grimmerk/matchapack-private#57) can hit the engine's run as well as a plain
# one, so the engine may differ from plain1 the way two plain runs can.
# emptyfirst_engine_verdict <case> <exit> [<engine tree>] [<plain2 tree>]:
# the verdict on such a case, whose harness output is in $work/out. Passes
# on exit 0; under Node >= 22 on exit 3 with the engine identical to plain1
# or differing only where plain varies, when plain's own variation is the
# race's (probe_only on plain2, as in the plain cases above: plain varying
# on an empty file is not the race either); and on exit 1 when the harness reached the comparison
# (ENGINE DIFFERS) and the engine tree differs from plain1 only the way the
# race does (probe_only: mtimes of non-empty package files). The evidence is
# kept as emptyfirst-engine-race-<case>.txt. Sets $verdict. The strict
# one-thread cases below have no tolerance: with one pool thread the race
# cannot happen.
emptyfirst_engine_verdict() {
  local c=$1 r=$2 tree=${3:-$work/$1/engine} p2=${4:-$work/$1/plain2}
  verdict="exit $r"
  if [ "$r" = 0 ] && grep -q 'plain vs engine: IDENTICAL' "$work/out"; then verdict="plain vs engine: IDENTICAL"; return 0; fi
  [ "$node_major" -ge 22 ] || return 1
  if [ "$r" = 3 ] && grep -Eq 'plain vs engine: (IDENTICAL|differs only where plain varies)' "$work/out"; then
    if ! probe_only "$c" "$p2" "$(window "$c" plain2)" "$(cachedir "$c" plain2)"; then
      verdict="plain varies beyond the probe race ($why)"; return 1
    fi
    verdict=$(grep -o 'plain vs engine: [a-zA-Z ]*' "$work/out" | head -1); return 0
  fi
  [ "$r" = 1 ] && grep -q 'plain vs engine: ENGINE DIFFERS' "$work/out" || return 1
  if probe_only "$c" "$tree" "$(window "$c" engine)" "$(cachedir "$c" engine)"; then
    # (evidence of a real run only: not for a self-test's changed copy)
    [ -z "${3:-}" ] && cp "$work/$c/diff-engine.txt" "$work/emptyfirst-engine-race-$c.txt" 2>/dev/null
    verdict="the probe race on the engine side: mtimes of non-empty package files only ($work/emptyfirst-engine-race-$c.txt)"; return 0
  fi
  verdict="the engine differs beyond the probe race ($why)"; return 1
}
for c in empty empty-empty; do
  if [ $c = empty ]; then everify e-$c "$here/fixtures/yarn1-emptyfirst"; else everify e-$c --cache empty "$here/fixtures/yarn1-emptyfirst"; fi
  n=$(ejson e-$c "y['cloned']")
  if emptyfirst_engine_verdict e-$c $rc && [ "${n:-0}" -gt 0 ] && first128 e-$c; then
    ok "yarn1-emptyfirst ($c), node $node_major: engine as plain ($verdict); $n files cloned, Yarn's first 128 copies left to Node"
  else bad "yarn1-emptyfirst engine ($c, exit $rc: $verdict; first $(ejson e-$c "y['first']"), first-copies $(copied e-$c first-copies))"; grep '^    ' "$work/probe.txt" 2>/dev/null | head -10; fi
done
# The verdict itself, on changed copies of e-empty's engine tree, with the
# harness's ENGINE DIFFERS: a data file's mtime moved is the race (the
# "forced race": the branch runs here even when the real race did not); an
# empty file's mtime, a group, a content byte or .yarn-integrity's time is not.
if [ -d "$work/e-empty/engine" ] && [ "$node_major" -ge 22 ]; then
  out_kept=$work/out-e-empty; cp "$work/out" "$out_kept"
  # forced <name> <change (sh, $d is the copy)>: the verdict on a copy with that change.
  forced() {
    d=$(copy2 e-empty "$1" engine); (cd "$d" && eval "$2")
    printf 'plain vs engine: ENGINE DIFFERS (forced)\n' > "$work/out"
    emptyfirst_engine_verdict e-empty 1 "$d"
  }
  nudge='f=node_modules/y1-empty/data05.txt; t=$(mtime_of "$f"); if [ $(( t % 1000000 )) -ge 500000 ]; then set_ns "$f" $(( t - 1000 )); else set_ns "$f" $(( t + 1000 )); fi'
  if forced race-a "$nudge"; then ok "engine-side verdict: a data file's mtime moved (a forced race) is accepted as the race"
  else bad "engine-side verdict rejected a forced race ($verdict)"; fi
  if ! forced race-b "$nudge"'; set_ns node_modules/y1-empty/e007.empty $(( $(mtime_of node_modules/y1-empty/e007.empty) + 1000 ))'; then ok "engine-side verdict: an empty file's mtime is not the race ($verdict)"
  else bad "engine-side verdict accepted an empty file's mtime"; fi
  g=$(groups_other "$work/e-empty/engine/node_modules/y1-empty/data06.txt")
  if [ -n "$g" ]; then
    if ! forced race-c "$nudge"'; chgrp -h '"$g"' node_modules/y1-empty/data06.txt'; then ok "engine-side verdict: a group change is not the race ($verdict)"
    else bad "engine-side verdict accepted a group change"; fi
  fi
  if ! forced race-d "$nudge"'; f=node_modules/y1-empty/data07.txt; t=$(mtime_of "$f"); printf y >> "$f"; set_ns "$f" "$t"'; then ok "engine-side verdict: a content change is not the race ($verdict)"
  else bad "engine-side verdict accepted a content change"; fi
  if ! forced race-e "$nudge"'; set_ns node_modules/.yarn-integrity 1700000000123456789'; then ok "engine-side verdict: .yarn-integrity's mtime is not the race ($verdict)"
  else bad "engine-side verdict accepted .yarn-integrity's mtime"; fi
  # Exit 3 ("differs only where plain varies") with plain varying on an empty
  # file: not the race on either side.
  d=$(copy2 e-empty race-f plain2); (cd "$d" && set_ns node_modules/y1-empty/e011.empty $(( $(mtime_of node_modules/y1-empty/e011.empty) + 1000 )))
  printf 'plain vs plain: PLAIN VARIES (forced)\nplain vs engine: differs only where plain varies (forced)\n' > "$work/out"
  if ! emptyfirst_engine_verdict e-empty 3 "" "$d" && [ "${verdict#plain varies beyond}" != "$verdict" ]; then ok "engine-side verdict: exit 3 with plain varying on an empty file is not the race ($verdict)"
  else bad "engine-side verdict accepted exit 3 with plain varying on an empty file ($verdict)"; fi
  # Exit 1 for another reason than the comparison (here: no clones) is never the race.
  printf 'engine: cloned 0 files, expected at least 1\n' > "$work/out"
  if ! emptyfirst_engine_verdict e-empty 1 "$work/e-empty-race-a"; then ok "engine-side verdict: an exit 1 that is not ENGINE DIFFERS is not the race"
  else bad "engine-side verdict accepted an exit 1 without ENGINE DIFFERS"; fi
  cp "$out_kept" "$work/out"
fi

# The same fixture with one pool thread (UV_THREADPOOL_SIZE=1, which reaches
# plain Yarn and the engine alike): libuv then finishes each operation in
# the order it was queued, the last writer of Yarn's flag is always the 128th
# copy (an empty file), and the race cannot happen. Strict: no tolerance of
# any kind. These two cases guard the clones' times (an empty clone's, a
# non-empty clone's), which the default-pool cases above cannot tell from
# the race.
for c in empty-uv1 empty-empty-uv1; do
  if [ $c = empty-uv1 ]; then UV_THREADPOOL_SIZE=1 everify e-$c "$here/fixtures/yarn1-emptyfirst"; else UV_THREADPOOL_SIZE=1 everify e-$c --cache empty "$here/fixtures/yarn1-emptyfirst"; fi
  n=$(ejson e-$c "y['cloned']")
  if [ $rc = 0 ] && grep -q 'plain vs plain: IDENTICAL' "$work/out" && same && [ "${n:-0}" -gt 0 ] && first128 e-$c; then
    ok "yarn1-emptyfirst ($c), one pool thread, node $node_major: plain vs plain and plain vs engine identical, no tolerance; $n files cloned"
  else bad "yarn1-emptyfirst one pool thread ($c, exit $rc, cloned ${n:-?}, first $(ejson e-$c "y['first']"), first-copies $(copied e-$c first-copies))"; fi
done

# The group: under Node >= 22 Node's copy takes the cache file's group,
# which the engine sets with lchown; under Node 20 it does not, and neither
# does the engine.
other=$(groups_other "$work")
if [ -n "$other" ]; then
  everify e-group --cache-group "$other" "$here/fixtures/yarn1-many"
  g=$(ejson e-group "y['facts']['group']")
  want=True; [ "$node_major" -ge 22 ] || want=False
  if [ $rc = 0 ] && same && [ "$g" = "$want" ]; then ok "regrouped cache, node $node_major: Node's copy takes the cache's group: $g; engine identical"
  else bad "regrouped cache engine (exit $rc, facts.group $g)"; fi
else echo "SKIP regrouped cache: the user is in one group only"; fi

everify e-xattr --cache-xattr "$here/fixtures/yarn1-many"
if [ $rc = 0 ] && same && [ "$(ejson e-xattr "y['cloned']")" -gt 0 ]; then ok "every cache file with an extra xattr: the clones drop it as Node's copy does"
else bad "xattr engine (exit $rc)"; fi

MATCHAPACK_YARN_FAIL_EVERY=7 everify e-refuse "$here/fixtures/yarn1-many"
n=$(copied e-refuse test-refused)
if [ $rc = 0 ] && same && [ "${n:-0}" -gt 0 ]; then ok "every 7th clone refused: $n files copied by Node instead, tree identical"
else bad "every 7th clone refused (exit $rc, refused ${n:-?})"; fi

# The clone is made, then its fix-up fails (before the xattrs are removed):
# the clone must be removed, so Node's copy, which opens and truncates what
# is there, starts from no file (with the clone left, its xattr would stay).
MATCHAPACK_YARN_FAIL_FIXUP_EVERY=5 everify e-fixup --cache-xattr "$here/fixtures/yarn1-many"
n=$(copied e-fixup fixup-failed)
if [ $rc = 0 ] && same && [ "${n:-0}" -gt 0 ] && grep -q "engine: shared $(ejson e-fixup "y['cloned']") of" "$work/out"; then
  ok "clone made, fix-up failed ($n files): ends as Node's copy, tree identical, only the clones share blocks"
else bad "fix-up failure (exit $rc, failed ${n:-?})"; fi

everify e-prod "$here/fixtures/yarn1-many" --production
if [ $rc = 0 ] && same && [ ! -e "$work/e-prod/engine/node_modules/y1-script" ] && [ "$(ejson e-prod "y['cloned']")" -gt 0 ]; then ok "--production: engine identical"
else bad "--production engine (exit $rc)"; fi

everify e-ws "$here/fixtures/yarn1-workspaces"
if [ $rc = 0 ] && same && [ -L "$work/e-ws/engine/node_modules/w1" ] && [ -d "$work/e-ws/engine/packages/w1/node_modules/y1-b" ] && [ "$(ejson e-ws "y['cloned']")" -gt 0 ]; then
  ok "workspaces (links, a nested node_modules in a workspace): engine identical"
else bad "workspaces engine (exit $rc)"; fi

everify e-incr --then "$here/fixtures/yarn1-many/v2" "$here/fixtures/yarn1-many"
if [ $rc = 0 ] && same && [ "$(ejson e-incr "y['cloned']")" -gt 0 ] && [ -f "$work/e-incr/engine/node_modules/y1-many/lib/new.js" ] && [ ! -e "$work/e-incr/engine/node_modules/y1-many/lib/d14" ]; then
  ok "incremental v1 -> v2 over the tree: engine identical; $(ejson e-incr "y['cloned']") files cloned in the second install"
else bad "incremental engine (exit $rc)"; fi

# (verify sets rc inside the subshell: pass it out)
(umask 002; everify e-umask "$here/fixtures/yarn1-many"; exit $rc); rc=$?
if [ $rc = 0 ] && same; then ok "umask 002: engine identical"; else bad "umask 002 engine (exit $rc)"; fi

# The cache filled by another Node major (MATCHAPACK_YARN_PRIME_NODE): Yarn's
# times pass through two Node versions' conversions.
if [ -n "${MATCHAPACK_YARN_PRIME_NODE:-}" ]; then
  everify e-cross --prime-node "$MATCHAPACK_YARN_PRIME_NODE" "$here/fixtures/yarn1-many"
  if [ $rc = 0 ] && same; then ok "cache filled by node $("$MATCHAPACK_YARN_PRIME_NODE" -v), installed by node $node_major: engine identical"
  else bad "cross-node engine (exit $rc)"; fi
else echo "SKIP cache filled by another Node (set MATCHAPACK_YARN_PRIME_NODE)"; fi

# --- Install flags and workspace layouts (each: plain vs engine, same flags)
# Every flag goes to Yarn as given; the engine decides only where the
# project is (the nearest yarn.lock at or above the working directory) and
# which destinations it clones (inside that project, below a node_modules).
# Where a flag makes plain Yarn fail, the engine must fail the same way.

# `yarn install` in a workspace directory: Yarn installs at the workspace
# root, and so does the engine (the root is the lockfile folder above).
# As many clones as the same install run from the root (e-ws).
everify e-wsub --subdir packages/w2 "$here/fixtures/yarn1-workspaces"
n=$(ejson e-wsub "y['cloned']")
if [ $rc = 0 ] && same && [ -f "$work/e-wsub/engine/node_modules/.yarn-integrity" ] && [ -L "$work/e-wsub/engine/node_modules/w2" ] \
  && [ "${n:-0}" -gt 0 ] && [ "$n" = "$(ejson e-ws "y['cloned']")" ] && [ "$(ejson e-wsub "r['dir']")" = "$work/e-wsub/engine/packages/w2" ]; then
  ok "yarn install in a workspace directory: installed at the workspace root, engine identical; $n files cloned, as from the root"
else bad "install in a workspace directory (exit $rc, cloned ${n:-?}, from the root $(ejson e-ws "y['cloned']"))"; fi

# `file:` dependencies (fixtures/yarn1-file): a tarball and a directory in
# the project, each a package of 308 files (two of them empty; more than
# Yarn's first 128 copies, so the engine clones some). Yarn takes both
# through its cache: the tarball into an entry keyed by its sha1, the
# directory into a new entry on every install. One pool thread: with
# empty and non-empty files among Yarn's first copies, plain Yarn's own
# probe race (#57) makes even two plain runs differ now and then at the
# default pool (measured: 5 of 14 runs of the tarball case, the mtime of
# one non-empty file; 0 of 16 with one thread). The tarball case is
# strict; the directory case also accepts plain Yarn's own variation
# (exit 3, the engine differing only where plain does), which Yarn's copy
# of the directory into its cache was seen to have on a second install.
for kind in tgz dir; do
  fsrc=$work/file-$kind-src; rm -rf "${fsrc:?}"; mkdir -p "$fsrc"
  cp -p "$here/fixtures/yarn1-file/$kind/package.json" "$here/fixtures/yarn1-file/$kind/yarn.lock" "$fsrc/"
  python3 -I "$here/fixtures/yarn1-file/make.py" "$kind" "$fsrc"
  UV_THREADPOOL_SIZE=1 everify e-file-$kind "$fsrc"
  n=$(ejson e-file-$kind "y['cloned']")
  if [ $kind = tgz ]; then verdict_ok() { [ $rc = 0 ] && same; }
  else verdict_ok() { { [ $rc = 0 ] && same; } || { [ $rc = 3 ] && grep -Eq 'plain vs engine: (IDENTICAL|differs only where plain varies)' "$work/out"; }; }; fi
  if verdict_ok && [ "${n:-0}" -gt 0 ] && [ -x "$work/e-file-$kind/engine/node_modules/local-$kind/bin/cli.js" ] \
    && [ -L "$work/e-file-$kind/engine/node_modules/.bin/local-$kind-cli" ]; then
    ok "a file: $kind dependency: engine as plain ($(grep -o 'plain vs engine: [a-zA-Z ]*' "$work/out" | head -1)); $n files cloned"
  else bad "file: $kind dependency (exit $rc, cloned ${n:-?})"; fi
done

# --focus in a workspace: its dependencies are installed (hoisted to the
# root's node_modules), its sibling workspace y1-c as a copy from the
# "registry" (the lockfile's tarball) into the workspace's own node_modules.
everify e-focus --subdir packages/app "$here/fixtures/yarn1-focus" --focus
n=$(ejson e-focus "y['cloned']"); fc=$work/e-focus/engine
if [ $rc = 0 ] && same && [ "${n:-0}" -ge 150 ] && [ -d "$fc/packages/app/node_modules/y1-c" ] && [ ! -L "$fc/packages/app/node_modules/y1-c" ] \
  && [ -f "$fc/packages/app/node_modules/y1-c/data/x.json" ] && [ -f "$fc/node_modules/y1-many/index.js" ]; then
  ok "--focus in a workspace: the sibling workspace comes from the lockfile's tarball, not a link; engine identical; $n files cloned"
else bad "--focus in a workspace (exit $rc, cloned ${n:-?})"; fi

# flag_run <name> <source dir> plain|engine [yarn args]: one install of a
# fresh copy of <source dir> at $fl/<name>; exit in $rc, output in
# $fl/<name>.log. For the cases where Yarn itself refuses.
fl=$work/flags; rm -rf "${fl:?}"; mkdir -p "$fl"
flag_run() {
  local d=$fl/$1 src=$2 how=$3; shift 3
  cpp "$src" "$d"
  if [ "$how" = plain ]; then
    (cd "$d" && env -u GITHUB_TOKEN YARN_DISABLE_SELF_UPDATE_CHECK=true \
      "$node" "$yarnjs" install --frozen-lockfile --non-interactive --cache-folder "$work/e-many/cache" "$@") > "$d.log" 2>&1 < /dev/null; rc=$?
  else
    (cd "$d" && env -u GITHUB_TOKEN -u MATCHAPACK_ENGINE -u MATCHAPACK_PATHS -u MATCHAPACK \
      YARN_DISABLE_SELF_UPDATE_CHECK=true PATH="$(dirname "$node"):$PATH" MATCHAPACK_YARN_JS="$yarnjs" \
      MATCHAPACK_STORE="$fl/mp/store" MATCHAPACK_LOG_DIR="$fl/mp/log" MATCHAPACK_CONFIG_DIR="$fl/mp/config" MATCHAPACK_NO_FALLBACK=1 \
      "$bin" yarn install --frozen-lockfile --non-interactive --cache-folder "$work/e-many/cache" "$@") > "$d.log" 2>&1 < /dev/null; rc=$?
  fi
}
frec() { python3 -c 'import json,sys; r=json.loads(open(sys.argv[1]).read().splitlines()[-1]); y=r.get("yarn1") or {}; print(eval(sys.argv[2]))' "$fl/mp/log/log.jsonl" "$1" 2>/dev/null; }
# refused <name> <source dir> <Yarn's error text> [yarn args]: plain Yarn
# fails with that error and installs nothing; the engine fails with the
# same status and error, logged yarn_failed with no link step.
refused() {
  local name=$1 src=$2 want=$3; shift 3
  flag_run "$name-plain" "$src" plain "$@"; local rp=$rc
  flag_run "$name-engine" "$src" engine "$@"; local re=$rc
  local why= t
  for t in "$name-plain" "$name-engine"; do
    grep -qF "$want" "$fl/$t.log" || why="$why $t:no-error"
    [ ! -e "$fl/$t/node_modules/.yarn-integrity" ] || why="$why $t:installed"
  done
  local rec; rec=$(frec '(r["outcome"], r.get("code"), y.get("link_steps"), y.get("cloned"))')
  if [ "$rp" != 0 ] && [ "$re" = "$rp" ] && [ -z "$why" ] && [ "$rec" = "('yarn_failed', $re, 0, 0)" ]; then
    ok "$name: plain and engine both exit $rp with Yarn's error ($want); nothing installed; logged yarn_failed, no link step"
  else bad "$name (plain $rp, engine $re,$why; record $rec)"; fi
}
# --focus in the workspace root: Yarn refuses it.
fr=$fl/focus-root-src; cpp "$here/fixtures/yarn1-focus" "$fr"
refused "--focus in the workspace root" "$fr" "can only be run inside an individual workspace" --focus

# nohoist: y1-many stays in each workspace's own node_modules (two copies).
everify e-nohoist "$here/fixtures/yarn1-nohoist"
n=$(ejson e-nohoist "y['cloned']"); nh=$work/e-nohoist/engine
if [ $rc = 0 ] && same && [ "${n:-0}" -ge 400 ] && [ -f "$nh/packages/w1/node_modules/y1-many/index.js" ] && [ -f "$nh/packages/w2/node_modules/y1-many/index.js" ] \
  && [ ! -e "$nh/node_modules/y1-many" ]; then
  ok "nohoist: y1-many in each workspace's node_modules, none at the root; engine identical; $n files cloned"
else bad "nohoist (exit $rc, cloned ${n:-?})"; fi

# --link-duplicates: Yarn copies the first of two identical packages and
# hardlinks the other's files to it (fs.link, not a copy; the engine only
# answers copies). matchapack diff does not compare link counts: the groups
# of paths sharing an inode are compared here, in each tree.
# links <tree>: one line per inode with more than one link, its paths.
links() { python3 -c '
import os, sys
root = sys.argv[1]; groups = {}
for d, dirs, files in os.walk(root):
    for f in files:
        p = os.path.join(d, f); st = os.lstat(p)
        if st.st_nlink > 1: groups.setdefault(st.st_ino, []).append(os.path.relpath(p, root))
for g in sorted(" ".join(sorted(v)) for v in groups.values()): print(g)
' "$1"; }
everify e-linkdup "$here/fixtures/yarn1-nohoist" --link-duplicates
n=$(ejson e-linkdup "y['cloned']")
links "$work/e-linkdup/plain1" > "$work/links-plain1.txt"; links "$work/e-linkdup/plain2" > "$work/links-plain2.txt"; links "$work/e-linkdup/engine" > "$work/links-engine.txt"
g=$(wc -l < "$work/links-engine.txt" | tr -d ' ')
if [ $rc = 0 ] && same && [ "${n:-0}" -gt 0 ] && [ "$g" -ge 300 ] && cmp -s "$work/links-plain1.txt" "$work/links-plain2.txt" && cmp -s "$work/links-plain1.txt" "$work/links-engine.txt" \
  && ! grep -qv ' ' "$work/links-engine.txt"; then
  ok "--link-duplicates: the same $g pairs of hardlinked files as plain Yarn; engine identical; $n files cloned (the copies the links point at)"
else bad "--link-duplicates (exit $rc, cloned ${n:-?}, link groups $g; $work/links-*.txt)"; fi

# --flat: one version per package. With resolutions for every package that
# has two versions it installs flat; without, it asks, and with no TTY
# (--non-interactive) Yarn fails.
everify e-flat "$here/fixtures/yarn1-flat" --flat
n=$(ejson e-flat "y['cloned']")
if [ $rc = 0 ] && same && [ "${n:-0}" -gt 0 ] && [ ! -e "$work/e-flat/engine/node_modules/y1-a/node_modules" ] && grep -q '"2.0.0"' "$work/e-flat/engine/node_modules/y1-b/package.json" \
  && cmp -s "$here/fixtures/yarn1-flat/package.json" "$work/e-flat/engine/package.json"; then
  ok "--flat with resolutions: no nested node_modules, package.json rewritten with the same bytes; engine identical; $n files cloned"
else bad "--flat with resolutions (exit $rc, cloned ${n:-?})"; fi
refused "--flat with two versions and no resolution" "$here/fixtures/yarn1-many" "Can't answer a question unless a user TTY" --flat

# yarn1-many with @y1/scoped an optional dependency instead of a dev one
# (the lockfile is the same).
fo=$fl/optional-src; mkdir -p "$fo"; cp -p "$here/fixtures/yarn1-many/yarn.lock" "$fo/"
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); d["optionalDependencies"]={"@y1/scoped": d["devDependencies"].pop("@y1/scoped")}; json.dump(d, open(sys.argv[2],"w"), indent=2)' \
  "$here/fixtures/yarn1-many/package.json" "$fo/package.json"
everify e-noopt "$fo" --ignore-optional
if [ $rc = 0 ] && same && [ "$(ejson e-noopt "y['cloned']")" -gt 0 ] && [ ! -e "$work/e-noopt/engine/node_modules/@y1" ] && [ -f "$work/e-noopt/engine/node_modules/y1-many/index.js" ]; then
  ok "--ignore-optional: the optional dependency left out; engine identical; $(ejson e-noopt "y['cloned']") files cloned"
else bad "--ignore-optional (exit $rc)"; fi

everify e-nobin "$here/fixtures/yarn1-many" --no-bin-links
if [ $rc = 0 ] && same && [ "$(ejson e-nobin "y['cloned']")" -gt 0 ] && [ ! -e "$work/e-nobin/engine/node_modules/.bin" ] && [ -e "$work/e-many/engine/node_modules/.bin" ]; then
  ok "--no-bin-links: no node_modules/.bin; engine identical; $(ejson e-nobin "y['cloned']") files cloned"
else bad "--no-bin-links (exit $rc)"; fi

# --check-files: a second install after part of a package was removed
# (300 files: more than Yarn's first copies, which Node makes). Without the
# flag Yarn's integrity check says up to date and puts nothing back.
everify e-check --then "$here/fixtures/yarn1-many" --before-then 'rm -r node_modules/y1-many/lib' "$here/fixtures/yarn1-many" --check-files
n=$(ejson e-check "y['cloned']")
if [ $rc = 0 ] && same && [ "${n:-0}" -gt 0 ] && [ "$(find "$work/e-check/engine/node_modules/y1-many/lib" -type f | wc -l | tr -d ' ')" = 300 ]; then
  ok "--check-files after node_modules/y1-many/lib was removed: put back; engine identical; $n files cloned in the second install"
else bad "--check-files (exit $rc, cloned ${n:-?})"; fi

# --force: a second install over a complete tree. Yarn runs its link step
# again, and skips each file whose size and mtime already match the cache
# file (its copy queue), so nothing is copied and nothing cloned.
verify e-force --engine yarn1 --engine-min-clones 0 --then "$here/fixtures/yarn1-many" "$here/fixtures/yarn1-many" --force
if [ $rc = 0 ] && same && [ "$(ejson e-force "y['linked']")" = True ] && [ "$(ejson e-force "y['link_steps']")" -ge 1 ] && [ "$(ejson e-force "y['copied_total']")" = 0 ]; then
  ok "--force over a complete tree: Yarn links again and copies nothing; engine identical, 0 files cloned"
else bad "--force (exit $rc, linked $(ejson e-force "y['linked']"), copied $(ejson e-force "y['copied_total']"))"; fi

# --offline / --prefer-offline with a warm cache (e-many's, so the prime run
# also finds everything there).
for f in offline prefer-offline; do
  everify e-$f --cache-dir "$work/e-many/cache" "$here/fixtures/yarn1-many" --$f
  if [ $rc = 0 ] && same && [ "$(ejson e-$f "y['cloned']")" -gt 0 ]; then ok "--$f, warm cache: engine identical; $(ejson e-$f "y['cloned']") files cloned"
  else bad "--$f (exit $rc)"; fi
done

# --ignore-engines: the project asks for node >= 999 (a root engines field,
# which Yarn checks; "<1" would not do: Yarn reads an engines range below 1
# as 0.10-0.13 of the running major).
fe=$fl/engines-src; mkdir -p "$fe"; cp -p "$here/fixtures/yarn1-many/yarn.lock" "$fe/"
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); d["engines"]={"node":">=999"}; json.dump(d, open(sys.argv[2],"w"), indent=2)' \
  "$here/fixtures/yarn1-many/package.json" "$fe/package.json"
refused "engines: node >=999" "$fe" 'The engine "node" is incompatible with this module'
everify e-engines "$fe" --ignore-engines
if [ $rc = 0 ] && same && [ "$(ejson e-engines "y['cloned']")" -gt 0 ]; then ok "--ignore-engines: installs; engine identical; $(ejson e-engines "y['cloned']") files cloned"
else bad "--ignore-engines (exit $rc)"; fi

# NODE_ENV=production in the environment: as --production.
NODE_ENV=production everify e-nodeenv "$here/fixtures/yarn1-many"
if [ $rc = 0 ] && same && [ ! -e "$work/e-nodeenv/engine/node_modules/y1-script" ] && [ "$(ejson e-nodeenv "y['cloned']")" -gt 0 ]; then
  ok "NODE_ENV=production: no devDependencies; engine identical; $(ejson e-nodeenv "y['cloned']") files cloned"
else bad "NODE_ENV=production (exit $rc)"; fi

# A run that cloned nothing is a failure of the harness, not a pass.
MATCHAPACK_YARN_HOOK=none everify e-nohook "$here/fixtures/yarn1-many"
if [ $rc = 1 ] && grep -q 'engine: cloned 0 files, expected at least 1' "$work/out"; then ok "harness: an engine run that cloned nothing fails"
else bad "harness on a run that cloned nothing (exit $rc)"; fi

fi # TEST_YARN1_PART != commands

# --- Yarn commands that change dependencies (NOTES "Yarn 1 commands") --------
# Each: plain Yarn installs every tree, then runs the command (plain Yarn in
# the prime run and in plain1 and plain2, `matchapack yarn` in the engine
# tree); the trees, package.json, yarn.lock and .yarn-integrity included,
# must be identical, the exit status the same, and the engine's log line
# the command's. yarn1-upgrade names its dependencies by range, with the
# local tarball server as the registry (make-tarballs.py writes a registry
# document per package).
T=http://127.0.0.1:48321
# cverify <case> '<words>' [verify-yarn1 options...] <fixture>
cverify() { local c=$1 w=$2; shift 2; verify "$c" --engine yarn1 --command "$w" "$@"; }
said() { grep -qF -- "$1" "$work/out"; }
# said_line <line>: the harness printed exactly this line.
said_line() { grep -qxF -- "$1" "$work/out"; }
# both_changed <manifest list>: the command changed these package.json files
# and yarn.lock, the same bytes in plain1 and the engine.
both_changed() {
  said ": exit 0 in every run" && said_line "command: changed package.json: $1" && said_line "command: changed yarn.lock: yes" \
    && said "command: package.json byte-identical (plain1, engine): true" && said "command: yarn.lock byte-identical (plain1, engine): true"
}
# pj <case> <tree> <python expression over d, the tree's package.json>
pj() { python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(eval(sys.argv[2]))' "$work/$1/$2/package.json" "$3" 2>/dev/null; }

cverify c-add "add $T/y1-many-1.0.0.tgz" "$here/fixtures/yarn1-small"
n=$(ejson c-add "y['cloned']")
if [ $rc = 0 ] && same && both_changed package.json && [ "${n:-0}" -ge 150 ] && first128 c-add && [ "$(ejson c-add "r['command']")" = add ]; then
  ok "yarn add <a package of 302 files>: engine identical (package.json, yarn.lock, .yarn-integrity, tree); $n files cloned, Yarn's first 128 copies left to Node; logged as add"
else bad "yarn add (exit $rc, cloned ${n:-?})"; fi

cverify c-add-dev "add y1-c --dev --exact" --engine-min-clones 0 "$here/fixtures/yarn1-upgrade"
if [ $rc = 0 ] && same && both_changed package.json && [ "$(pj c-add-dev engine "d['devDependencies'].get('y1-c')")" = 1.0.0 ]; then
  ok "yarn add y1-c --dev --exact (by name, from the registry document): engine identical; devDependencies y1-c 1.0.0; 3 files, all among Yarn's first copies"
else bad "yarn add --dev --exact (exit $rc)"; fi

cverify c-add-wsdir "add $T/y1-many-2.0.0.tgz" --subdir packages/w2 "$here/fixtures/yarn1-workspaces"
n=$(ejson c-add-wsdir "y['cloned']")
if [ $rc = 0 ] && same && both_changed packages/w2/package.json && [ "${n:-0}" -gt 285 ]; then
  ok "yarn add in a workspace directory: the workspace's package.json and the root's yarn.lock; engine identical; $n files cloned (Yarn moved the root's y1-many 1.0.0 too)"
else bad "yarn add in a workspace directory (exit $rc, cloned ${n:-?})"; fi

# Yarn refuses an add in the workspace root without -W: the same status
# and nothing changed; logged yarn_failed, no link step.
cverify c-add-root "add $T/y1-c-1.0.0.tgz" --engine-min-clones 0 "$here/fixtures/yarn1-workspaces"
if [ $rc = 0 ] && grep -Eq 'command: yarn .*: exit [1-9][0-9]* in every run' "$work/out" && said "command: changed package.json: none" && said "command: changed yarn.lock: no" \
  && grep -q 'Running this command will add the dependency to the workspace root' "$work/c-add-root/engine.log" \
  && [ "$(ejson c-add-root "(r['outcome'], y.get('link_steps'))")" = "('yarn_failed', 0)" ]; then
  ok "yarn add in the workspace root without -W: plain and engine both refuse ($(grep -o 'exit [0-9]* in every run' "$work/out")); nothing changed; logged yarn_failed, no link step"
else bad "yarn add in the workspace root (exit $rc)"; fi

cverify c-upgrade "upgrade" "$here/fixtures/yarn1-upgrade"
n=$(ejson c-upgrade "y['cloned']")
if [ $rc = 0 ] && same && said "command: changed package.json: none" && said "command: changed yarn.lock: yes" && said "command: yarn.lock byte-identical (plain1, engine): true" \
  && [ "${n:-0}" -ge 150 ] && first128 c-upgrade && grep -q '"2.0.0"' "$work/c-upgrade/engine/node_modules/y1-many/package.json"; then
  ok "yarn upgrade: y1-many 1.0.0 -> 2.0.0 within its range; yarn.lock rewritten, package.json not; engine identical; $n files cloned"
else bad "yarn upgrade (exit $rc, cloned ${n:-?})"; fi

cverify c-upgrade-latest "upgrade y1-b --latest" --engine-min-clones 0 "$here/fixtures/yarn1-upgrade"
if [ $rc = 0 ] && same && both_changed package.json && [ "$(pj c-upgrade-latest engine "d['devDependencies']['y1-b']")" = '^2.0.0' ]; then
  ok "yarn upgrade y1-b --latest: its range rewritten to ^2.0.0; engine identical"
else bad "yarn upgrade --latest (exit $rc)"; fi

cverify c-add-setup "add $T/y1-many-1.0.0.tgz" --command-setup engine "$here/fixtures/yarn1-small"
if [ $rc = 0 ] && same && both_changed package.json && [ "$(ejson c-add-setup "y['cloned']")" -ge 150 ]; then
  ok "yarn add over a tree the engine installed: identical to the all-plain trees"
else bad "yarn add after an engine install (exit $rc)"; fi

cverify c-add-xattr "add $T/y1-many-1.0.0.tgz" --cache-xattr "$here/fixtures/yarn1-small"
if [ $rc = 0 ] && same && [ "$(ejson c-add-xattr "y['cloned']")" -ge 150 ]; then
  ok "yarn add with an extra xattr on every cache file: the add's clones drop it as Node's copy does"
else bad "yarn add, cache with xattrs (exit $rc)"; fi

cverify c-remove "remove y1-many" --engine-min-clones 0 "$here/fixtures/yarn1-many"
if [ $rc = 0 ] && same && both_changed package.json && [ ! -e "$work/c-remove/engine/node_modules/y1-many" ]; then
  ok "yarn remove y1-many: engine identical (nothing to copy, nothing cloned)"
else bad "yarn remove (exit $rc)"; fi

# `yarn workspace <name> add …`: Yarn's workspace command starts a second
# Yarn in the workspace's directory; the hook starts it with itself and
# relays its requests. y1-execpath records npm_execpath as that Yarn gives
# it (its bundle, lib/cli.js), compared as file content.
cverify c-ws-add "workspace w2 add $T/y1-many-2.0.0.tgz $T/y1-execpath-1.0.0.tgz" "$here/fixtures/yarn1-workspaces"
n=$(ejson c-ws-add "y['cloned']"); x=$work/c-ws-add/engine/node_modules/y1-execpath/execpath.txt
if [ $rc = 0 ] && same && both_changed packages/w2/package.json && [ "${n:-0}" -gt 285 ] && first128 c-ws-add && [ "$(ejson c-ws-add "r['command']")" = "workspace add" ] \
  && [ "$(head -1 "$x" 2>/dev/null)" = "$(dirname "$(dirname "$yarnjs")")/lib/cli.js" ]; then
  ok "yarn workspace w2 add: the workspace command's own Yarn ran with the hook; engine identical; $n files cloned; npm_execpath is Yarn's bundle, as plain"
else bad "yarn workspace add (exit $rc, cloned ${n:-?}, execpath $(head -1 "$x" 2>/dev/null))"; fi

# The reverse, over the tree that add left (2.0.0 at the root, 1.0.0 in
# w1's node_modules; a fresh install from its lockfile lays it out the
# other way): removing w2's y1-many 2.0.0 makes Yarn copy y1-many 1.0.0 to
# the root again, 302 files (measured: new inodes). A remove that clones.
cverify c-ws-remove "workspace w2 remove y1-many" --command-before "workspace w2 add $T/y1-many-2.0.0.tgz" "$here/fixtures/yarn1-workspaces"
n=$(ejson c-ws-remove "y['cloned']")
if [ $rc = 0 ] && same && said ": exit 0 in every run" && said "command: package.json byte-identical (plain1, engine): true" && said "command: yarn.lock byte-identical (plain1, engine): true" \
  && [ "${n:-0}" -ge 150 ] && [ "$(ejson c-ws-remove "r['command']")" = "workspace remove" ] && [ -f "$work/c-ws-remove/engine/node_modules/y1-many/lib/d14/f00.js" ]; then
  ok "yarn workspace w2 remove: Yarn moves the other version back to the root; engine identical; $n files cloned"
else bad "yarn workspace remove (exit $rc, cloned ${n:-?})"; fi

# The workspace's Yarn declines (its probe only): it runs as plain Yarn in
# that process, the command succeeds, nothing is cloned, the reason logged.
MATCHAPACK_YARN_SIMULATE=child-copy-keeps-times cverify c-ws-decline "workspace w2 add $T/y1-many-2.0.0.tgz" --engine-min-clones 0 "$here/fixtures/yarn1-workspaces"
if [ $rc = 0 ] && same && both_changed packages/w2/package.json && [ "$(ejson c-ws-decline "r['outcome']")" = installed ] \
  && ejson c-ws-decline "y.get('declined') or ''" | grep -q "in the workspace's Yarn: .*kept a non-empty file's mtime" \
  && [ "$(ejson c-ws-decline "y['unseen']")" = False ] && grep -q "matchapack: the workspace's Yarn ran as plain Yarn, without clones" "$work/c-ws-decline/engine.log" \
  && ! grep -q 'never saw its link step' "$work/c-ws-decline/engine.log"; then
  ok "a workspace command's Yarn whose hook declines runs as plain Yarn: command done, engine identical, nothing cloned, the reason logged"
else bad "workspace child decline (exit $rc, declined $(ejson c-ws-decline "y.get('declined')"))"; fi

# The same when the workspace's hook fails outside its decline path (an
# exception before Yarn starts: MATCHAPACK_YARN_SIMULATE=child-throw): plain
# Yarn in that process, and the reason reaches the log (not "unseen").
MATCHAPACK_YARN_SIMULATE=child-throw cverify c-ws-throw "workspace w2 add $T/y1-many-2.0.0.tgz" --engine-min-clones 0 "$here/fixtures/yarn1-workspaces"
if [ $rc = 0 ] && same && both_changed packages/w2/package.json && [ "$(ejson c-ws-throw "r['outcome']")" = installed ] \
  && ejson c-ws-throw "y.get('declined') or ''" | grep -q "in the workspace's Yarn: an error before Yarn started: simulated" \
  && [ "$(ejson c-ws-throw "y['unseen']")" = False ] && grep -q "matchapack: the workspace's Yarn ran as plain Yarn, without clones" "$work/c-ws-throw/engine.log" \
  && ! grep -q 'never saw its link step' "$work/c-ws-throw/engine.log"; then
  ok "a workspace command's Yarn whose hook throws before Yarn starts runs as plain Yarn: command done, engine identical, the error logged as its decline"
else bad "workspace child error before Yarn (exit $rc, declined $(ejson c-ws-throw "y.get('declined')"))"; fi

# upgrade-interactive, through a pseudo-terminal that takes every update:
# one Yarn process, one link step per dependency type (y1-many in
# dependencies, y1-b in devDependencies); Yarn's first 128 copies of the
# process left to Node, counted once.
cverify c-ui "upgrade-interactive --latest" --command-tty "$here/fixtures/yarn1-upgrade"
n=$(ejson c-ui "y['cloned']")
if [ $rc = 0 ] && same && both_changed package.json && [ "$(ejson c-ui "y['link_steps']")" -ge 2 ] && first128 c-ui && [ "${n:-0}" -ge 150 ] \
  && [ "$(pj c-ui engine "(d['dependencies']['y1-many'], d['devDependencies']['y1-b'])")" = "('>=2.0.0', '^2.0.0')" ]; then
  ok "yarn upgrade-interactive --latest, every update taken (pseudo-terminal): $(ejson c-ui "y['link_steps']") link steps in one process; engine identical; $n files cloned"
else bad "yarn upgrade-interactive (exit $rc, cloned ${n:-?}, link steps $(ejson c-ui "y['link_steps']"), first-copies $(copied c-ui first-copies))"; tail -5 "$work/c-ui/engine.log" | sed 's/^/    /'; fi

# The same in one workspace (yarn1-upgrade-ws: workspace u1 has
# yarn1-upgrade's dependencies): `yarn workspace u1 upgrade-interactive`,
# whose prompt is the workspace command's own Yarn's.
cverify c-ws-ui "workspace u1 upgrade-interactive --latest" --command-tty "$here/fixtures/yarn1-upgrade-ws"
n=$(ejson c-ws-ui "y['cloned']")
if [ $rc = 0 ] && same && both_changed packages/u1/package.json && [ "$(ejson c-ws-ui "y['link_steps']")" -ge 2 ] && first128 c-ws-ui && [ "${n:-0}" -ge 150 ] \
  && [ "$(ejson c-ws-ui "r['command']")" = "workspace upgrade-interactive" ]; then
  ok "yarn workspace u1 upgrade-interactive --latest (pseudo-terminal): the workspace's Yarn prompts and links with the hook; engine identical; $n files cloned"
else bad "yarn workspace upgrade-interactive (exit $rc, cloned ${n:-?}, link steps $(ejson c-ws-ui "y['link_steps']"))"; tail -5 "$work/c-ws-ui/engine.log" | sed 's/^/    /'; fi

# `yarn workspace u1 upgrade` (yarn1-upgrade-ws): the workspace command's
# own Yarn upgrades within u1's ranges (y1-many 1.0.0 -> 2.0.0, hoisted to
# the root); yarn.lock rewritten, no package.json.
cverify c-ws-upgrade "workspace u1 upgrade" "$here/fixtures/yarn1-upgrade-ws"
n=$(ejson c-ws-upgrade "y['cloned']")
if [ $rc = 0 ] && same && said ": exit 0 in every run" && said "command: changed package.json: none" && said "command: changed yarn.lock: yes" && said "command: yarn.lock byte-identical (plain1, engine): true" \
  && [ "${n:-0}" -ge 150 ] && first128 c-ws-upgrade && [ "$(ejson c-ws-upgrade "r['command']")" = "workspace upgrade" ] \
  && grep -q '"2.0.0"' "$work/c-ws-upgrade/engine/node_modules/y1-many/package.json"; then
  ok "yarn workspace u1 upgrade: the workspace's Yarn upgrades y1-many to 2.0.0 within its range; yarn.lock rewritten, package.json not; engine identical; $n files cloned; logged as workspace upgrade"
else bad "yarn workspace upgrade (exit $rc, cloned ${n:-?}, command $(ejson c-ws-upgrade "r['command']"))"; fi

# The cache of a workspace command is its own Yarn's: the words after the
# name, resolved in the workspace's directory. (1) The command's own
# --cache-folder before `workspace` (a decoy folder; Yarn gives it to the
# workspace command, not to its child, which takes the run's cache from
# YARN_CACHE_FOLDER). (2) A relative --cache-folder after the name, which
# names the run's cache from packages/w2 (work/<tree>/packages/w2/../../..
# is the work directory) and another folder from the root. Asking
# `yarn cache dir` with the wrong options or in the wrong directory clones
# nothing.
cverify c-ws-cache "--cache-folder $work/c-ws-cache-decoy workspace w2 add $T/y1-many-2.0.0.tgz" "$here/fixtures/yarn1-workspaces"
n=$(ejson c-ws-cache "y['cloned']")
if [ $rc = 0 ] && same && [ "${n:-0}" -gt 285 ] && [ "$(ejson c-ws-cache "y['cache']")" = "$work/c-ws-cache/cache/v6" ]; then
  ok "yarn --cache-folder <other> workspace w2 add: the cache asked is the workspace's Yarn's, not the command's; engine identical; $n files cloned"
else bad "workspace command with its own --cache-folder (exit $rc, cloned ${n:-?}, cache $(ejson c-ws-cache "y['cache']"))"; fi
cverify c-ws-relcache "workspace w2 add $T/y1-many-2.0.0.tgz --cache-folder ../../../cache" "$here/fixtures/yarn1-workspaces"
n=$(ejson c-ws-relcache "y['cloned']")
if [ $rc = 0 ] && same && [ "${n:-0}" -gt 285 ] && [ "$(ejson c-ws-relcache "y['cache']")" = "$work/c-ws-relcache/cache/v6" ]; then
  ok "yarn workspace w2 add --cache-folder <relative>: resolved in the workspace's directory, as Yarn does; engine identical; $n files cloned"
else bad "workspace command with a relative --cache-folder (exit $rc, cloned ${n:-?}, cache $(ejson c-ws-relcache "y['cache']"))"; fi

# A command with nothing to do and no link step (upgrade-interactive after
# `upgrade --latest`: every dependency up to date, Yarn returns before its
# Install): passes when the case expects no clone.
cverify c-ui-noop "upgrade-interactive --latest" --command-tty --command-before "upgrade --latest" --engine-min-clones 0 "$here/fixtures/yarn1-upgrade"
if [ $rc = 0 ] && same && said ": exit 0 in every run" && grep -q 'All of your dependencies are up to date' "$work/c-ui-noop/engine.log" && [ "$(ejson c-ui-noop "y['link_steps']")" = 0 ]; then
  ok "yarn upgrade-interactive with nothing to upgrade: no prompt, no link step; engine identical"
else bad "yarn upgrade-interactive with nothing to upgrade (exit $rc, link steps $(ejson c-ui-noop "y['link_steps']"))"; fi

# The harness itself: a command that changes nothing reads "none" / "no",
# so a case cannot pass because the command did nothing.
cverify c-noop "install --force" --engine-min-clones 0 "$here/fixtures/yarn1-small"
if [ $rc = 0 ] && said "command: changed package.json: none" && said "command: changed yarn.lock: no"; then ok "harness: a command that changes nothing is reported as changing nothing"
else bad "harness on a command that changes nothing (exit $rc)"; fi

fi # TEST_YARN1_PART != route
if [ "${TEST_YARN1_PART:-all}" = commands ]; then echo "passed $pass, failed $fail"; [ $fail = 0 ]; exit; fi

# --- matchapack yarn run directly: gates, the hook's checks, the report -------
y1env() {
  env -u GITHUB_TOKEN -u MATCHAPACK_ENGINE -u MATCHAPACK_PATHS -u MATCHAPACK YARN_DISABLE_SELF_UPDATE_CHECK=true \
    PATH="$(dirname "$node"):$PATH" MATCHAPACK_YARN_JS="$yarnjs" MATCHAPACK_STORE="$work/y1/store" \
    MATCHAPACK_LOG_DIR="$work/y1/log" MATCHAPACK_CONFIG_DIR="$work/y1/config" "$@"
}
# myarn <dir> [VAR=value...] -- [yarn args]: matchapack yarn install in <dir>
# with the warm cache of e-many; output in $work/out, exit in $rc.
myarn() {
  local d=$1; shift; local vars=()
  while [ $# -gt 0 ] && [ "$1" != -- ]; do vars+=("$1"); shift; done
  shift
  # (bash 3.2 calls an empty array unbound under set -u)
  (cd "$d" && y1env ${vars[@]+"${vars[@]}"} "$bin" yarn install --frozen-lockfile --non-interactive --cache-folder "$work/e-many/cache" "$@") > "$work/out" 2>&1; rc=$?
}
lastlog() { tail -1 "$work/y1/log/log.jsonl" | python3 -c 'import json,sys; r=json.load(sys.stdin); print(eval(sys.argv[1]))' "$1"; }
project() { local d=$work/g-$1; rm -rf "${d:?}"; mkdir -p "$d"; cp -p "$here/fixtures/yarn1-many/package.json" "$here/fixtures/yarn1-many/yarn.lock" "$d/"; echo "$d"; }

# Each gate: with MATCHAPACK_NO_FALLBACK=1, exit 3, the project unchanged, the
# reason printed and logged.
gate() { # gate <name> <expected reason text> <dir> [VAR=value...] -- [yarn args]
  local name=$1 want=$2 d=$3; shift 3
  myarn "$d" MATCHAPACK_NO_FALLBACK=1 "$@"
  if [ $rc = 3 ] && [ ! -e "$d/node_modules/y1-many" ] && grep -q "not handled, left unchanged (.*$want" "$work/out" \
    && [ "$(lastlog 'r["outcome"]')" = declined ] && lastlog 'r["reason"]' | grep -q "$want"; then
    ok "gate $name: plain yarn ($want), logged"
  else bad "gate $name (exit $rc)"; fi
}
d=$(project yarnpath); printf 'yarn-path "%s"\n' "$yarnjs" > "$d/.yarnrc"; gate yarn-path 'sets yarn-path' "$d" --
# The version floor: a Yarn 1 older than 1.10.0 asks for no clone (a copy of
# the Yarn under test whose package.json says 1.9.4).
oy=$work/yarn-1.9.4; rm -rf "${oy:?}"; mkdir -p "$oy"; cp -R "$(dirname "$(dirname "$yarnjs")")/." "$oy/"
python3 -c 'import json,sys; p=sys.argv[1]; d=json.load(open(p)); d["version"]="1.9.4"; json.dump(d, open(p,"w"))' "$oy/package.json"
d=$(project floor); gate "Yarn older than 1.10.0" 'yarn 1.9.4 is older than 1.10.0' "$d" MATCHAPACK_YARN_JS="$oy/bin/yarn.js" --
# (decided by matchapack from the package's version, before any node starts;
# the hook's own check, for a Yarn corepack chose, is the corepack case)
if ! grep -q 'matchapack yarn hook:' "$work/out"; then ok "Yarn older than 1.10.0: declined before node started"
else bad "Yarn older than 1.10.0: declined only by the hook (node was started)"; fi
# A Yarn 2+ project is told from its files before anything else, and is
# plain Yarn unlogged (NOTES "Yarn 1 shell route"): exit 3 here, the reason
# printed, no log line.
yarn2() { # yarn2 <name> <expected reason text> <dir>
  local n0; n0=$(wc -l < "$work/y1/log/log.jsonl" 2>/dev/null || echo 0)
  myarn "$3" MATCHAPACK_NO_FALLBACK=1 --
  if [ $rc = 3 ] && [ ! -e "$3/node_modules" ] && grep -q "not handled, left unchanged (.*$2" "$work/out" \
    && [ "$(wc -l < "$work/y1/log/log.jsonl" 2>/dev/null || echo 0)" = "$n0" ]; then ok "Yarn 2+ $1: plain yarn ($2), not logged"
  else bad "Yarn 2+ $1 (exit $rc)"; fi
}
setpm() { python3 -c 'import json,sys; p=sys.argv[1]; d=json.load(open(p)); d["packageManager"]=sys.argv[2]; json.dump(d, open(p,"w"))' "$1/package.json" "$2"; }
d=$(project yarnrcyml); touch "$d/.yarnrc.yml"; yarn2 .yarnrc.yml 'Yarn 2+ project' "$d"
d=$(project pm); setpm "$d" yarn@4.5.0; yarn2 packageManager 'packageManager is yarn@4.5.0' "$d"
d=$(project berry); printf '__metadata:\n  version: 8\n' > "$d/yarn.lock"; yarn2 berry-lockfile 'Yarn 2+ lockfile' "$d"
d=$(project nolockyml); rm "$d/yarn.lock"; touch "$d/.yarnrc.yml"; yarn2 no-yarn.lock-with-.yarnrc.yml 'Yarn 2+ project' "$d"
# A ~/.yarnrc.yml-like file above the lockfile folder does not make it one.
d=$(project ymlabove); mkdir -p "$d/sub"; mv "$d/package.json" "$d/yarn.lock" "$d/sub/"; touch "$d/.yarnrc.yml"; myarn "$d/sub" MATCHAPACK_NO_FALLBACK=1 --
if [ $rc = 0 ] && [ -f "$d/sub/node_modules/.yarn-integrity" ]; then ok "a .yarnrc.yml above the lockfile folder: still Yarn 1, installed"; else bad ".yarnrc.yml above the project (exit $rc)"; fi
# Another package manager's project: an ordinary gate (printed and logged).
d=$(project pnpm); setpm "$d" pnpm@10.0.0; gate packageManager-pnpm 'packageManager is pnpm@10.0.0' "$d" --
d=$(project nolock); rm "$d/yarn.lock"; gate no-yarn.lock 'no yarn.lock' "$d" --
d=$(project nmlink); mkdir -p "$work/g-nm-target"; ln -s "$work/g-nm-target" "$d/node_modules"; gate node_modules-symlink 'node_modules is a symlink' "$d" --
d=$(project modfolder); gate --modules-folder 'moves the install' "$d" -- --modules-folder "$d/elsewhere"
d=$(project cwd); gate --cwd 'moves the install' "$d" -- --cwd "$d"
d=$(project scope); myarn "$d" MATCHAPACK_NO_FALLBACK=1 MATCHAPACK_PATHS=/nonexistent-scope --
if [ $rc = 3 ] && grep -q 'outside the trial scope' "$work/out"; then ok "gate trial scope: plain yarn"; else bad "gate trial scope (exit $rc)"; fi
# The probe: a Node whose copy keeps times (a future libuv), and one that
# shares blocks: the hook steps aside before Yarn starts.
d=$(project keeps); gate copy-keeps-times "kept a non-empty file's mtime" "$d" MATCHAPACK_YARN_SIMULATE=copy-keeps-times --
d=$(project shares); myarn "$d" MATCHAPACK_NO_FALLBACK=1 MATCHAPACK_YARN_SIMULATE=probe-shares --
if [ $rc = 3 ] && grep -q 'shares blocks with its source' "$work/out" && [ ! -e "$d/node_modules/y1-many" ]; then ok "probe: a copy that shares blocks: plain yarn"; else bad "probe shares (exit $rc)"; fi
# Without MATCHAPACK_NO_FALLBACK a decline runs plain Yarn, which installs.
d=$(project fallback); printf 'yarn-path "%s"\n' "$yarnjs" > "$d/.yarnrc"; myarn "$d" --
if [ $rc = 0 ] && grep -q 'running plain yarn instead' "$work/out" && [ -f "$d/node_modules/.yarn-integrity" ]; then ok "a declined install runs plain yarn, which installs"
else bad "decline then plain yarn (exit $rc)"; fi
d=$(project keepsplain); myarn "$d" MATCHAPACK_YARN_SIMULATE=copy-keeps-times --
if [ $rc = 0 ] && grep -q 'running plain yarn instead' "$work/out" && [ -f "$d/node_modules/.yarn-integrity" ]; then ok "the probe's decline runs plain yarn, which installs"
else bad "probe decline then plain yarn (exit $rc)"; fi
# Secrets in the arguments never reach the log.
d=$(project secret); myarn "$d" -- --otp 314159 --registry "http://u:hunter2@127.0.0.1:48321/"
if [ $rc = 0 ] && ! grep -q '314159\|hunter2' "$work/y1/log/log.jsonl" && [ "$(lastlog 'r["args"][-1]')" = "http://***@127.0.0.1:48321/" ]; then ok "an --otp value and URL credentials are not logged"
else bad "secrets in the log (exit $rc)"; fi
# With MATCHAPACK_YARN_JS the only Yarn there is (no yarn on PATH), every
# path runs it: other commands, --version, the MATCHAPACK=0 bypass.
nodeonly=$work/nodeonly; mkdir -p "$nodeonly"; ln -sf "$node" "$nodeonly/node"
d=$(project override); python3 -c 'import json,sys; p=sys.argv[1]; d=json.load(open(p)); d["scripts"]={"hello":"node -e \"console.log(\x27hello-from-run\x27)\""}; json.dump(d, open(p,"w"))' "$d/package.json"
o1=$(cd "$d" && env -u MATCHAPACK_ENGINE PATH="$nodeonly:/usr/bin:/bin" MATCHAPACK_YARN_JS="$yarnjs" MATCHAPACK_LOG_DIR="$work/y1/log" "$bin" yarn --version 2>&1); r1=$?
o2=$(cd "$d" && env -u MATCHAPACK_ENGINE PATH="$nodeonly:/usr/bin:/bin" MATCHAPACK_YARN_JS="$yarnjs" MATCHAPACK_LOG_DIR="$work/y1/log" "$bin" yarn run hello 2>&1); r2=$?
(cd "$d" && env -u MATCHAPACK_ENGINE -u GITHUB_TOKEN PATH="$nodeonly:/usr/bin:/bin" MATCHAPACK=0 MATCHAPACK_YARN_JS="$yarnjs" YARN_DISABLE_SELF_UPDATE_CHECK=true \
  "$bin" yarn install --frozen-lockfile --non-interactive --cache-folder "$work/e-many/cache") > "$work/out" 2>&1; r3=$?
if [ $r1 = 0 ] && [ "$o1" = "$("$node" "$yarnjs" --version)" ] && [ $r2 = 0 ] && printf '%s' "$o2" | grep -q hello-from-run && [ $r3 = 0 ] && [ -f "$d/node_modules/.yarn-integrity" ]; then
  ok "MATCHAPACK_YARN_JS runs every command, --version and the MATCHAPACK=0 bypass included"
else bad "override for passthrough commands (exit $r1, $r2, $r3; $o1)"; fi
# Not installs: passed to Yarn as they are, not logged.
n0=$(wc -l < "$work/y1/log/log.jsonl")
(cd "$work" && y1env "$bin" yarn --version) > "$work/out" 2>&1; r1=$?
(cd "$(project plainpass)" && y1env MATCHAPACK=0 "$bin" yarn install --frozen-lockfile --non-interactive --cache-folder "$work/e-many/cache") > /dev/null 2>&1; r2=$?
if [ $r1 = 0 ] && grep -qx '1\.[0-9.]*' "$work/out" && [ $r2 = 0 ] && [ "$(wc -l < "$work/y1/log/log.jsonl")" = "$n0" ]; then ok "yarn --version and MATCHAPACK=0 go straight to Yarn, unlogged"
else bad "passthrough (exit $r1, $r2)"; fi

# Yarn linked, the hook saw no link step: said, logged, and in the report.
d=$(project unseen); myarn "$d" MATCHAPACK_YARN_HOOK=none --
if [ $rc = 0 ] && grep -q 'never saw its link step' "$work/out" && [ "$(lastlog 'r["yarn1"]["unseen"]')" = True ]; then ok "hook not installed: Yarn installs, the run says the hook saw no link step"
else bad "hook not installed (exit $rc)"; fi
myarn "$d" --
if [ $rc = 0 ] && [ "$(lastlog 'r["yarn1"]["linked"]')" = False ] && [ "$(lastlog 'r["yarn1"]["unseen"]')" = False ]; then ok "an up-to-date install (no link step) is not reported as unseen"
else bad "up-to-date install (exit $rc)"; fi
y1env "$bin" report > "$work/out" 2>&1
if grep -q "Yarn linked node_modules but the hook never saw its link step" "$work/out" && grep -q '^yarn: .* runs of `matchapack yarn`' "$work/out" \
  && grep -q 'files cloned from Yarn' "$work/out" && grep -q 'runs 0: installed 0' "$work/out"; then
  ok "matchapack report: yarn lines (outcomes, clones, the unseen link step), none in the npm counts"
else bad "matchapack report"; fi

# TEST_YARN1_PART=engine: the engine's cases only (not corepack, the shell
# route or convert-existing.sh).
if [ "${TEST_YARN1_PART:-all}" != engine ]; then
# --- corepack (`yarn` linked to corepack's dist/yarn.js) ----------------------
# corepack chooses the project's Yarn and starts its bin/yarn.js in the same
# process; the hook takes over at that start. A scratch COREPACK_HOME,
# seeded from the Yarn 1 package under test (never the real one; no network:
# COREPACK_ENABLE_NETWORK=0). MATCHAPACK_COREPACK_JS names corepack's
# dist/yarn.js; default: the one bundled with $node (Node <= 24).
cpjs=${MATCHAPACK_COREPACK_JS:-$(dirname "$node")/../lib/node_modules/corepack/dist/yarn.js}
yv=$("$node" -p 'require(process.argv[1]).version' "$(dirname "$(dirname "$yarnjs")")/package.json")
if [ ! -f "$cpjs" ]; then
  echo "SKIP corepack: no corepack next to $node (Node >= 25 does not bundle it; set MATCHAPACK_COREPACK_JS to a userland one's dist/yarn.js)"
else
  cpjs=$(python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$cpjs")
  ch=$work/corepack-home
  rm -rf "${ch:?}"; mkdir -p "$ch/v1/yarn/$yv"; cp -R "$(dirname "$(dirname "$yarnjs")")/." "$ch/v1/yarn/$yv/"
  printf '{"locator":{"name":"yarn","reference":"%s"},"bin":{"yarn":"./bin/yarn.js","yarnpkg":"./bin/yarn.js"},"hash":"sha512.seeded-by-test-yarn1"}' "$yv" > "$ch/v1/yarn/$yv/.corepack"
  printf '{"yarn":"%s"}' "$yv" > "$ch/lastKnownGood.json"
  # A release corepack could choose that is not Yarn 1's package (the shape
  # of a Yarn 2+ one: name @yarnpkg/cli-dist), with a bundle that would pass
  # the hook's other checks.
  fk=$ch/v1/yarn/1.99.0; mkdir -p "$fk/bin" "$fk/lib"
  printf '{"locator":{"name":"yarn","reference":"1.99.0"},"bin":{"yarn":"./bin/yarn.js"},"hash":"sha512.fake"}' > "$fk/.corepack"
  printf '{"name":"@yarnpkg/cli-dist","version":"4.5.0"}' > "$fk/package.json"
  printf "#!/usr/bin/env node\nrequire('../lib/cli')\n" > "$fk/bin/yarn.js"
  printf "const CONCURRENT_QUEUE_ITEMS = require('fs').copyFile ? 128 : 4;\nconsole.log('FAKE-YARN ' + process.argv.slice(2).join(' '))\n" > "$fk/lib/cli.js"
  export COREPACK_ENABLE_NETWORK=0 COREPACK_ENABLE_DOWNLOAD_PROMPT=0 COREPACK_ENABLE_AUTO_PIN=0
  cpsrc=$work/cp-src; rm -rf "${cpsrc:?}"; mkdir -p "$cpsrc"; cp "$here/fixtures/yarn1-many/package.json" "$here/fixtures/yarn1-many/yarn.lock" "$cpsrc/"
  setpm "$cpsrc" "yarn@$yv"
  if [ "$(cd "$cpsrc" && COREPACK_HOME=$ch "$node" "$cpjs" --version 2>&1)" != "$yv" ]; then
    bad "corepack does not run the seeded Yarn $yv offline: $(cd "$cpsrc" && COREPACK_HOME=$ch "$node" "$cpjs" --version 2>&1 | tail -1)"
  else
    # plain (through corepack) vs engine (through corepack), same cache.
    COREPACK_HOME=$ch MATCHAPACK_YARN_JS=$cpjs everify cp-many "$cpsrc"
    n=$(ejson cp-many "y['cloned']")
    x=$work/cp-many/engine/node_modules/y1-execpath/execpath.txt
    if [ $rc = 0 ] && same && [ "${n:-0}" -ge 150 ] && [ "$(ejson cp-many "r.get('launcher')")" = corepack ] && [ "$(ejson cp-many "r['yarn']")" = "$yv" ] \
      && [ "$(head -1 "$x" 2>/dev/null)" = "$ch/v1/yarn/$yv/bin/yarn.js" ]; then
      ok "corepack: the Yarn it chose ($yv) runs with the hook, identical to plain corepack; $n files cloned; npm_execpath is corepack's copy"
    else bad "corepack engine (exit $rc, cloned ${n:-?}, execpath $(head -1 "$x" 2>/dev/null))"; fi
    # corepack chose a release that is not Yarn 1's package: declined before
    # Yarn starts, then plain corepack runs it.
    d=$(project cpfake); setpm "$d" yarn@1.99.0
    myarn "$d" COREPACK_HOME="$ch" MATCHAPACK_YARN_JS="$cpjs" MATCHAPACK_NO_FALLBACK=1 --
    if [ $rc = 3 ] && grep -q "not Yarn 1's npm package (corepack chose yarn 4.5.0)" "$work/out" && ! grep -q FAKE-YARN "$work/out" && [ "$(lastlog 'r["outcome"]')" = declined ]; then
      myarn "$d" COREPACK_HOME="$ch" MATCHAPACK_YARN_JS="$cpjs" --
      if [ $rc = 0 ] && grep -q 'running plain yarn instead' "$work/out" && grep -q '^FAKE-YARN install' "$work/out"; then ok "corepack chose a release that is not Yarn 1: declined, then plain corepack runs it"
      else bad "corepack non-Yarn-1, plain run (exit $rc)"; fi
    else bad "corepack non-Yarn-1 declined (exit $rc)"; fi
    # corepack chose a Yarn 1 older than 1.10.0: the hook declines before
    # Yarn starts (the version floor; matchapack never saw that Yarn).
    fo=$ch/v1/yarn/1.9.4; mkdir -p "$fo/bin" "$fo/lib"
    printf '{"locator":{"name":"yarn","reference":"1.9.4"},"bin":{"yarn":"./bin/yarn.js"},"hash":"sha512.fake"}' > "$fo/.corepack"
    printf '{"name":"yarn","version":"1.9.4"}' > "$fo/package.json"
    printf "#!/usr/bin/env node\nrequire('../lib/cli')\n" > "$fo/bin/yarn.js"
    printf "const CONCURRENT_QUEUE_ITEMS = require('fs').copyFile ? 128 : 4;\nconsole.log('OLD-YARN ' + process.argv.slice(2).join(' '))\n" > "$fo/lib/cli.js"
    d=$(project cpold); setpm "$d" yarn@1.9.4
    myarn "$d" COREPACK_HOME="$ch" MATCHAPACK_YARN_JS="$cpjs" MATCHAPACK_NO_FALLBACK=1 --
    if [ $rc = 3 ] && grep -q "yarn 1.9.4 is older than 1.10.0" "$work/out" && ! grep -q OLD-YARN "$work/out" && [ "$(lastlog 'r["outcome"]')" = declined ]; then
      ok "corepack chose Yarn 1.9.4: the hook declines (older than 1.10.0) before Yarn starts"
    else bad "corepack chose an old Yarn 1 (exit $rc)"; fi
  fi
fi

# --- Homebrew's yarn (homebrew-core Formula/y/yarn.rb) -------------------------
# Built here as the formula builds it, without brew: Yarn 1's package in
# <keg>/libexec with the formula's two edits (/usr/local → the prefix in
# lib/cli.js; installationMethod in package.json), <keg>/bin/yarn the script
# Pathname#write_env_script writes (PREFIX="<prefix>" exec <yarn.js>), and
# <prefix>/bin/yarn a relative link to it. `yarn` on PATH is that link, no
# MATCHAPACK_YARN_JS. Plain Homebrew Yarn against `matchapack yarn`, same
# cache: identical trees; the root postinstall records PREFIX and
# npm_execpath as Yarn's scripts see them.
if [ "$(uname)" != Darwin ]; then echo "SKIP Homebrew's yarn: macOS only (matchapack yarn is plain Yarn elsewhere)"
else
  hb=$work/hb; rm -rf "${hb:?}"; hbp=$hb/brew; keg=$hbp/Cellar/yarn/$yv
  mkdir -p "$keg/libexec" "$keg/bin" "$hbp/bin"
  cp -R "$(dirname "$(dirname "$yarnjs")")/." "$keg/libexec/"
  # (each edit must change something, as `inreplace` requires)
  python3 -c '
import sys
k, p = sys.argv[1], sys.argv[2]
f = k + "/libexec/lib/cli.js"; s = open(f).read(); assert "/usr/local" in s; open(f, "w").write(s.replace("/usr/local", p))
f = k + "/libexec/package.json"; s = open(f).read(); a = "\"installationMethod\": \"tar\""; assert a in s
open(f, "w").write(s.replace(a, "\"installationMethod\": \"homebrew\""))' "$keg" "$hbp" || bad "Homebrew layout: the formula's edits did not apply"
  # write_env_script: "#!/bin/bash\n#{env_export}exec \"#{target}\" #{args} \"$@\"\n", no args.
  hbjs=$keg/libexec/bin/yarn.js
  printf '#!/bin/bash\nPREFIX="%s" exec "%s"  "$@"\n' "$hbp" "$hbjs" > "$keg/bin/yarn"; chmod 0555 "$keg/bin/yarn"
  ln -s yarn "$keg/bin/yarnpkg"; ln -s "../Cellar/yarn/$yv/bin/yarn" "$hbp/bin/yarn"
  mkdir -p "$hb/src"; cp -p "$here/fixtures/yarn1-many/package.json" "$here/fixtures/yarn1-many/yarn.lock" "$hb/src/"
  python3 -c 'import json,sys; p=sys.argv[1]; d=json.load(open(p)); d["scripts"]={"postinstall":"node -e \"require(\x27fs\x27).writeFileSync(\x27node_modules/.hb-env\x27, process.env.PREFIX + \x27\\n\x27 + process.env.npm_execpath)\""}; json.dump(d, open(p,"w"))' "$hb/src/package.json"
  # hbrun <name> plain|engine [PATH dir]: a fresh copy $hb/<name>, one install
  # with `yarn` from PATH (default the Homebrew prefix); $rc, $win, $hb/<name>.log.
  hbrun() {
    local d=$hb/$1 how=$2 pd=${3:-$hbp/bin} s; cpp "$hb/src" "$d"; s=$(now_ns)
    local run=(yarn); [ "$how" = engine ] && run=("$bin" yarn)
    (cd "$d" && env -u GITHUB_TOKEN -u PREFIX -u MATCHAPACK_YARN_JS -u MATCHAPACK_ENGINE -u MATCHAPACK_PATHS -u MATCHAPACK \
      YARN_DISABLE_SELF_UPDATE_CHECK=true PATH="$pd:$(dirname "$node"):/usr/bin:/bin" MATCHAPACK_STORE="$hb/mp/store" \
      MATCHAPACK_LOG_DIR="$hb/mp/log" MATCHAPACK_CONFIG_DIR="$hb/mp/config" ${HB_VARS-} \
      "${run[@]}" install --frozen-lockfile --non-interactive --cache-folder "$hb/cache") > "$d.log" 2>&1; rc=$?
    win=$s:$(now_ns)
  }
  hrec() { python3 -c 'import json,sys; r=json.loads(open(sys.argv[1]).read().splitlines()[-1]); y=r.get("yarn1") or {}; print(eval(sys.argv[2]))' "$hb/mp/log/log.jsonl" "$1" 2>/dev/null; }
  hbrun prime plain; r0=$rc
  hbrun plain plain; rp=$rc wp=$win
  HB_VARS=MATCHAPACK_NO_FALLBACK=1 hbrun engine engine; re=$rc we=$win
  n=$(hrec 'y["cloned"]')
  "$bin" diff --meta --normalize-roots "$hb/plain" "$hb/engine" --a-run "$wp" --b-run "$we" --a-cache "$hb/cache" --b-cache "$hb/cache" > "$hb/diff.txt" 2>&1; rd=$?
  want=$(printf '%s\n%s' "$hbp" "$hbjs")
  if [ "$r0" = 0 ] && [ "$rp" = 0 ] && [ "$re" = 0 ] && [ "$rd" = 0 ] && [ "$(hrec 'r["outcome"]')" = installed ] && [ "${n:-0}" -gt 0 ] \
    && [ "$(cat "$hb/plain/node_modules/.hb-env")" = "$want" ] && [ "$(cat "$hb/engine/node_modules/.hb-env")" = "$want" ] \
    && [ "$(head -1 "$hb/engine/node_modules/y1-execpath/execpath.txt")" = "$hbjs" ] \
    && cmp -s "$hb/plain/node_modules/y1-execpath/execpath.txt" "$hb/engine/node_modules/y1-execpath/execpath.txt"; then
    ok "Homebrew's yarn: plain vs matchapack yarn identical (diff --meta); $n files cloned; scripts see the launcher's PREFIX and npm_execpath = its libexec/bin/yarn.js"
  else bad "Homebrew's yarn (prime $r0, plain $rp, engine $re, diff $rd, cloned ${n:-?}; env $(tr '\n' ' ' < "$hb/engine/node_modules/.hb-env" 2>/dev/null))"; sed 's/^/    /' "$hb/engine.log" | tail -4; grep -v '^      ' "$hb/diff.txt" | tail -15; fi
  # A launcher of another shape (it works, and runs the same Yarn) is not
  # recognized, and it is no version manager's shim matchapack follows
  # (util::known_launcher): declined without being run, and without
  # MATCHAPACK_NO_FALLBACK plain yarn runs it.
  hbo=$hb/other; mkdir -p "$hbo/Cellar/yarn/$yv/bin" "$hbo/bin"; ln -s "$keg/libexec" "$hbo/Cellar/yarn/$yv/libexec"
  printf '#!/bin/bash\nexport PREFIX="%s"\nexec "%s" "$@"\n' "$hbp" "$hbjs" > "$hbo/Cellar/yarn/$yv/bin/yarn"; chmod 0555 "$hbo/Cellar/yarn/$yv/bin/yarn"
  ln -s "../Cellar/yarn/$yv/bin/yarn" "$hbo/bin/yarn"
  HB_VARS=MATCHAPACK_NO_FALLBACK=1 hbrun other-declined engine "$hbo/bin"
  if [ $rc = 3 ] && grep -q "neither Yarn 1's npm package, Homebrew's launcher for it, nor corepack's launcher" "$hb/other-declined.log" \
    && [ ! -e "$hb/other-declined/node_modules" ] && [ "$(hrec 'r["outcome"]')" = declined ]; then
    hbrun other-plain engine "$hbo/bin"
    if [ $rc = 0 ] && grep -q 'running plain yarn instead' "$hb/other-plain.log" && [ "$(cat "$hb/other-plain/node_modules/.hb-env" 2>/dev/null)" = "$want" ]; then
      ok "a Homebrew-like launcher of another shape: declined, then plain yarn runs it (PREFIX $hbp)"
    else bad "other launcher shape, plain run (exit $rc)"; fi
  else bad "other launcher shape declined (exit $rc)"; sed 's/^/    /' "$hb/other-declined.log" | tail -4; fi
fi

# --- The shell route: the `yarn` function `matchapack init` prints ------------
# The real binary's init, in zsh and bash (fish at the end: MATCHAPACK_FISH);
# `yarn` on PATH linked to Yarn 1's bin/yarn.js as an npm-global install
# links it, no MATCHAPACK_YARN_JS. A
# `matchapack` shim on PATH records every start, so "never started" is
# proved, not assumed.
rt=$work/route; rm -rf "${rt:?}"; mkdir -p "$rt/bin" "$rt/stub"
ln -s "$yarnjs" "$rt/bin/yarn"; ln -s "$node" "$rt/bin/node"
printf '#!/bin/sh\necho "$*" >> "%s/starts"\nexec "%s" "$@"\n' "$rt" "$bin" > "$rt/bin/matchapack"
cp "$rt/bin/matchapack" "$rt/stub/matchapack"
# The Yarn 4 case: a `yarn` that is not Yarn 1 and a `node` that must not run.
printf '#!/bin/sh\nprintf STUB-YARN; for a in "$@"; do printf " [%%s]" "$a"; done; echo; exit "${STUB_RC:-0}"\n' > "$rt/stub/yarn"
printf '#!/bin/sh\necho "$*" >> "%s/nodes"\nexit 99\n' "$rt" > "$rt/stub/node"
chmod +x "$rt/bin/matchapack" "$rt/stub/matchapack" "$rt/stub/yarn" "$rt/stub/node"
# rrun <dir> <zsh|bash> <PATH> <commands> [VAR=value...]: output in $work/out, exit in $rc.
rrun() {
  local d=$1 sh=$2 p=$3 c=$4; shift 4
  local run; case $sh in zsh) run="zsh -f -c" ;; bash) run="bash --norc --noprofile -c" ;; esac
  : > "$rt/starts"; rm -f "$rt/nodes"
  (cd "$d" && env -u MATCHAPACK_YARN_JS -u MATCHAPACK_ENGINE -u MATCHAPACK_PATHS -u MATCHAPACK -u GITHUB_TOKEN PATH="$p" \
    YARN_CACHE_FOLDER="$work/e-many/cache" YARN_DISABLE_SELF_UPDATE_CHECK=true MATCHAPACK_STORE="$work/y1/store" \
    MATCHAPACK_LOG_DIR="$work/y1/log" MATCHAPACK_CONFIG_DIR="$work/y1/config" "$@" \
    $run "command -v matchapack >/dev/null && eval \"\$(matchapack init $sh)\"; $c") > "$work/out" 2>&1; rc=$?
}
starts() { grep -c '^yarn' "$rt/starts"; }
engine_ran() { grep -q "matchapack: yarn with clones of Yarn's cache" "$work/out"; }
quiet() { ! grep -q '^matchapack' "$work/out"; }
for sh in zsh bash; do
  rrun "$work" $sh "$rt/bin:/usr/bin:/bin" 'type yarn'
  case $(uname) in
    Darwin) want='yarn is a .*function' ;;
    *) want='yarn is /' ;;
  esac
  if [ $rc = 0 ] && head -1 "$work/out" | grep -q "$want"; then ok "route $sh: \`type yarn\` after the eval line: $(head -1 "$work/out")"
  else bad "route $sh: type yarn (exit $rc)"; fi
done
if [ "$(uname)" != Darwin ]; then echo "SKIP the shell route's cases: no yarn function off macOS"
else
  d=$(project rbare); rrun "$d" zsh "$rt/bin:/usr/bin:/bin" 'yarn'
  if [ $rc = 0 ] && [ "$(starts)" = 1 ] && engine_ran && grep -q 'files cloned from Yarn' "$work/out" && [ -f "$d/node_modules/.yarn-integrity" ]; then ok "route: bare yarn → matchapack yarn, cloned"
  else bad "route: bare yarn (exit $rc, starts $(starts))"; fi
  d=$(project rinstall); rrun "$d" zsh "$rt/bin:/usr/bin:/bin" 'yarn install --frozen-lockfile'
  if [ $rc = 0 ] && [ "$(starts)" = 1 ] && engine_ran && [ "$(lastlog 'r["args"]')" = "['install', '--frozen-lockfile']" ]; then ok "route: yarn install --frozen-lockfile → matchapack yarn, arguments unchanged"
  else bad "route: yarn install --frozen-lockfile (exit $rc, starts $(starts))"; fi
  d=$(project rflag); rrun "$d" bash "$rt/bin:/usr/bin:/bin" 'yarn --frozen-lockfile'
  if [ $rc = 0 ] && [ "$(starts)" = 1 ] && engine_ran && [ -f "$d/node_modules/.yarn-integrity" ]; then ok "route (bash): yarn --frozen-lockfile → matchapack yarn"
  else bad "route (bash): yarn --frozen-lockfile (exit $rc, starts $(starts))"; fi
  d=$(project radd); rrun "$d" zsh "$rt/bin:/usr/bin:/bin" 'yarn add http://127.0.0.1:48321/y1-c-1.0.0.tgz --non-interactive'
  if [ $rc = 0 ] && [ "$(starts)" = 1 ] && engine_ran && grep -q '"y1-c"' "$d/package.json" && [ "$(lastlog 'r["command"]')" = add ] \
    && [ "$(lastlog 'r["args"]')" = "['add', 'http://127.0.0.1:48321/y1-c-1.0.0.tgz', '--non-interactive']" ]; then ok "route: yarn add x → matchapack yarn, arguments unchanged"
  else bad "route: yarn add (exit $rc, starts $(starts))"; fi
  d=$work/g-rws; rm -rf "${d:?}"; cpp "$here/fixtures/yarn1-workspaces" "$d"
  rrun "$d" bash "$rt/bin:/usr/bin:/bin" 'yarn install --frozen-lockfile --non-interactive && yarn workspace w2 add http://127.0.0.1:48321/y1-c-1.0.0.tgz --non-interactive'
  if [ $rc = 0 ] && [ "$(starts)" = 2 ] && grep -q '"y1-c"' "$d/packages/w2/package.json" && [ "$(lastlog 'r["command"]')" = "workspace add" ]; then ok "route (bash): yarn workspace w2 add x → matchapack yarn"
  else bad "route: yarn workspace add (exit $rc, starts $(starts))"; fi
  # Not routed: Yarn's other commands, a workspace's other commands. A stub
  # yarn: never a real `yarn global` in a test (it writes the user's global
  # folder).
  rrun "$work" zsh "$rt/stub:/usr/bin:/bin" 'yarn global add x; yarn workspace w2 run add; yarn workspace w2 install; yarn why add'
  if [ "$(starts)" = 0 ] && [ "$(cat "$work/out")" = "STUB-YARN [global] [add] [x]
STUB-YARN [workspace] [w2] [run] [add]
STUB-YARN [workspace] [w2] [install]
STUB-YARN [why] [add]" ]; then ok "route: yarn global add, yarn workspace w2 run / install, yarn why → plain yarn, matchapack never started"
  else bad "route: commands not routed (starts $(starts); $(tr '\n' '|' < "$work/out"))"; fi
  rrun "$work" zsh "$rt/bin:/usr/bin:/bin" 'yarn --version'
  if [ $rc = 0 ] && [ "$(starts)" = 0 ] && [ "$(cat "$work/out")" = "$yv" ]; then ok "route: yarn --version → plain yarn, matchapack never started"
  else bad "route: yarn --version (exit $rc, starts $(starts))"; fi
  d=$(project roptout); rrun "$d" zsh "$rt/bin:/usr/bin:/bin" 'MATCHAPACK=0 yarn'
  if [ $rc = 0 ] && [ "$(starts)" = 0 ] && quiet && [ -f "$d/node_modules/.yarn-integrity" ]; then ok "route: MATCHAPACK=0 yarn → plain yarn, matchapack never started"
  else bad "route: MATCHAPACK=0 yarn (exit $rc, starts $(starts))"; fi
  d=$(project rcommand); rrun "$d" zsh "$rt/bin:/usr/bin:/bin" 'command yarn install --frozen-lockfile'
  if [ $rc = 0 ] && [ "$(starts)" = 0 ] && quiet && [ -f "$d/node_modules/.yarn-integrity" ]; then ok "route: command yarn → plain yarn, matchapack never started"
  else bad "route: command yarn (exit $rc, starts $(starts))"; fi
  # A failing install (the root postinstall exits 7): Yarn's status comes
  # back through the function, the same as plain Yarn's.
  rootfail() { python3 -c 'import json,sys; p=sys.argv[1]; d=json.load(open(p)); d["scripts"]={"postinstall":"node -e \"process.exit(7)\""}; json.dump(d, open(p,"w"))' "$1/package.json"; }
  d=$(project rfailplain); rootfail "$d"; rrun "$d" zsh "$rt/bin:/usr/bin:/bin" 'command yarn install --frozen-lockfile; echo "status $?"'
  want=$(tail -1 "$work/out")
  d=$(project rfail); rootfail "$d"; rrun "$d" zsh "$rt/bin:/usr/bin:/bin" 'yarn install --frozen-lockfile; echo "status $?"'
  if [ "$(starts)" = 1 ] && engine_ran && [ "$want" != "status 0" ] && [ "$(tail -1 "$work/out")" = "$want" ] && [ "$(lastlog 'r["outcome"]')" = yarn_failed ]; then ok "route: a failing install's status ($want) passes through the function, as plain Yarn's"
  else bad "route: failing install (starts $(starts), plain $want, got $(tail -1 "$work/out"))"; fi
  # A Yarn 4 project: told from its files; the `yarn` on PATH is not Yarn 1
  # and `node` must not start. Plain yarn, with its status, nothing printed
  # or logged.
  d=$work/g-ryarn4; rm -rf "${d:?}"; mkdir -p "$d"; printf '{"name":"y4","packageManager":"yarn@4.5.0"}' > "$d/package.json"
  printf 'nodeLinker: node-modules\n' > "$d/.yarnrc.yml"; printf '__metadata:\n  version: 8\n' > "$d/yarn.lock"
  n0=$(wc -l < "$work/y1/log/log.jsonl")
  rrun "$d" zsh "$rt/stub:/usr/bin:/bin" 'yarn install; echo "status $?"' STUB_RC=4
  if [ "$(starts)" = 1 ] && [ "$(cat "$work/out")" = "STUB-YARN [install]
status 4" ] && [ ! -e "$rt/nodes" ] && [ "$(wc -l < "$work/y1/log/log.jsonl")" = "$n0" ]; then ok "route: a Yarn 4 project → plain yarn, decided from files (no node start, nothing printed or logged), status 4 passed"
  else bad "route: Yarn 4 project ($(tr '\n' '|' < "$work/out"); node: $(cat "$rt/nodes" 2>/dev/null))"; fi
  # corepack, as `corepack enable` links it: `yarn` → corepack's dist/yarn.js.
  if [ -n "${ch-}" ] && [ -f "$cpjs" ]; then
    mkdir -p "$rt/cpbin"; ln -sf "$cpjs" "$rt/cpbin/yarn"; ln -sf "$node" "$rt/cpbin/node"; cp "$rt/bin/matchapack" "$rt/cpbin/"
    d=$(project rcorepack); setpm "$d" "yarn@$yv"
    rrun "$d" zsh "$rt/cpbin:/usr/bin:/bin" 'yarn' COREPACK_HOME="$ch"
    if [ $rc = 0 ] && [ "$(starts)" = 1 ] && grep -q 'files cloned from Yarn' "$work/out" && [ "$(lastlog 'r.get("launcher")')" = corepack ]; then ok "route: bare yarn with yarn linked to corepack → matchapack yarn through corepack, cloned"
    else bad "route: corepack (exit $rc, starts $(starts))"; fi
  fi
  # fish (MATCHAPACK_FISH, else a fish on PATH; none is installed on the
  # owner's Mac, so a standalone fish from fish-shell's releases, unpacked
  # anywhere, can be named): the real binary's `matchapack init fish`, whose
  # yarn function routes an install through `matchapack yarn`. Plain
  # (`command yarn`) against the function on fresh copies, one cache whose
  # path has a space and a quote (passed in fish's own quoting): identical
  # trees, clones, the arguments unchanged, and a failing install's status.
  fish=${MATCHAPACK_FISH:-$(command -v fish 2>/dev/null || true)}
  if [ -z "$fish" ]; then echo "SKIP route fish: no fish on PATH (set MATCHAPACK_FISH to one)"
  else
    fh=$work/fishhome; mkdir -p "$fh"; fc="$work/fish cache 'q'"; rm -rf "${fc:?}"
    # frun <dir> <fish commands>: output in $work/out, exit in $rc, START:END in $win.
    frun() {
      local d=$1 c=$2 s; : > "$rt/starts"; s=$(now_ns)
      (cd "$d" && env -u MATCHAPACK_YARN_JS -u MATCHAPACK_ENGINE -u MATCHAPACK_PATHS -u MATCHAPACK -u GITHUB_TOKEN \
        PATH="$rt/bin:/usr/bin:/bin" HOME="$fh" XDG_CONFIG_HOME="$fh/.config" XDG_DATA_HOME="$fh/.local/share" \
        YARN_DISABLE_SELF_UPDATE_CHECK=true MATCHAPACK_STORE="$work/y1/store" MATCHAPACK_LOG_DIR="$work/y1/log" MATCHAPACK_CONFIG_DIR="$work/y1/config" \
        "$fish" --no-config -c "type -q matchapack; and matchapack init fish | source; $c") > "$work/out" 2>&1; rc=$?
      win=$s:$(now_ns)
    }
    yargs="install --frozen-lockfile --non-interactive --cache-folder \"$fc\""
    frun "$work" 'type -t yarn; type -t npm'
    if [ $rc = 0 ] && [ "$(cat "$work/out")" = "function
function" ]; then ok "route fish ($("$fish" --version 2>&1)): the real init defines the yarn and npm functions"
    else bad "route fish: functions (exit $rc)"; fi
    d=$(project fprime); frun "$d" "command yarn $yargs"; r0=$rc
    da=$(project fplain); frun "$da" "command yarn $yargs"; ra=$rc wa=$win sa=$(starts)
    db=$(project ffunc); frun "$db" "yarn $yargs"; rb=$rc wb=$win sb=$(starts)
    n=$(lastlog 'r["yarn1"]["cloned"]')
    "$bin" diff --meta --normalize-roots "$da" "$db" --a-run "$wa" --b-run "$wb" --a-cache "$fc" --b-cache "$fc" > "$work/fish-diff.txt" 2>&1; rd=$?
    if [ "$r0" = 0 ] && [ "$ra" = 0 ] && [ "$rb" = 0 ] && [ "$sa" = 0 ] && [ "$sb" = 1 ] && [ "$rd" = 0 ] && [ "${n:-0}" -gt 0 ] \
      && [ "$(lastlog 'r["args"]')" = "['install', '--frozen-lockfile', '--non-interactive', '--cache-folder', \"$fc\"]" ]; then
      ok "route fish: yarn install → matchapack yarn, arguments unchanged (a cache path with a space and a quote); tree identical to plain (diff --meta); $n files cloned"
    else bad "route fish: install (prime $r0, plain $ra/$sa starts, function $rb/$sb starts, diff $rd, cloned ${n:-?}, args $(lastlog 'r["args"]' 2>/dev/null))"; grep -v '^      ' "$work/fish-diff.txt" | tail -15; fi
    d=$(project fadd); frun "$d" "yarn add http://127.0.0.1:48321/y1-c-1.0.0.tgz --non-interactive --cache-folder \"$fc\""
    if [ $rc = 0 ] && [ "$(starts)" = 1 ] && grep -q '"y1-c"' "$d/package.json" && [ "$(lastlog 'r["command"]')" = add ]; then ok "route fish: yarn add → matchapack yarn"
    else bad "route fish: yarn add (exit $rc, starts $(starts))"; fi
    d=$(project ffailplain); rootfail "$d"; frun "$d" "command yarn $yargs; echo \"status \$status\""; want=$(tail -1 "$work/out")
    d=$(project ffail); rootfail "$d"; frun "$d" "yarn $yargs; echo \"status \$status\""
    if [ "$want" != "status 0" ] && [ "$(tail -1 "$work/out")" = "$want" ] && [ "$(starts)" = 1 ] && [ "$(lastlog 'r["outcome"]')" = yarn_failed ]; then
      ok "route fish: a failing install's status ($want) passes through the function, as plain Yarn's"
    else bad "route fish: failing install (plain $want, got $(tail -1 "$work/out"), starts $(starts))"; fi
  fi
fi

# --- scripts/convert-existing.sh on Yarn 1 projects (macOS) -------------------
# Scratch projects installed by plain Yarn; the backups go to a scratch HOME.
if [ "$(uname)" = Darwin ]; then
  cv=$work/conv; rm -rf "${cv:?}"; mkdir -p "$cv"
  # cinstall <dir> [yarn args]: plain Yarn installs it.
  cinstall() { local d=$1; shift; (cd "$d" && y1env "$node" "$yarnjs" install --non-interactive --ignore-scripts --cache-folder "$work/e-many/cache" "$@") > /dev/null 2>&1; }
  # cproj <name> <fixture> [root]: a copy installed by plain Yarn.
  cproj() {
    local d=${3:-$cv}/$1; mkdir -p "$d"; cp -R "$here/fixtures/$2/." "$d/"; rm -f "$d/README.md"
    cinstall "$d" --frozen-lockfile; echo "$d"
  }
  y1=$(cproj y1 yarn1-many)
  ws=$(cproj ws yarn1-workspaces); echo keep > "$ws/packages/w1/node_modules/y1-b/SENTINEL"
  # Workspace patterns Yarn expands itself: `**` (any depth) and `{x,y}`.
  wg=$cv/wsglob; mkdir -p "$wg/group/sub/deep" "$wg/x" "$wg/y"
  printf '{"name":"wsglob","private":true,"workspaces":["group/**","{x,y}"],"dependencies":{"y1-b":"http://127.0.0.1:48321/y1-b-2.0.0.tgz","y1-many":"http://127.0.0.1:48321/y1-many-1.0.0.tgz"}}' > "$wg/package.json"
  printf '{"name":"deep","version":"1.0.0","dependencies":{"y1-b":"http://127.0.0.1:48321/y1-b-1.0.0.tgz"}}' > "$wg/group/sub/deep/package.json"
  printf '{"name":"wx","version":"1.0.0","dependencies":{"y1-b":"http://127.0.0.1:48321/y1-b-1.0.0.tgz"}}' > "$wg/x/package.json"
  printf '{"name":"wy","version":"1.0.0"}' > "$wg/y/package.json"
  cinstall "$wg"; echo keep > "$wg/group/sub/deep/node_modules/y1-b/SENTINEL"; echo keep > "$wg/x/node_modules/y1-b/SENTINEL"
  # `yarn link`-like: an installed package replaced by a symlink (its name
  # is in yarn.lock), plain and scoped; a package that is not in yarn.lock.
  mkdir -p "$work/linked-src"; cp -R "$here/fixtures/yarn1-many/package.json" "$work/linked-src/"
  lk=$(cproj linked yarn1-many); rm -rf "${lk:?}/node_modules/y1-a"; ln -s "$work/linked-src" "$lk/node_modules/y1-a"
  ls2=$(cproj linkedscoped yarn1-many); rm -rf "${ls2:?}/node_modules/@y1/scoped"; ln -s "$work/linked-src" "$ls2/node_modules/@y1/scoped"
  ex=$(cproj extra yarn1-many); mkdir -p "$ex/node_modules/manual-copy"
  fl=$(cproj fail yarn1-many); rootfail "$fl"; echo keep > "$fl/node_modules/y1-a/SENTINEL"
  dc=$(cproj declined yarn1-many); printf 'yarn-path "%s"\n' "$yarnjs" > "$dc/.yarnrc"; echo keep > "$dc/node_modules/y1-a/SENTINEL"
  # A backup that cannot be made (an unreadable file): nothing is removed.
  nb=$(cproj nobackup yarn1-many); echo keep > "$nb/node_modules/y1-a/UNREADABLE"; chmod 000 "$nb/node_modules/y1-a/UNREADABLE"
  # The same for an npm project (the npm path had the same unchecked backup).
  nbn=$cv/nobackup-npm; mkdir -p "$nbn/node_modules/lodash"; cp "$here/fixtures/small/package.json" "$here/fixtures/small/package-lock.json" "$nbn/"
  echo keep > "$nbn/node_modules/lodash/UNREADABLE"; chmod 000 "$nbn/node_modules/lodash/UNREADABLE"
  y4=$cv/yarn4; mkdir -p "$y4/node_modules/x"; printf '{"name":"y4","packageManager":"yarn@4.5.0"}' > "$y4/package.json"; printf '__metadata:\n  version: 8\n' > "$y4/yarn.lock"
  y1ino=$(ls -di "$y1/node_modules" | awk '{print $1}')
  conv() { # conv <root> [script args]   (CONV_PREFIX: a directory put first on PATH)
    local r=$1; shift
    (cd "$work" && env -u MATCHAPACK_YARN_JS -u MATCHAPACK_ENGINE -u MATCHAPACK_PATHS -u MATCHAPACK -u GITHUB_TOKEN PATH="${CONV_PREFIX:+$CONV_PREFIX:}$rt/bin:$PATH" \
      HOME="$work/convhome" YARN_CACHE_FOLDER="$work/e-many/cache" YARN_DISABLE_SELF_UPDATE_CHECK=true MATCHAPACK_STORE="$work/y1/store" \
      MATCHAPACK_LOG_DIR="$work/y1/log" MATCHAPACK_CONFIG_DIR="$work/y1/config" "$here/scripts/convert-existing.sh" "$@" "$r") > "$work/out" 2>&1
  }
  conv "$cv"; rc=$?
  if [ $rc = 0 ] && grep -q "^CANDIDATE yarn *$y1\$" "$work/out" && grep -q "^CANDIDATE yarn *$ws\$" "$work/out" && grep -q "^CANDIDATE yarn *$wg\$" "$work/out" \
    && grep -q "^SKIP Yarn 2+ *$y4 " "$work/out" && grep -q "^SKIP not in lock $ex .*manual-copy" "$work/out" \
    && [ "$(ls -di "$y1/node_modules" | awk '{print $1}')" = "$y1ino" ] && [ -f "$ws/packages/w1/node_modules/y1-b/SENTINEL" ]; then
    ok "convert-existing (dry run): Yarn 1 candidates listed (workspaces with ** and {x,y} included), Yarn 2+ and unlisted packages skipped, nothing changed"
  else bad "convert-existing dry run (exit $rc)"; fi
  if grep -q "^SKIP linked *$lk .*y1-a" "$work/out" && grep -q "^SKIP linked *$ls2 .*@y1/scoped" "$work/out"; then
    ok "convert-existing: a package replaced by a symlink (yarn link), plain or scoped, skips the project"
  else bad "convert-existing: linked packages"; fi
  n0=$(wc -l < "$work/y1/log/log.jsonl")
  conv "$cv" --apply; rc=$?
  chmod 644 "$nb/node_modules/y1-a/UNREADABLE" "$nbn/node_modules/lodash/UNREADABLE"
  inst=$(tail -n +$((n0+1)) "$work/y1/log/log.jsonl" | grep -c '"outcome":"installed"')
  if grep -q "^CONVERTED *$y1 .*files cloned from Yarn" "$work/out" && grep -q "^CONVERTED *$ws " "$work/out" && [ "$inst" = 3 ] \
    && [ ! -e "$ws/packages/w1/node_modules/y1-b/SENTINEL" ] && [ -f "$ws/packages/w1/node_modules/y1-b/package.json" ] \
    && grep -q "^FAILED (rc=7) *$fl " "$work/out" && [ -f "$fl/node_modules/y1-a/SENTINEL" ] \
    && grep -q "^SKIP not handled $dc .*yarn-path" "$work/out" && [ -f "$dc/node_modules/y1-a/SENTINEL" ] \
    && [ -d "$y4/node_modules/x" ] && [ -L "$lk/node_modules/y1-a" ] && [ -z "$(ls -A "$work/convhome/.cache/matchapack/backup" 2>/dev/null)" ]; then
    ok "convert-existing --apply: Yarn 1 root and workspace node_modules rebuilt with clones; a failed and a declined install put back; no backup left"
  else bad "convert-existing --apply (exit $rc)"; fi
  if grep -q "^CONVERTED *$wg " "$work/out" && [ ! -e "$wg/group/sub/deep/node_modules/y1-b/SENTINEL" ] && [ ! -e "$wg/x/node_modules/y1-b/SENTINEL" ] \
    && [ -f "$wg/group/sub/deep/node_modules/y1-b/package.json" ] && [ -f "$wg/x/node_modules/y1-b/package.json" ]; then
    ok "convert-existing --apply: workspaces matched by group/** and {x,y} (as Yarn lists them) rebuilt too"
  else bad "convert-existing: workspace patterns"; fi
  if grep -q "^SKIP no backup *$nb .*copying .* failed" "$work/out" && [ -f "$nb/node_modules/y1-a/UNREADABLE" ] && [ -f "$nb/node_modules/.yarn-integrity" ] \
    && grep -q "^SKIP no backup *$nbn .*copying .* failed" "$work/out" && [ -f "$nbn/node_modules/lodash/UNREADABLE" ]; then
    ok "convert-existing --apply: a backup that cannot be made skips the project (Yarn and npm), node_modules untouched"
  else bad "convert-existing: failed backup"; fi
  # Interrupted during the install (SIGINT to the script's process group, as
  # Ctrl-C sends it): the previous node_modules is put back.
  ci=$work/conv-intr; rm -rf "${ci:?}"; mkdir -p "$ci"
  it=$(cproj intr yarn1-many "$ci"); echo keep > "$it/node_modules/y1-a/SENTINEL"
  python3 -c 'import json,sys; p=sys.argv[1]; d=json.load(open(p)); d["scripts"]={"postinstall":"node -e \"require(\x27fs\x27).writeFileSync(\x27started\x27,\x27\x27); setTimeout(()=>{},60000)\""}; json.dump(d, open(p,"w"))' "$it/package.json"
  set -m; conv "$ci" --apply & pid=$!; set +m
  for _ in $(seq 600); do [ -e "$it/started" ] && break; sleep 0.1; done
  started=no; [ -e "$it/started" ] && started=yes
  kill -INT -- -"$pid" 2>/dev/null; wait "$pid"; rc=$?
  if [ $started = yes ] && [ $rc = 130 ] && grep -q "^INTERRUPTED *$it " "$work/out" && [ -f "$it/node_modules/y1-a/SENTINEL" ] \
    && [ -z "$(ls -A "$work/convhome/.cache/matchapack/backup" 2>/dev/null)" ]; then
    ok "convert-existing --apply interrupted during the install: previous node_modules put back, exit 130"
  else bad "convert-existing interrupted (started $started, exit $rc)"; fi
  # Review round 2. croot <name>: a fresh root of its own for one conv run.
  croot() { local r=$work/conv-$1; rm -rf "${r:?}"; mkdir -p "$r"; echo "$r"; }
  nobackups() { [ -z "$(ls -A "$work/convhome/.cache/matchapack/backup" 2>/dev/null)" ]; }
  # (1) Workspace planning reads Yarn's JSON from stdout only, and a plan that
  # cannot be made skips the project. A Yarn 1 package whose bin/yarn
  # launcher (a Homebrew-like layout) prints a warning on stderr for every
  # command and, with FAKE_WS_JSON=bad, prints something that is not JSON
  # for `workspaces info`; installs run its bin/yarn.js directly.
  fy=$work/fake-yarn; rm -rf "${fy:?}"; mkdir -p "$fy/bin"; cp -R "$(dirname "$(dirname "$yarnjs")")/." "$fy/"
  printf '#!/bin/sh\necho "warning: a warning from Yarn" >&2\ncase "$FAKE_WS_JSON $*" in "bad "*workspaces*) echo "{not json"; exit 0 ;; esac\nexec node "%s/bin/yarn.js" "$@"\n' "$fy" > "$fy/bin/yarn"
  chmod +x "$fy/bin/yarn"; mkdir -p "$work/fybin"; ln -sf "$fy/bin/yarn" "$work/fybin/yarn"
  r=$(croot warn); w1=$(cproj ws yarn1-workspaces "$r"); echo keep > "$w1/packages/w1/node_modules/y1-b/SENTINEL"
  CONV_PREFIX=$work/fybin conv "$r" --apply; rc=$?
  if [ $rc = 0 ] && grep -q "^CONVERTED *$w1 " "$work/out" && [ ! -e "$w1/packages/w1/node_modules/y1-b/SENTINEL" ]; then
    ok "convert-existing: Yarn's warnings on stderr do not reach the workspace list (workspace node_modules rebuilt)"
  else bad "convert-existing: workspace list with warnings (exit $rc)"; fi
  r=$(croot badjson); w2=$(cproj ws yarn1-workspaces "$r"); echo keep > "$w2/node_modules/y1-b/SENTINEL"
  CONV_PREFIX=$work/fybin FAKE_WS_JSON=bad conv "$r" --apply; rc=$?
  if grep -q "^SKIP no plan *$w2 " "$work/out" && [ -f "$w2/node_modules/y1-b/SENTINEL" ] && nobackups; then
    ok "convert-existing: a workspace list that is not JSON skips the project before anything changes"
  else bad "convert-existing: bad workspace list (exit $rc)"; fi
  # (2) A signal while a failed install is being rolled back (between two
  # put-backs): each node_modules goes back exactly once.
  r=$(croot rollback); w3=$(cproj ws yarn1-workspaces "$r"); rootfail "$w3"
  echo keep > "$w3/node_modules/y1-b/SENTINEL"; echo keep > "$w3/packages/w1/node_modules/y1-b/SENTINEL"
  set -m; MATCHAPACK_CONVERT_TEST_PAUSE_RESTORE=3 conv "$r" --apply & pid=$!; set +m
  for _ in $(seq 600); do [ -f "$w3/node_modules/y1-b/SENTINEL" ] && [ ! -e "$w3/packages/w1/node_modules/y1-b/SENTINEL" ] && break; sleep 0.05; done
  kill -INT -- -"$pid" 2>/dev/null; wait "$pid"; rc=$?
  if grep -q "^FAILED (rc=7) *$w3 .*previous node_modules restored" "$work/out" && [ -f "$w3/node_modules/y1-b/SENTINEL" ] \
    && [ -f "$w3/packages/w1/node_modules/y1-b/SENTINEL" ] && nobackups; then
    ok "convert-existing: a signal during a rollback does not undo what was put back (both node_modules restored)"
  else bad "convert-existing: signal during rollback (exit $rc)"; fi
  # (3) The tree a failed install left cannot be removed (an immutable file):
  # the backup is kept where it is, never moved inside that tree.
  r=$(croot stuck); w4=$(cproj stuck yarn1-many "$r"); echo keep > "$w4/node_modules/y1-a/SENTINEL"
  python3 -c 'import json,sys; p=sys.argv[1]; d=json.load(open(p)); d["scripts"]={"postinstall":"mkdir -p node_modules/stuck && touch node_modules/stuck/f && chflags uchg node_modules/stuck/f && exit 7"}; json.dump(d, open(p,"w"))' "$w4/package.json"
  conv "$r" --apply; rc=$?
  kept=$(sed -n 's/.*could not remove .*: its previous node_modules is kept at \(.*\)$/\1/p' "$work/out" | head -1)
  chflags nouchg "$w4/node_modules/stuck/f" 2>/dev/null
  if grep -q "^FAILED (rc=7) *$w4 .*NOT fully restored" "$work/out" && [ -n "$kept" ] && [ -f "$kept/y1-a/SENTINEL" ] && [ ! -e "$w4/node_modules/0" ]; then
    ok "convert-existing: a tree that cannot be removed keeps the backup in place (said where), not moved inside it"
  else bad "convert-existing: unremovable tree (exit $rc)"; fi
  # (round 3) The next run neither removes nor reuses a backup an earlier run
  # kept: the project is skipped, pointing at it.
  conv "$r" --apply; rc=$?
  if [ -n "$kept" ] && grep -q "^SKIP old backup *$w4 .*$(dirname "$kept");" "$work/out" && [ -f "$kept/y1-a/SENTINEL" ]; then
    ok "convert-existing: a backup kept by an earlier run is left alone and its project skipped, pointing at it"
  else bad "convert-existing: kept backup on the next run (exit $rc)"; fi
  rm -rf "${work:?}/convhome/.cache/matchapack/backup"
  # (4) A process enters a project after the start: checked again just before
  # its node_modules is removed. Project a waits for `go`; meanwhile a
  # process starts in project b.
  r=$(croot inuse); wa=$(cproj a yarn1-many "$r"); wb=$(cproj b yarn1-many "$r"); echo keep > "$wb/node_modules/y1-a/SENTINEL"
  python3 -c 'import json,sys; p=sys.argv[1]; d=json.load(open(p)); d["scripts"]={"postinstall":"node -e \"const fs=require(\x27fs\x27); fs.writeFileSync(\x27started\x27,\x27\x27); const t=setInterval(()=>{ if (fs.existsSync(\x27go\x27)) clearInterval(t) }, 50)\""}; json.dump(d, open(p,"w"))' "$wa/package.json"
  conv "$r" --apply & pid=$!
  for _ in $(seq 600); do [ -e "$wa/started" ] && break; sleep 0.1; done
  (cd "$wb" && exec sleep 60) & sp=$!
  sleep 0.5; touch "$wa/go"; wait "$pid"; rc=$?; kill "$sp" 2>/dev/null; wait "$sp" 2>/dev/null
  if [ $rc = 0 ] && grep -q "^CONVERTED *$wa " "$work/out" && grep -q "^SKIP in use *$wb .*entered it during this run" "$work/out" && [ -f "$wb/node_modules/y1-a/SENTINEL" ] && nobackups; then
    ok "convert-existing: a project a process entered during the run is skipped just before removal, untouched"
  else bad "convert-existing: in use during the run (exit $rc)"; fi
  # (5) A file whose group differs from the backup folder's: a clone takes
  # the folder's group; the backup must keep the file's, so the rollback of a
  # failed install restores it.
  g=$(groups_other "$work")
  r=$(croot group); w5=$(cproj grp yarn1-many "$r"); rootfail "$w5"
  if [ -z "$g" ]; then echo "SKIP convert-existing groups: the user has only one group"
  else
    chgrp "$g" "$w5/node_modules/y1-a/package.json"
    conv "$r" --apply; rc=$?
    if grep -q "^FAILED (rc=7) *$w5 .*previous node_modules restored" "$work/out" && [ "$(stat -f %g "$w5/node_modules/y1-a/package.json")" = "$g" ]; then
      ok "convert-existing: the backup keeps each file's group (a clone takes its folder's); restored with it"
    else bad "convert-existing: group in the backup (exit $rc, group $(stat -f %g "$w5/node_modules/y1-a/package.json"), want $g)"; fi
  fi
  # (round 3) A workspace without node_modules at planning time: the failed
  # install made one; the rollback removes it (it was not there before).
  r=$(croot newnm); w7=$(cproj ws yarn1-workspaces "$r"); rootfail "$w7"; echo keep > "$w7/node_modules/y1-b/SENTINEL"
  rm -rf "${w7:?}/packages/w1/node_modules"
  conv "$r" --apply; rc=$?
  if grep -q "^FAILED (rc=7) *$w7 .*previous node_modules restored" "$work/out" && [ -f "$w7/node_modules/y1-b/SENTINEL" ] && [ ! -e "$w7/packages/w1/node_modules" ] && nobackups; then
    ok "convert-existing: a rollback removes a workspace node_modules the install made where there was none"
  else bad "convert-existing: new workspace node_modules on rollback (exit $rc)"; fi
  # (round 3) npm: "added N" is the number of clones (src/wrapper.rs), and
  # only N = 0 is nothing cloned. A stand-in `matchapack` prints the inject
  # engine's summary line (its format checked against the source).
  if grep -qF '"matchapack: added {} packages from the store in {:.1}s (inject engine; npm extracted {by_npm})"' "$here/src/wrapper.rs"; then
    mkdir -p "$work/fakemp"
    printf '#!/bin/sh\ncase "$1 $2" in "npm ci") rm -rf node_modules; mkdir -p node_modules/lodash; echo new > node_modules/lodash/NEW\n  echo "matchapack: added $FAKE_ADDED packages from the store in 0.1s (inject engine; npm extracted $FAKE_EXTRACTED)" >&2; exit 0 ;; esac\nexit 9\n' > "$work/fakemp/matchapack"
    chmod +x "$work/fakemp/matchapack"
    npmproj() { local d=$1/n; mkdir -p "$d/node_modules/lodash"; cp "$here/fixtures/small/package.json" "$here/fixtures/small/package-lock.json" "$d/"; echo keep > "$d/node_modules/lodash/SENTINEL"; echo "$d"; }
    r=$(croot npm-some); n1=$(npmproj "$r"); CONV_PREFIX=$work/fakemp FAKE_ADDED=2 FAKE_EXTRACTED=3 conv "$r" --apply; o1=$(cat "$work/out")
    r=$(croot npm-none); n2=$(npmproj "$r"); CONV_PREFIX=$work/fakemp FAKE_ADDED=0 FAKE_EXTRACTED=5 conv "$r" --apply; o2=$(cat "$work/out")
    if printf '%s\n' "$o1" | grep -q "^CONVERTED *$n1 " && [ -f "$n1/node_modules/lodash/NEW" ] \
      && printf '%s\n' "$o2" | grep -q "^NOT CONVERTED *$n2 .*previous node_modules restored" && [ -f "$n2/node_modules/lodash/SENTINEL" ] && [ ! -e "$n2/node_modules/lodash/NEW" ] && nobackups; then
      ok "convert-existing (npm, inject summary): 2 cloned and 3 extracted is a conversion; 0 cloned is not, and is rolled back"
    else bad "convert-existing: npm clone count ($(printf '%s' "$o1$o2" | grep -E 'CONVERTED|FAILED|SKIP' | tr '\n' '|'))"; fi
  else bad "convert-existing: the inject engine's summary line in src/wrapper.rs changed; update this test and the script"; fi
  # (round 4) Planning skips what it cannot back up or check as planned: a
  # workspace node_modules that is a symlink; a nested linked package; a
  # nested package not in yarn.lock.
  r=$(croot odd); w8=$(cproj ws yarn1-workspaces "$r"); mkdir -p "$work/odd-target"
  mv "$w8/packages/w1/node_modules" "$work/odd-target/nm"; ln -s "$work/odd-target/nm" "$w8/packages/w1/node_modules"
  n9=$(cproj nestlink yarn1-many "$r"); mkdir -p "$n9/node_modules/y1-a/node_modules"; ln -s "$work/linked-src" "$n9/node_modules/y1-a/node_modules/linked-nested"
  n10=$(cproj nestextra yarn1-many "$r"); mkdir -p "$n10/node_modules/y1-a/node_modules/not-in-lock"
  conv "$r"; rc=$?
  if [ $rc = 0 ] && grep -q "^SKIP odd *$w8 .*packages/w1/node_modules" "$work/out" && grep -q "^SKIP linked *$n9 .*node_modules/y1-a/node_modules/linked-nested" "$work/out" \
    && grep -q "^SKIP not in lock $n10 .*node_modules/y1-a/node_modules/not-in-lock" "$work/out"; then
    ok "convert-existing: a symlinked workspace node_modules, and a linked or unlisted package in a nested node_modules, skip the project"
  else bad "convert-existing: odd and nested node_modules (exit $rc)"; fi
  # (round 4) Interrupted while a process that ignores SIGINT stays in the
  # project: no restore under it; the backup is kept and its place said.
  # (Its output goes elsewhere: a process that keeps the install's output
  # pipe open holds the script until it exits; NOTES, residual risk.)
  r=$(croot stay); w11=$(cproj stay yarn1-many "$r"); echo keep > "$w11/node_modules/y1-a/SENTINEL"
  python3 -c 'import json,sys; p=sys.argv[1]; d=json.load(open(p)); d["scripts"]={"postinstall":"sh -c \"trap \x27\x27 INT; exec sleep 60\" </dev/null >/dev/null 2>&1 & echo $! > stayer.pid; touch started; sleep 60"}; json.dump(d, open(p,"w"))' "$w11/package.json"
  set -m; MATCHAPACK_CONVERT_TEST_WAIT_TICKS=4 conv "$r" --apply & pid=$!; set +m
  for _ in $(seq 600); do [ -e "$w11/started" ] && break; sleep 0.1; done
  kill -INT -- -"$pid" 2>/dev/null; wait "$pid"; rc=$?
  stayer=$(cat "$w11/stayer.pid" 2>/dev/null)
  kb=$(sed -n 's/.*not restored automatically; the previous node_modules is kept at \([^,]*\),.*/\1/p' "$work/out")
  if [ $rc = 130 ] && [ -n "$kb" ] && [ -f "$kb/0/y1-a/SENTINEL" ] && [ ! -f "$w11/node_modules/y1-a/SENTINEL" ]; then
    ok "convert-existing: interrupted while a project process stays: not restored under it, the backup kept and named"
  else bad "convert-existing: interrupt with a process staying (exit $rc)"; fi
  [ -n "$stayer" ] && kill "$stayer" 2>/dev/null
  rm -rf "${work:?}/convhome/.cache/matchapack/backup"
  # (round 4) A root given through a symlink: the in-use check still matches
  # lsof's physical working directories.
  r=$(croot viasym); w12=$(cproj p yarn1-many "$r"); ln -sfn "$r" "$work/conv-viasym-link"
  (cd "$w12" && exec sleep 60) & sp=$!
  sleep 0.3; conv "$work/conv-viasym-link"; rc=$?; kill "$sp" 2>/dev/null; wait "$sp" 2>/dev/null
  if [ $rc = 0 ] && grep -q "^SKIP in use *$w12\$" "$work/out"; then ok "convert-existing: a root given through a symlink: a project in use is still seen as in use"
  else bad "convert-existing: root through a symlink (exit $rc)"; fi
  # (6) An install that cloned nothing (every file a Node copy; the hook in
  # `off` mode, as when Yarn's cache is on another volume) is not a
  # conversion: the previous node_modules is put back.
  r=$(croot noclone); w6=$(cproj nc yarn1-many "$r"); echo keep > "$w6/node_modules/y1-a/SENTINEL"
  MATCHAPACK_YARN_HOOK=off conv "$r" --apply; rc=$?
  if [ $rc = 0 ] && grep -q "^NOT CONVERTED *$w6 .*nothing was cloned.*previous node_modules restored" "$work/out" && [ -f "$w6/node_modules/y1-a/SENTINEL" ] && nobackups; then
    ok "convert-existing: an install that cloned nothing is reported as not converted, previous node_modules put back"
  else bad "convert-existing: zero clones (exit $rc)"; fi
fi
fi # TEST_YARN1_PART != engine

# Two volumes: the project on a disk image, Yarn's cache on the data volume.
# Nothing can be cloned; the install is plain Yarn's copies.
# The work directory (trees) on the image, the cache given with --cache-dir.
vol=$work/vol
dmg=$work/vol.dmg
if [ "$(uname)" != Darwin ]; then echo "SKIP two volumes: macOS only"
elif ! hdiutil create -quiet -ov -size 64m -fs APFS -volname mpy1 "$dmg" 2>/dev/null; then echo "SKIP two volumes: no disk image could be made"
elif ! { mkdir -p "$vol" && hdiutil attach -quiet -nobrowse -mountpoint "$vol" "$dmg" 2>/dev/null; }; then
  rm -f "$dmg"; echo "SKIP two volumes: the disk image could not be attached"
else
  verify vol --work "$vol/w" --cache-dir "$work/e-many/cache" --engine yarn1 --engine-min-clones 0 "$here/fixtures/yarn1-many"
  cp "$vol/w/engine.json" "$work/vol-engine.json" 2>/dev/null
  cache_note=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["yarn1"]["cache"])' "$work/vol-engine.json" 2>/dev/null)
  if [ $rc = 0 ] && same && [ "${cache_note#unusable: }" != "$cache_note" ]; then ok "two volumes: nothing cloned (${cache_note#unusable: }), tree identical to plain"
  else bad "two volumes (exit $rc, cache '$cache_note')"; fi
  hdiutil detach -quiet "$vol" || hdiutil detach -quiet -force "$vol"
  rm -f "$dmg"
fi
mkdir -p "$work/withnm"; cp "$here/fixtures/yarn1-small/package.json" "$here/fixtures/yarn1-small/yarn.lock" "$work/withnm/"; mkdir -p "$work/withnm/node_modules"
verify withnm "$work/withnm"
if [ $rc = 2 ]; then ok "a source with node_modules is refused"; else bad "source with node_modules (exit $rc)"; fi

if [ "$(ls -l ~/.yarnrc 2>/dev/null || echo absent)" = "$yarnrc_before" ]; then ok "~/.yarnrc untouched"; else bad "~/.yarnrc changed"; fi
echo "passed $pass, failed $fail"
[ $fail = 0 ]
