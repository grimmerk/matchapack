#!/usr/bin/env bash
# A large real repository on a reflink filesystem (Linux: btrfs, or XFS with
# reflink=1): plain `npm ci` against matchapack, twice, with the tree checks
# of scripts/verify.sh (linux-large.yml runs it):
#   large-repo.sh <work-dir> <project-dir>
# <project-dir> holds the project's package.json and package-lock.json at
# its root (and whatever else it needs to install: workspace sources);
# <work-dir> is on the reflink filesystem, which also gets npm's cache
# (<work-dir>/npm-cache) and matchapack's store (<work-dir>/store).
#   0. npm's cache is filled by a plain `npm ci --ignore-scripts` (the only
#      install that downloads; its time is shown, not compared), then that
#      tree is removed.
#   1. verify.sh run 1: plain npm ci (A) and matchapack (B) with an empty
#      store, which B fills.
#   2. verify.sh run 2 in fresh copies: the same with the store B filled
#      (the main use case: one more worktree of a project).
# verify.sh is run with MATCHAPACK_VERIFY_CLONES=some, _BYTES=1 and
# _META=1. After each run: the files of B sharing blocks with the store
# (scripts/shared-blocks.py), btrfs's own count (btrfs filesystem du), and
# the disk each tree takes (free space before and after removing it).
# A table goes to $GITHUB_STEP_SUMMARY when it is set, and to the output.
# LARGE_LABEL names the run in the table (default: the filesystem type).
# Exit status 1 when a verify.sh run failed; the table is written anyway.
set -uo pipefail
work=${1:?usage: large-repo.sh <work-dir> <project-dir>}; src=${2:?usage: large-repo.sh <work-dir> <project-dir>}
here=$(cd "$(dirname "$0")/.." && pwd)
[ -x "$here/target/release/matchapack" ] || { echo "build first: cargo build --release" >&2; exit 2; }
[ -f "$src/package-lock.json" ] || { echo "no package-lock.json in $src" >&2; exit 2; }
mkdir -p "$work"; work=$(cd "$work" && pwd -P); src=$(cd "$src" && pwd -P)
fs=$(stat -f -c %T "$work")
label=${LARGE_LABEL:-$fs}
export npm_config_cache=$work/npm-cache npm_config_update_notifier=false MATCHAPACK_STORE=$work/store
unset MATCHAPACK_ENGINE MATCHAPACK
sudo=; [ "$(id -u)" = 0 ] || sudo=sudo
failures=0
now() { python3 -c 'import time; print(time.time())'; }
free_kb() { df -Pk "$work" | awk 'NR==2 {print $4}'; }
# Let the filesystem account for what was freed (btrfs frees extents when
# the transaction commits, XFS frees inodes in the background).
settle() {
  sync
  [ "$fs" = btrfs ] && $sudo btrfs filesystem sync "$work" >/dev/null 2>&1
  sleep 5
}
# freed_mib <dir>: the free space removing <dir> gives back, in MiB.
freed_mib() {
  local f0 f1
  settle; f0=$(free_kb)
  chmod -R u+w "${1:?}" 2>/dev/null; rm -rf "${1:?}"
  settle; f1=$(free_kb)
  echo $(( (f1 - f0) / 1024 ))
}
# field <file> <python regex with one group>: the first match, or "?".
field() { python3 -I - "$1" "$2" <<'EOF'
import re, sys
m = re.search(sys.argv[2], open(sys.argv[1], errors="replace").read(), re.M)
print(m.group(1) if m else "?")
EOF
}

echo "large-repo: $(node -v), npm $(npm -v), $(uname -sm), work on $fs ($(df -h "$work" | awk 'NR==2 {print $2 " total, " $4 " free"}'))"
echo "project: $src ($(python3 -I -c 'import json,sys; d=json.load(open(sys.argv[1])); p=d.get("packages",{}); print(len([k for k in p if "node_modules/" in k]), "lockfile entries, lockfileVersion", d.get("lockfileVersion"))' "$src/package-lock.json"))"

# 0. npm's cache.
echo "== filling npm's cache (plain npm ci --ignore-scripts)"
rm -rf "${work:?}/prime"; cp -a --reflink=auto "$src" "$work/prime"
t0=$(now)
(cd "$work/prime" && npm ci --ignore-scripts --no-audit --no-fund > "$work/prime.log" 2>&1) || { echo "npm ci could not fill the cache"; tail -30 "$work/prime.log"; exit 1; }
t1=$(now)
prime_secs=$(python3 -c "print(f'{$t1-$t0:.1f}')")
echo "cache filled in ${prime_secs}s: $(du -sh "$npm_config_cache" | cut -f1)"
rm -rf "${work:?}/prime"

