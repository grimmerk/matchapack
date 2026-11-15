#!/usr/bin/env bash
# The npm upgrade routine (#11): run the harness on the npm on PATH.
#   verify-npm.sh [--no-record] <work-dir> [extra project dir...]
# Cases: the fixtures (hidden lockfile byte-identical; rootpath writes a file
# named after its own directory), script-env through the shell function,
# legacy-peer with legacy-peer-deps, then each extra project (a real
# repository is worth adding: the fixtures are small). Every case must have
# cloned something (a run in which the hook never fired would pass without
# testing it). Nothing is recorded: the version gate is by npm major
# (INJECT_NPM_MAJORS in src/wrapper.rs), and inside a major by the pacote
# fingerprint (src/npm-fingerprints.json; the cases run on any, and the
# first line says whether this npm's is listed). --no-record is accepted and ignored
# (it turned off recording into npm-verified, the classic engine's gate,
# removed in PR C2).
set -uo pipefail
if [ "${1-}" = --no-record ]; then shift; fi
work=${1:?usage: verify-npm.sh [--no-record] <work-dir> [extra project dir...]}; shift
here=$(cd "$(dirname "$0")/.." && pwd)
bin=$here/target/release/matchapack
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 2; }
mkdir -p "$work"
npm_version=$(npm -v) || { echo "no npm on PATH" >&2; exit 2; }
# What is certified is fixed here, never inherited: a caller's settings that
# loosen the comparison (ignored paths, noise filtering, another baseline,
# mtime handling) or route the install differently do not apply.
unset MATCHAPACK_VERIFY_IGNORE MATCHAPACK_VERIFY_NOISE MATCHAPACK_VERIFY_BASELINE MATCHAPACK_VERIFY_MTIME MATCHAPACK_VERIFY_VIA MATCHAPACK_VERIFY_BYTES MATCHAPACK_VERIFY_CLONES MATCHAPACK_VERIFY_META
# The engine is fixed too: a caller's MATCHAPACK_ENGINE other than inject
# would make every install an error.
unset MATCHAPACK_ENGINE
# The fixtures use a store of their own in the work directory, never the
# user's, unless the caller names one. verify.sh puts each case's trial log
# (MATCHAPACK_LOG_DIR) and its trial scope (MATCHAPACK_PATHS) in the work
# directory too; nothing else is written under ~/.cache/matchapack or
# ~/.config/matchapack (the run's helper files go to <store>/tmp).
export MATCHAPACK_STORE=${MATCHAPACK_STORE:-$(cd "$work" && pwd -P)/store}
export MATCHAPACK_VERIFY_CLONES=some
# npm 12 refuses tarballs from a non-registry URL unless told otherwise; the
# local fixtures install from 127.0.0.1 (as in test-inject.sh).
[ "${npm_version%%.*}" -ge 12 ] && export npm_config_allow_remote=all
echo "npm $npm_version, node $(node -v), $("$bin" --version)"
# The cases run whatever this npm's pacote fingerprint is (verify.sh); say
# whether the binary would take it without an override.
# npm's package directory as npm itself reports it (npm_execpath, its
# bin/npm-cli.js): also right behind a version manager's shim, where the
# `npm` on PATH is not a symlink into the package.
npm_dir=$(cd "$(dirname "$(npm exec -c 'printf %s "$npm_execpath"')")/.." && pwd)
fp=$(node "$here/src/inject.js" --fingerprint "$npm_dir")
if grep -q "\"$fp\"" "$here/src/npm-fingerprints.json"; then
  echo "pacote fingerprint $fp: listed in src/npm-fingerprints.json"
else
  echo "pacote fingerprint $fp: NOT listed in src/npm-fingerprints.json; add it once this passes (NOTES \"G-lite\")"
fi

# The optional-upward fixture installs from a local tarball server.
server=
if ! curl -sf -o /dev/null http://127.0.0.1:48321/lc-q-1.0.0.tgz; then
  python3 -m http.server 48321 --bind 127.0.0.1 --directory "$here/fixtures/optional-upward/tarballs" >/dev/null 2>&1 &
  server=$!
  for _ in $(seq 100); do curl -sf -o /dev/null http://127.0.0.1:48321/lc-q-1.0.0.tgz && break; sleep 0.1; done
fi
trap '[ -n "$server" ] && kill "$server" 2>/dev/null' EXIT

