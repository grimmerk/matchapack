#!/usr/bin/env bash
# Sampled verification during the trial.
#   verify-sample.sh [N] [project-dir...]
# With no directories: takes the N (default 2) most recent distinct projects
# that matchapack installed (from ~/.cache/matchapack/log.jsonl). For each, on
# copy-on-write copies of the project: npm ci twice (A, A2) and matchapack once
# (B); paths that differ between A and A2 are build noise and ignored; then the
# harness compares A with B. One JSON line per project goes to
# ~/.cache/matchapack/verify.jsonl, shown by `matchapack report`.
# Costs a real npm ci twice per project, so run it now and then, not per install.
set -uo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
logdir=${MATCHAPACK_LOG_DIR_REAL:-$HOME/.cache/matchapack}
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
    # Replayed as npm ci: only npm ci runs (records without a command are
    # from before the install family was routed). `npm install` in a
    # workspace member or with --prefix installs elsewhere (`root`).
    if r.get("command", "ci") != "ci":
        continue
    d = r.get("root") or d
    if r.get("outcome") == "installed" and d and d not in seen and os.path.isdir(d):
        seen.add(d); out.append(d)
    if len(out) >= n:
        break
print("\n".join(out))
EOF
)
fi
[ ${#dirs[@]} -gt 0 ] || { echo "nothing to verify: no matchapack installs in $logdir/log.jsonl yet"; exit 0; }

work=$(mktemp -d "${TMPDIR:-/tmp}/matchapack-verify.XXXXXX")
trap 'rm -rf "$work"' EXIT
for d in "${dirs[@]}"; do
  echo "######## $d"
  out=$(MATCHAPACK_VERIFY_NOISE=1 MATCHAPACK_LOG_DIR="$work/logs" "$here/scripts/verify.sh" "$d" "$work/v" 2>&1)
  printf '%s\n' "$out" | grep -E 'RESULT|npm ci:|noise|npm ls:|stale in B|^  ' | head -20
  printf '%s\n' "$out" > "$work/out.txt"
  # The build verify.sh ran (target/release), which may differ from the one on PATH.
  build=$("$here/target/release/matchapack" --version 2>/dev/null | sed -n 's/.*(\(.*\)).*/\1/p')
  python3 -I - "$logdir/verify.jsonl" "$d" "$work/out.txt" "${build:-unknown}" <<'EOF'
import fcntl, json, re, sys, time
log, d, outfile, build = sys.argv[1:5]
out = open(outfile, errors="replace").read()
m = re.search(r"RESULT: (\w+)", out)
t = re.search(r"npm ci: ([\d.]+)s\s+matchapack: ([\d.]+)s", out)
n = re.search(r"noise .*?: (\d+) paths", out)
ls = re.search(r"npm ls: (.*)", out)
result = m.group(1) if m else "ERROR"
# Differences only OUTSIDE node_modules come from the project's own build
# scripts (e.g. tsdown output that varies run to run, which two npm ci runs may
# happen to agree on): reported, but kept apart from a node_modules mismatch.
paths = re.findall(r"^  ([^ :]+)(?::|$)", out.split("== diff A B")[-1].split("== mtime")[0], re.M)
if result == "DIFFERENT" and paths and all(not (p.startswith("node_modules/") or "/node_modules/" in p) for p in paths):
    result = "BUILD-OUTPUT-ONLY"
rec = {"ts": time.time(), "build": build, "dir": d, "result": result, "differing": paths[:20],
       "npm_ci_secs": float(t.group(1)) if t else None, "matchapack_secs": float(t.group(2)) if t else None,
       "noise_paths": int(n.group(1)) if n else None, "npm_ls": ls.group(1).strip() if ls else None}
# One whole line under the lock matchapack's own log writer takes (src/trial.rs
# append_line), in case two samplers run at once.
with open(log, "a") as f:
    fcntl.flock(f, fcntl.LOCK_EX)
    f.write(json.dumps(rec) + "\n")
print("recorded:", rec["result"])
if rec["result"] == "ERROR":
    print("\n".join("    " + l for l in out.strip().splitlines()[-15:]))
EOF
  rm -rf "${work:?}/v"
done
