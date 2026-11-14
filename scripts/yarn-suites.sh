#!/usr/bin/env bash
# The Yarn 1 suite (test-yarn1.sh) on the node on PATH and the Yarn 1 named
# by MATCHAPACK_YARN_JS: what the canary's node-macos job runs for a new Node
# or a new Yarn 1 release (.github/workflows/npm-canary.yml; NOTES "The Node
# and Yarn 1 release watch").
#   yarn-suites.sh <work dir> <out dir>
# <out dir> gets test-yarn1.log and suites.json, in the npm suites' format
# (scripts/npm-suites.sh) plus what the Yarn engine depends on:
#   {"node", "uv", "yarn", "os", "facts", "suites": {"test-yarn1": {"status",
#    "pass", "fail", "skip", "failed": [...], "last"}}}
# facts: what Node's fs.copyFile keeps, as the hook's start-up probe measured
# it in an engine case (yarn1-many); null when no engine case ran.
# MATCHAPACK_YARN_PRIME_NODE and MATCHAPACK_FISH pass through. Exit status:
# test-yarn1.sh's. Needs the release binary (cargo build --release).
set -uo pipefail
work=${1:?usage: yarn-suites.sh <work dir> <out dir>}; out=${2:?usage: yarn-suites.sh <work dir> <out dir>}
here=$(cd "$(dirname "$0")/.." && pwd)
[ -x "$here/target/release/matchapack" ] || { echo "build first: cargo build --release" >&2; exit 2; }
[ -f "${MATCHAPACK_YARN_JS:-}" ] || { echo "set MATCHAPACK_YARN_JS to Yarn 1's bin/yarn.js" >&2; exit 2; }
mkdir -p "$work" "$out"; work=$(cd "$work" && pwd -P); out=$(cd "$out" && pwd -P)
yver=$(node "$MATCHAPACK_YARN_JS" --version 2>/dev/null) || yver=unknown
echo "yarn $yver, node $(node -v) (libuv $(node -p process.versions.uv)), $(uname -sm)"
t0=$(date +%s)
"$here/scripts/test-yarn1.sh" "$work/y1" > "$out/test-yarn1.log" 2>&1; rc=$?
echo "test-yarn1: exit $rc, $(( $(date +%s) - t0 )) s ($(grep -c '^PASS ' "$out/test-yarn1.log") passed, $(grep -c '^FAIL ' "$out/test-yarn1.log") failed)"
awk '/^FAIL / {p=1; print; next} p && /^ / {print; next} {p=0}' "$out/test-yarn1.log" | head -60 | sed 's/^/    /'
python3 - "$out" "$rc" "$yver" "$(node -v)" "$(node -p process.versions.uv)" "$(uname -sm)" "$work/y1/e-many/engine.json" <<'PY'
import json, os, sys
out, rc, yver, node, uv, os_, engine = sys.argv[1:8]
log = open(os.path.join(out, "test-yarn1.log"), errors="replace").read().splitlines()
facts = None
try:
    facts = (json.load(open(engine)).get("yarn1") or {}).get("facts")
except (OSError, ValueError):
    pass
suite = {
    "status": int(rc),
    "pass": sum(1 for l in log if l.startswith("PASS ")),
    "fail": sum(1 for l in log if l.startswith("FAIL ")),
    "skip": sum(1 for l in log if l.startswith("SKIP ")),
    "failed": [l[:300] for l in log if l.startswith("FAIL ")][:20],
    "last": log[-1][:300] if log else "",
}
json.dump({"node": node, "uv": uv, "yarn": yver, "os": os_, "facts": facts, "suites": {"test-yarn1": suite}}, open(os.path.join(out, "suites.json"), "w"), indent=2)
PY
exit "$rc"
