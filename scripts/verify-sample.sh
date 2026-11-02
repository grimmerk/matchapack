#!/usr/bin/env bash
# Sampled verification during the trial.
#   verify-sample.sh [N] [project-dir...]
# With no directories: takes the N (default 2) most recent distinct projects
# that lockclone installed (from ~/.cache/lockclone/log.jsonl). For each, on
# copy-on-write copies of the project: npm ci twice (A, A2) and lockclone once
# (B); paths that differ between A and A2 are build noise and ignored; then the
# harness compares A with B. One JSON line per project goes to
# ~/.cache/lockclone/verify.jsonl, shown by `lockclone report`.
# Costs a real npm ci twice per project, so run it now and then, not per install.
set -uo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
logdir=${LOCKCLONE_LOG_DIR_REAL:-$HOME/.cache/lockclone}
n=2
if [[ "${1-}" =~ ^[0-9]+$ ]]; then n=$1; shift; fi
dirs=("$@")
if [ ${#dirs[@]} -eq 0 ]; then
  while IFS= read -r d; do dirs+=("$d"); done < <(python3 -I - "$logdir/log.jsonl" "$n" <<'EOF'
import json, os, sys
path, n = sys.argv[1], int(sys.argv[2])
seen, out = set(), []
try:
    lines = open(path).read().splitlines()
except FileNotFoundError:
    lines = []
for l in reversed(lines):
    try:
        r = json.loads(l)
    except ValueError:
        continue
    d = r.get("dir")
    if r.get("outcome") == "installed" and d and d not in seen and os.path.isdir(d):
        seen.add(d); out.append(d)
    if len(out) >= n:
        break
print("\n".join(out))
EOF
)
fi
[ ${#dirs[@]} -gt 0 ] || { echo "nothing to verify: no lockclone installs in $logdir/log.jsonl yet"; exit 0; }

work=$(mktemp -d "${TMPDIR:-/tmp}/lockclone-verify.XXXXXX")
trap 'rm -rf "$work"' EXIT
for d in "${dirs[@]}"; do
  echo "######## $d"
  out=$(LOCKCLONE_VERIFY_NOISE=1 LOCKCLONE_LOG_DIR="$work/logs" "$here/scripts/verify.sh" "$d" "$work/v" 2>&1)
  printf '%s\n' "$out" | grep -E 'RESULT|npm ci:|noise|npm ls:|stale in B|^  ' | head -20
  printf '%s\n' "$out" > "$work/out.txt"
  python3 -I - "$logdir/verify.jsonl" "$d" "$work/out.txt" <<'EOF'
import json, re, sys, time
log, d, outfile = sys.argv[1:4]
out = open(outfile, errors="replace").read()
m = re.search(r"RESULT: (\w+)", out)
t = re.search(r"npm ci: ([\d.]+)s\s+lockclone: ([\d.]+)s", out)
n = re.search(r"noise .*?: (\d+) paths", out)
ls = re.search(r"npm ls: (.*)", out)
rec = {"ts": time.time(), "dir": d, "result": m.group(1) if m else "ERROR",
       "npm_ci_secs": float(t.group(1)) if t else None, "lockclone_secs": float(t.group(2)) if t else None,
       "noise_paths": int(n.group(1)) if n else None, "npm_ls": ls.group(1).strip() if ls else None}
open(log, "a").write(json.dumps(rec) + "\n")
print("recorded:", rec["result"])
if rec["result"] == "ERROR":
    print("\n".join("    " + l for l in out.strip().splitlines()[-15:]))
EOF
  rm -rf "${work:?}/v"
done