fail=0
case_() { # name env-assignments... -- source [matchapack install args...]
  local name=$1; shift
  local envs=()
  while [ "$1" != -- ]; do envs+=("$1"); shift; done
  shift
  local out="$work/$name.txt"
  local src=$1; shift
  if env MATCHAPACK_VERIFY_BYTES=1 ${envs[@]+"${envs[@]}"} "$here/scripts/verify.sh" "$src" "$work/$name" "$@" > "$out" 2>&1; then
    echo "ok    $name ($(grep -m1 '^npm ci:' "$out"))"
  else
    fail=1; echo "FAIL  $name (log: $out)"; tail -15 "$out" | sed 's/^/      /'
  fi
}
# rootpath: an install script names a file after its own directory, so A and
# B have another path there; verify.sh's --normalize-roots must make them the
# same (issue grimmerk/matchapack-private#48). workspaces: `npm ci` at the root of an npm workspaces
# project (links, a workspace's own nested node_modules and .bin, an install
# script), also compared with --meta (times outside the runs, uid, gid,
# xattr names).
for f in small stale-flags optional-upward optional-engines legacy-peer bundled rootpath; do
  case_ "$f" -- "$here/fixtures/$f"
done
case_ workspaces MATCHAPACK_VERIFY_META=1 -- "$here/fixtures/workspaces"
# The root's predependencies / dependencies / postdependencies scripts,
# which npm ci runs (the classic engine did not; NOTES "Inject engine").
case_ dep-event -- "$here/fixtures/dep-event"
# A failing optional dependency script: npm ci removes the package.
case_ optional-script-fail -- "$here/fixtures/optional-script-fail"
# The same package placed by per-file clones (the Linux placement).
case_ small-file-clones -- "$here/fixtures/small" --clone-mode file
# Tarballs where an extractor other than pacote's can give another tree
# (a path listed twice; .gitignore next to .npmignore): the store is
# filled through pacote.
case_ odd-tarballs -- "$here/fixtures/odd-tarballs"
# script-env records the npm environment scripts see; the user's flags
# (--no-audit) reach matchapack only through the shell function.
case_ script-env MATCHAPACK_VERIFY_VIA=wrapper -- "$here/fixtures/script-env"
case_ legacy-peer-on npm_config_legacy_peer_deps=true -- "$here/fixtures/legacy-peer"
for p in "$@"; do
  case_ "project-$(basename "$p")" -- "$p"
done

# The install family (npm install, update, uninstall; NOTES "The install
# family"): scripts/verify-steps.sh runs the same npm commands with plain
# npm and through `matchapack npm`, from the same start, and compares after
# every step (tree, hidden lockfile bytes, package.json and
# package-lock.json bytes, npm ls, the hidden lockfile trusted, the log
# record and the gc record). fixtures/small installs from the public
# registry: added packages are pinned, and `update` and installs without a
# lockfile set npm's `before`, so both sides resolve against the same view
# of the registry (the owner's decision Q8).
old_day=2025-01-15; day=2026-11-01
steps_() { # name source [verify-steps.sh options] -- step [-- step ...]
  local name=$1 src=$2; shift 2
  local out="$work/steps-$name.txt"
  if "$here/scripts/verify-steps.sh" "$src" "$work/steps-$name" "$@" > "$out" 2>&1; then
    echo "ok    steps $name ($(grep -c '^step ' "$out") steps)"
  else
    fail=1; echo "FAIL  steps $name (log: $out)"; grep -E '^step|FAIL' "$out" | head -14 | sed 's/^/      /'
  fi
}
# The harness's own self-test: one byte of B's package-lock.json changed
# after the step must fail the run, by the tree comparison and by the line
# that names the file.
st=$work/steps-selftest.txt
if MATCHAPACK_VERIFY_STEPS_CORRUPT=package-lock.json "$here/scripts/verify-steps.sh" "$here/fixtures/workspaces" "$work/steps-selftest" --start ci -- install > "$st" 2>&1; then
  fail=1; echo "FAIL  steps self-test: a changed byte in package-lock.json was not noticed (log: $st)"
elif grep -q 'FAIL cmp: package-lock.json differs' "$st" && grep -q 'FAIL diff:' "$st"; then
  echo "ok    steps self-test (a changed byte in package-lock.json: diff and cmp both fail)"
else
  fail=1; echo "FAIL  steps self-test: the run failed, but not by both the diff and the cmp of package-lock.json (log: $st)"