rows=()
for run in 1 2; do
  w=$work/run$run; out=$work/run$run.out
  case $run in 1) store_was=empty ;; 2) store_was=warm ;; esac
  echo "== run $run (store $store_was): verify.sh"
  MATCHAPACK_VERIFY_CLONES=some MATCHAPACK_VERIFY_BYTES=1 MATCHAPACK_VERIFY_META=1 \
    "$here/scripts/verify.sh" "$src" "$w" > "$out" 2>&1; rc=$?
  grep -v '^\[matchapack\] skip ' "$out" | grep -E 'RESULT|^matchapack:|inject: cloned|^npm ci:|^cpu|meta:|npm ls:|byte-identical|stale in B|^[AB]: |cloned nothing|DIFFERS|differ' | sed 's/^/    /'
  if [ $rc != 0 ]; then
    failures=$((failures+1)); echo "    verify.sh FAILED (status $rc); its last lines:"; tail -40 "$out" | sed 's/^/    /'
  fi
  plain_wall=$(field "$out" '^npm ci: ([0-9.]+)s'); mp_wall=$(field "$out" 'matchapack: ([0-9.]+)s$')
  plain_cpu=$(field "$out" '^cpu \(user\+sys\): npm ci ([0-9.]+)s'); mp_cpu=$(field "$out" 'matchapack ([0-9.]+)s$')
  same=$(field "$out" '^RESULT: (\w+)'); meta=$(field "$out" '^meta: (\w+)'); ls_=$(field "$out" '^npm ls: (same result|DIFFERS)')
  cloned=$(field "$w/B.log" 'inject: cloned ([0-9]+)'); by_npm=$(field "$w/B.log" 'extracted by npm ([0-9]+)'); new=$(field "$w/B.log" '([0-9]+) new in the store')
  shared="?"; [ -d "$w/B/node_modules" ] && shared=$(python3 -I "$here/scripts/shared-blocks.py" "$w/B" "$MATCHAPACK_STORE" 2>&1 | tail -1)
  echo "    shared with the store: $shared"
  if [ "$fs" = btrfs ] && [ -d "$w/B/node_modules" ]; then
    echo "    btrfs filesystem du (B/node_modules): $($sudo btrfs filesystem du -s --raw "$w/B/node_modules" 2>/dev/null | awk 'NR==2 {printf "total %.0f MiB, exclusive %.0f MiB, shared %.0f MiB", $1/1048576, $2/1048576, $3/1048576}')"
  fi
  disk_a="?"; disk_b="?"
  [ -d "$w/A" ] && disk_a=$(freed_mib "$w/A")
  [ -d "$w/B" ] && disk_b=$(freed_mib "$w/B")
  echo "    disk taken (free space regained by removing the tree): plain ${disk_a} MiB, matchapack ${disk_b} MiB"
  rm -rf "${w:?}/A2"
  rows+=("| $label | $run ($store_was store) | ${plain_wall}s / ${plain_cpu}s | ${mp_wall}s / ${mp_cpu}s | $same, meta $meta, npm ls $ls_ | $cloned cloned, $by_npm by npm, $new new in store | $shared | $disk_a / $disk_b MiB |")
done

{
  echo "### $label: $(basename "$src") ($(node -v), npm $(npm -v))"
  echo
  echo "npm's cache filled first by a plain \`npm ci --ignore-scripts\` (network): ${prime_secs}s. Both sides then install from that cache."
  echo
  echo "| fs | run | plain npm ci: wall / CPU | matchapack: wall / CPU | A vs B (verify.sh) | packages | B's files sharing blocks with the store | disk: plain / matchapack |"
  echo "|---|---|---|---|---|---|---|---|"
  printf '%s\n' "${rows[@]}"
  echo
  echo "CPU is user + sys of the install and every process under it. Disk is the free space regained by removing the tree (df), after the run."
} > "$work/summary.md"
cat "$work/summary.md"
[ -n "${GITHUB_STEP_SUMMARY:-}" ] && cat "$work/summary.md" >> "$GITHUB_STEP_SUMMARY"
echo "large-repo: verify.sh runs failed: $failures"
[ "$failures" -eq 0 ]
