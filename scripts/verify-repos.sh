#!/usr/bin/env bash
# Run verify.sh on the default branch of several local repos.
#   verify-repos.sh <work-dir> <repo-dir>...
# Each repo is exported with `git archive` (tracked files only; the checkout is
# never touched), then verified. One summary line per repo at the end.
set -uo pipefail
work=$1; shift
here=$(cd "$(dirname "$0")" && pwd)
summary=()
for repo in "$@"; do
  name=$(basename "$repo")
  ref=$(git -C "$repo" symbolic-ref --short refs/remotes/origin/HEAD 2>/dev/null || echo HEAD)
  src="$work/src-$name"
  rm -rf "${src:?}"; mkdir -p "$src"
  git -C "$repo" archive "$ref" | tar -x -C "$src"
  echo "######## $name ($ref $(git -C "$repo" rev-parse --short "$ref"))"
  out=$("$here/verify.sh" "$src" "$work/v-$name" 2>&1); rc=$?
  echo "$out" | grep -E 'RESULT|npm ls:|npm ci:|only in|differ:|byte-identical|resolved [0-9]+|Error|error' | head -12
  res=$(echo "$out" | grep -o 'RESULT: [A-Z]*' || echo "RESULT: (none)")
  ls_=$(echo "$out" | grep -o 'npm ls: [a-zA-Z ]*' | head -1)
  t=$(echo "$out" | grep -o 'npm ci: .*' | head -1)
  summary+=("$name | rc=$rc | $res | $ls_ | $t")
  [ $rc -ne 0 ] && echo "$out" | tail -8
  rm -rf "${work:?}/v-$name"
done
echo "======== SUMMARY"
printf '%s\n' "${summary[@]}"