fi
# The log check fails closed: a checker that dies before printing anything,
# a record of another npm command than the step's, a file B touched that
# plain npm left alone, and two log records for one step each fail the run.
for t in checker-dies:'the log check could not run' wrong-command:'want update' touch-b:'that plain npm left alone' extra-record:'grew by 2 records' untrust-b:'npm trusts the hidden lockfile'; do
  st=$work/steps-selftest-${t%%:*}.txt
  if MATCHAPACK_VERIFY_STEPS_SELFTEST=${t%%:*} "$here/scripts/verify-steps.sh" "$here/fixtures/workspaces" "$work/steps-selftest-${t%%:*}" --start ci -- install > "$st" 2>&1; then
    fail=1; echo "FAIL  steps self-test ${t%%:*}: the run passed (log: $st)"
  elif grep -F "${t#*:}" "$st" | grep -q FAIL; then
    echo "ok    steps self-test (${t%%:*}: the run fails)"
  else
    fail=1; echo "FAIL  steps self-test ${t%%:*}: the run failed, but not by the log check (log: $st)"
  fi
done
small=$here/fixtures/small; ws=$here/fixtures/workspaces
edit_pkg='node -e "const fs=require(\"fs\");const p=JSON.parse(fs.readFileSync(\"package.json\"));p.dependencies[\"left-pad\"]=\"1.3.0\";delete p.dependencies.lodash;fs.writeFileSync(\"package.json\",JSON.stringify(p,null,2)+\"\\n\")"'
# Starting without node_modules.
steps_ fresh-install         "$small" -- CLONES=some install
steps_ fresh-install-noscr   "$small" -- CLONES=some install --ignore-scripts
steps_ fresh-nolock          "$small" --start nolock -- npm_config_before=$day CLONES=some install
steps_ fresh-nested          "$small" --start nolock -- npm_config_before=$day CLONES=some install --install-strategy=nested --ignore-scripts
steps_ fresh-omit-dev        "$small" -- CLONES=some install --omit=dev
# On a tree installed by npm ci (plain in A, matchapack in B).
steps_ noop-install          "$small" --start ci -- CLONES=none install
steps_ add                   "$small" --start ci -- CLONES=1 install left-pad@1.3.0
steps_ add-dev-exact         "$small" --start ci -- CLONES=none install --save-dev --save-exact is-number@7.0.0
steps_ add-no-save           "$small" --start ci -- CLONES=1 install --no-save left-pad@1.3.0
steps_ add-two-i-alias       "$small" --start ci -- CLONES=1 i left-pad@1.3.0 is-number@7.0.0
steps_ change-version        "$small" --start ci -- CLONES=some install semver@7.6.0
steps_ uninstall             "$small" --start ci -- CLONES=none uninstall lodash
steps_ uninstall-two         "$small" --start ci -- CLONES=none uninstall express debug
steps_ update-all            "$small" --start ci -- npm_config_before=$day update
steps_ update-from-old-lock  "$small" --pre "rm -f package-lock.json; npm_config_before=$old_day \$NPM install --no-audit --no-fund --ignore-scripts" -- npm_config_before=$day CLONES=some update
steps_ update-one            "$small" --start ci -- npm_config_before=$day update semver
steps_ omit-dev-on-full      "$small" --start ci -- CLONES=none install --omit=dev
steps_ edited-package-json   "$small" --start ci --pre "$edit_pkg" -- CLONES=1 install
steps_ repair-missing-dirs   "$small" --start ci --pre 'rm -rf node_modules/lodash node_modules/express' -- CLONES=some install
# The same with the hook not installed: npm restores the two packages at the
# versions the hidden lockfile already had; both count as unseen.
steps_ repair-missing-nohook "$small" --start ci --pre 'rm -rf node_modules/lodash node_modules/express' -- MATCHAPACK_INJECT_HOOK=none CLONES=0 UNSEEN=2 install
steps_ no-hidden-lockfile    "$small" --start ci --pre 'rm -f node_modules/.package-lock.json' -- CLONES=1 install left-pad@1.3.0
steps_ no-package-lock       "$small" --start ci -- CLONES=some install --no-package-lock left-pad@1.3.0
# npm fails after packages were placed (an install script exits 1): the
# same status, npm's rollback, both trees equal.
steps_ fails-in-script       "$small" --start ci -- install left-pad@1.3.0 http://127.0.0.1:48321/lc-fail-1.0.0.tgz
# The flags the owner named (passed on to npm; each must be plain npm's).
steps_ flag-force            "$small" --start ci -- CLONES=1 install --force left-pad@1.3.0
steps_ flag-legacy-peer-deps "$small" --start ci -- CLONES=1 install --legacy-peer-deps left-pad@1.3.0
steps_ legacy-peer-nolock    "$here/fixtures/legacy-peer" --start nolock -- npm_config_before=$day CLONES=some install --legacy-peer-deps
steps_ flag-save-peer        "$small" --start ci -- CLONES=1 install --save-peer left-pad@1.3.0
steps_ flag-save-optional    "$small" --start ci -- CLONES=1 install --save-optional left-pad@1.3.0
# Workspaces (local tarballs).
steps_ ws-fresh-install      "$ws" -- CLONES=some install
steps_ ws-add-w              "$ws" --start ci -- CLONES=1 install left-pad@1.3.0 -w ws-app
steps_ ws-add-in-member      "$ws" --start ci -- @packages/app CLONES=1 install left-pad@1.3.0
steps_ ws-ci-in-member       "$ws" -- @packages/app CLONES=some ci
steps_ ws-update             "$ws" --start ci -- CLONES=none update
steps_ ws-uninstall-w        "$ws" --start ci -- CLONES=none uninstall y1-b -w ws-app
# Packages new in the hidden lockfile that npm keeps on disk (it takes their
# integrity from package-lock.json, extracts nothing): no "unseen" alarm.
# Only the root's node_modules removed (a member's own stays), and a hidden
# lockfile missing an entry (as after an npm stopped before saving it).
steps_ ws-root-nm-removed    "$ws" --start ci --pre 'rm -rf node_modules' -- CLONES=some install
# A package npm keeps that gets a nested node_modules in this step (chalk
# 2.4.2 under an added ansi-styles@4), with the hidden lockfile behind for
# it: its directory is new, its package.json is not; no "unseen" alarm.
mkfx() { # name package.json [npmrc] -> $work/fx-<name> (with package-lock.json unless npmrc says otherwise)
  local d=$work/fx-$1; rm -rf "${d:?}"; mkdir -p "$d"; echo "$2" > "$d/package.json"; [ -n "${3-}" ] && echo "$3" > "$d/.npmrc"
  if [ -z "${3-}" ]; then (cd "$d" && npm_config_before=$day npm install --package-lock-only --no-audit --no-fund > /dev/null 2>&1); fi
  echo "$d"
}
fx=$(mkfx nested '{"name":"nested","version":"1.0.0","dependencies":{"chalk":"2.4.2","ms":"2.1.3"}}')
steps_ nested-behind         "$fx" --start ci --pre 'node -e "const fs=require(\"fs\");const p=\"node_modules/.package-lock.json\";const h=JSON.parse(fs.readFileSync(p));delete h.packages[\"node_modules/chalk\"];fs.writeFileSync(p,JSON.stringify(h,null,2)+\"\\n\")"' -- CLONES=some install ansi-styles@4.3.0
# No package-lock.json (package-lock=false), and node_modules/.cache made
# after the first install (as a tool would), so npm stops trusting the
# hidden lockfile: its kept packages have neither an integrity nor a
# resolved; the gc record must keep their entries (store.rs update_record).
# (Made between the steps, a second later: a root postinstall making it
# races with npm's own write of the hidden lockfile, in plain npm too.)
fx=$(mkfx nolock-cache '{"name":"nolock-cache","version":"1.0.0","dependencies":{"chalk":"2.4.2","ms":"2.1.3","is-number":"7.0.0"}}' 'package-lock=false')
steps_ nolock-untrusted       "$fx" -- npm_config_before=$day CLONES=some install -- 'RUN=sleep 1; mkdir node_modules/.cache' CLONES=1 install left-pad@1.3.0
# The same project: uninstall (the record keeps what an incomplete hidden
# lockfile cannot account for), then update. The harness's bound applies
# only after a complete hidden lockfile.
steps_ nolock-uninstall-update "$fx" -- npm_config_before=$day install -- 'RUN=sleep 1; mkdir node_modules/.cache' uninstall ms -- npm_config_before=$day update
# A root postinstall that rewrites a dependency npm keeps (a fix-up script,
# patch-package): its package.json is new, its directory is not; a no-op
# install must not count it as unseen.
fx=$(mkfx fixup '{"name":"fixup","version":"1.0.0","scripts":{"postinstall":"node -e \"const fs=require(\\\"fs\\\");const f=\\\"node_modules/ms/package.json\\\";fs.writeFileSync(f,fs.readFileSync(f))\""},"dependencies":{"ms":"2.1.3"}}')
steps_ postinstall-rewrites  "$fx" --start ci -- CLONES=none UNSEEN=0 install
# The same version and `resolved` with other bytes: a file: tarball rebuilt
# and added again. npm writes the new integrity; the gc record drops the
# old entry, and the harness's record bound counts the integrity change.
fx=$work/fx-intchange; rm -rf "${fx:?}"; mkdir -p "$fx/t/package"
echo '{"name":"dep","version":"1.0.0"}' > "$fx/t/package/package.json"
echo 'module.exports = 1' > "$fx/t/package/index.js"; (cd "$fx/t" && tar czf ../dep-1.0.0.tgz package)
echo 'module.exports = 2' > "$fx/t/package/index.js"; (cd "$fx/t" && tar czf ../dep-b.tgz package); rm -rf "${fx:?}/t"
echo '{"name":"intchange","version":"1.0.0","dependencies":{"dep":"file:dep-1.0.0.tgz"}}' > "$fx/package.json"
(cd "$fx" && npm install --package-lock-only --no-audit --no-fund > /dev/null 2>&1)
steps_ integrity-change      "$fx" --start ci -- 'RUN=cp dep-b.tgz dep-1.0.0.tgz' install ./dep-1.0.0.tgz
# `--prefix packages/a`: npm's install root is that directory, not the
# project; the harness's root checks (the log's root, the hidden lockfile,
# the gc record, npm ls, file times) follow it.
fx=$(mkfx prefixsub '{"name":"prefixsub","version":"1.0.0","dependencies":{"ms":"2.1.3"}}')
mkdir -p "$fx/packages/a"; echo '{"name":"a","version":"1.0.0","dependencies":{"is-number":"7.0.0"}}' > "$fx/packages/a/package.json"
steps_ prefix-subdir         "$fx" --start ci -- npm_config_before=$day CLONES=some install --prefix packages/a -- npm_config_before=$day install --prefix packages/a ms@2.1.2
# --install-strategy=linked (npm 10 lists hoisted paths as symlinks into
# node_modules/.store): npm ci and npm install, no "unseen" alarm.
steps_ ci-linked             "$small" -- ci --install-strategy=linked --ignore-scripts
steps_ install-linked        "$small" --start ci -- install --install-strategy=linked --ignore-scripts
# Started in a plain subdirectory (no package.json there): npm's root is the
# project.
steps_ subdir-start          "$small" --start ci --pre 'mkdir -p src/lib' -- @src/lib CLONES=1 install left-pad@1.3.0
steps_ hidden-lockfile-behind "$small" --start ci --pre 'node -e "const fs=require(\"fs\");const p=\"node_modules/.package-lock.json\";const h=JSON.parse(fs.readFileSync(p));delete h.packages[\"node_modules/lodash\"];fs.writeFileSync(p,JSON.stringify(h,null,2)+\"\\n\")"' -- CLONES=none install
# --prefix: started in a directory with its own package.json, pointing npm
# at the project.
steps_ prefix-elsewhere      "$small" --start ci --pre 'mkdir other && echo "{\"name\":\"other\",\"version\":\"1.0.0\"}" > other/package.json' -- @other CLONES=1 install --prefix .. left-pad@1.3.0
steps_ bundled-reinstall     "$here/fixtures/bundled" --start ci --pre 'rm -rf node_modules/lc-bundler' -- CLONES=some install
# The hook not installed at all: the tree is npm's, and the one package
# npm added is counted as unseen.
steps_ hook-none-add         "$small" --start ci -- MATCHAPACK_INJECT_HOOK=none CLONES=0 UNSEEN=1 install left-pad@1.3.0
# Several commands in a row (with the aliases add, up, rm): the gc record
# and the unseen count across them; ends with npm ci.
steps_ sequence              "$small" -- CLONES=some install -- CLONES=1 add left-pad@1.3.0 -- npm_config_before=$day up semver -- CLONES=none rm left-pad -- CLONES=some ci
# Times outside the runs, owner, group, extended attribute names.
MATCHAPACK_VERIFY_META=1 steps_ meta "$ws" --start ci -- CLONES=1 install left-pad@1.3.0
if [ "${npm_version%%.*}" -ge 12 ]; then
  # npm 12 patchedDependencies: the patch file changed, npm install
  # re-extracts and re-patches; the store entry stays pacote's.
  steps_ npm12-patch "$here/fixtures/npm12-patch" --start ci --pre "sed -i.bak 's/patched by gl-patch-fixture/patched again by gl-patch-fixture/' patches/is-number@7.0.0.patch && rm patches/is-number@7.0.0.patch.bak" -- install
fi

if [ $fail != 0 ]; then
  echo "npm $npm_version: matchapack is NOT identical to npm ci here (see the failing cases)"
  exit 1
fi
echo "npm $npm_version: every case identical"
