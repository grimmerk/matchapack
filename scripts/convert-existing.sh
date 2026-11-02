#!/usr/bin/env bash
# One-time conversion of EXISTING node_modules into store clones.
#   convert-existing.sh [--apply] [--max-depth N] <root>...   (default depth 6:
#   reaches Claude Code worktrees at <repo>/.claude/worktrees/<name>/)
# Finds npm projects (package.json + package-lock.json + node_modules, not
# inside node_modules) under each root and re-installs each one with
# `lockclone npm ci`, i.e. npm ci semantics: node_modules is rebuilt and its
# install scripts run again. Without --apply it only lists what it would do.
#
# Skipped, and left exactly as they are:
#   - projects lockclone would not handle (it reports why; LOCKCLONE_NO_FALLBACK=1
#     keeps it from falling back to a plain npm ci, which would gain nothing);
#   - projects where a running process has its working directory inside the
#     project (a dev server, a watcher, a shell), so nothing in use is replaced.
# Prints the volume's free space before and after.
set -uo pipefail
apply=0; depth=6; roots=()
while [ $# -gt 0 ]; do
  case $1 in
    --apply) apply=1 ;;
    --max-depth) depth=$2; shift ;;
    *) roots+=("$1") ;;
  esac
  shift
done
[ ${#roots[@]} -gt 0 ] || { echo "usage: $0 [--apply] [--max-depth N] <root>..." >&2; exit 2; }
command -v lockclone >/dev/null || { echo "lockclone is not on PATH" >&2; exit 2; }

# Working directories of every running process (one lsof call).
cwds=$(lsof -a -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | sort -u)

free_kb() { df -k "$1" | tail -1 | awk '{print $4}'; }
before=$(free_kb "${roots[0]}")
n_conv=0; n_skip=0; n_fail=0
while IFS= read -r lock; do
  dir=$(dirname "$lock")
  [ -f "$dir/package.json" ] && [ -d "$dir/node_modules" ] || continue
  # Exact prefix match: /x/app must not match /x/app-review.
  if printf '%s\n' "$cwds" | awk -v d="$dir" '$0 == d || index($0, d "/") == 1 { f = 1 } END { exit !f }'; then
    echo "SKIP in use      $dir"; n_skip=$((n_skip+1)); continue
  fi
  if [ $apply = 0 ]; then
    echo "CANDIDATE        $dir"; continue
  fi
  out=$(cd "$dir" && LOCKCLONE_NO_FALLBACK=1 lockclone npm ci --no-audit --no-fund 2>&1); rc=$?
  case $rc in
    0) echo "CONVERTED        $dir  ($(printf '%s\n' "$out" | grep -o 'lockclone: added.*' | sed 's/^lockclone: //'))"; n_conv=$((n_conv+1)) ;;
    3) echo "SKIP not handled $dir  ($(printf '%s\n' "$out" | grep '^lockclone: not handled' | sed 's/^lockclone: not handled, left unchanged //'))"; n_skip=$((n_skip+1)) ;;
    *) echo "FAILED (rc=$rc)  $dir"; printf '%s\n' "$out" | grep -E '^lockclone|npm error' | head -3 | sed 's/^/    /'; n_fail=$((n_fail+1)) ;;
  esac
# -prune: never descend into node_modules or .git (a -not -path filter would
# still walk millions of files and take minutes).
done < <(for r in "${roots[@]}"; do find "$r" -maxdepth "$depth" \( -name node_modules -o -name .git \) -prune -o -name package-lock.json -print 2>/dev/null; done | sort)

if [ $apply = 1 ]; then
  sync; sleep 2
  after=$(free_kb "${roots[0]}")
  echo "converted $n_conv, skipped $n_skip, failed $n_fail; free space $((before/1048576)) GiB → $((after/1048576)) GiB"
else
  echo "dry run: nothing changed. Candidates lockclone cannot handle are found and skipped at --apply."
fi
