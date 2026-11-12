#!/usr/bin/env bash
# One-time conversion of EXISTING node_modules into clones.
#   convert-existing.sh [--apply] [--max-depth N] <root>...   (default depth 6:
#   reaches Claude Code worktrees at <repo>/.claude/worktrees/<name>/)
# Finds projects (not inside node_modules) under each root and re-installs
# each one through matchapack. Without --apply it only lists what it would do.
#   - npm (package.json + package-lock.json + node_modules): `matchapack npm
#     ci`, i.e. npm ci semantics: node_modules is rebuilt and its install
#     scripts run again (clones of matchapack's store).
#   - Yarn 1, macOS only (package.json + yarn.lock + node_modules, no
#     package-lock.json): node_modules is removed (the root's and each
#     workspace's; Yarn keeps files that are already in place, so nothing
#     would be shared otherwise), then `matchapack yarn install
#     --frozen-lockfile`: install scripts run again (clones of Yarn's cache).
#     Off macOS plain Yarn 1 already clones: nothing to convert.
#
# Skipped, and left exactly as they are:
#   - projects matchapack would not handle (it reports why; MATCHAPACK_NO_FALLBACK=1
#     keeps it from falling back to a plain npm ci / yarn, which would gain
#     nothing; a Yarn project's removed node_modules is put back);
#   - Yarn 2+ projects (.yarnrc.yml, packageManager yarn@2+, a Yarn 2+
#     lockfile);
#   - projects where a running process has its working directory inside the
#     project (a dev server, a watcher, a shell), so nothing in use is replaced;
#   - projects with packages in node_modules that the lockfile does not list
#     (`npm link`, `yarn link`, `--no-save`): a reinstall would drop them.
# Each node_modules replaced is first backed up as a copy-on-write copy
# (~/.cache/matchapack/backup) and put back if the install fails.
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
command -v matchapack >/dev/null || { echo "matchapack is not on PATH" >&2; exit 2; }

# lsof reports real paths (/private/tmp, resolved symlinks): compare like with like.
for i in "${!roots[@]}"; do roots[$i]=$(cd "${roots[$i]}" && pwd -P) || exit 2; done
# Working directories of every running process (one lsof call). An empty
# answer means we cannot tell what is in use: stop rather than guess.
cwds=$(lsof -a -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | sort -u)
[ -n "$cwds" ] || { echo "lsof reported no working directories; refusing to run" >&2; exit 2; }

# A copy-on-write copy of $1 at $2 (APFS clone; elsewhere a reflink or copy).
backup() { mkdir -p "$(dirname "$2")"; cp -c -R "$1" "$2" 2>/dev/null || cp -a --reflink=auto "$1" "$2"; }
free_kb() { df -k "$1" | tail -1 | awk '{print $4}'; }
before=$(free_kb "${roots[0]}")
n_conv=0; n_skip=0; n_fail=0
while IFS= read -r lock; do
  dir=$(dirname "$lock")
  case $lock in */yarn.lock) kind=yarn ;; *) kind=npm ;; esac
  [ -f "$dir/package.json" ] && [ -d "$dir/node_modules" ] || continue
  # A project with both lockfiles is converted as an npm one (as before).
  [ $kind = yarn ] && [ -f "$dir/package-lock.json" ] && continue
  if [ $kind = yarn ] && [ "$(uname)" != Darwin ]; then
    echo "SKIP yarn, not macOS $dir  (plain Yarn 1 already clones here)"; n_skip=$((n_skip+1)); continue
  fi
  # Exact prefix match: /x/app must not match /x/app-review.
  if printf '%s\n' "$cwds" | awk -v d="$dir" '$0 == d || index($0, d "/") == 1 { f = 1 } END { exit !f }'; then
    echo "SKIP in use      $dir"; n_skip=$((n_skip+1)); continue
  fi
  if [ $kind = npm ]; then
  # Packages in node_modules that the lockfile does not list (`npm link`,
  # `npm install --no-save`) would be dropped by npm ci semantics: skip.
  extra=$(python3 -I - "$dir" <<'EOF'
import json, os, sys
d = sys.argv[1]
keys = set(json.load(open(os.path.join(d, "package-lock.json"))).get("packages", {}))
nm = os.path.join(d, "node_modules")
names = []
for e in os.listdir(nm):
    if e.startswith("."):
        continue
    if e.startswith("@") and os.path.isdir(os.path.join(nm, e)):
        names += [f"{e}/{s}" for s in os.listdir(os.path.join(nm, e)) if not s.startswith(".")]
    else:
        names.append(e)
print(" ".join(n for n in names if f"node_modules/{n}" not in keys))
EOF
)
  else
  # Yarn: a Yarn 2+ project is not matchapack's (told from files, as
  # `matchapack yarn` tells it); then the node_modules directories to
  # replace (the root's and each workspace's), and any package in them
  # that is neither in yarn.lock nor a workspace (`yarn link`, a manual
  # copy), which a reinstall would drop. Prints "YARN2 <why>", or
  # "EXTRA <names>", or one node_modules path per line.
  yplan=$(python3 -I - "$dir" <<'EOF'
import glob, json, os, sys
d = sys.argv[1]
pkg = json.load(open(os.path.join(d, "package.json")))
pm = pkg.get("packageManager") or ""
lock = open(os.path.join(d, "yarn.lock"), encoding="utf-8", errors="replace").read()
if os.path.exists(os.path.join(d, ".yarnrc.yml")):
    sys.exit(print("YARN2 .yarnrc.yml"))
if pm.startswith("yarn@") and not pm.startswith("yarn@1."):
    sys.exit(print(f"YARN2 packageManager {pm}"))
if any(l.startswith("__metadata:") for l in lock.splitlines()):
    sys.exit(print("YARN2 a Yarn 2+ lockfile"))
# Names in yarn.lock: each top-level key line, `a@^1, "@s/b@x":`.
names = set()
for line in lock.splitlines():
    if not line or line[0] in " #" or not line.endswith(":"):
        continue
    for k in line[:-1].split(", "):
        k = k.strip('"')
        at = k.find("@", 1)
        names.add(k[:at] if at > 0 else k)
ws = pkg.get("workspaces") or []
if isinstance(ws, dict):
    ws = ws.get("packages") or []
dirs = [d]
for pat in ws:
    for w in sorted(glob.glob(os.path.join(d, pat))):
        if os.path.isfile(os.path.join(w, "package.json")) and "node_modules" not in os.path.relpath(w, d).split(os.sep):
            dirs.append(w)
            try:
                names.add(json.load(open(os.path.join(w, "package.json"))).get("name") or "")
            except ValueError:
                pass
nms, extra = [], []
for w in dirs:
    nm = os.path.join(w, "node_modules")
    if not os.path.isdir(nm) or os.path.islink(nm):
        continue
    nms.append(nm)
    for e in os.listdir(nm):
        if e.startswith("."):
            continue
        if e.startswith("@") and os.path.isdir(os.path.join(nm, e)):
            extra += [f"{e}/{s}" for s in os.listdir(os.path.join(nm, e)) if not s.startswith(".") and f"{e}/{s}" not in names]
        elif e not in names:
            extra.append(e)
if extra:
    sys.exit(print("EXTRA " + " ".join(sorted(set(extra)))))
print("\n".join(nms))
EOF
)
  case $yplan in
    YARN2*) echo "SKIP Yarn 2+     $dir  (${yplan#YARN2 })"; n_skip=$((n_skip+1)); continue ;;
    EXTRA*) extra=${yplan#EXTRA } ;;
    *) extra= ;;
  esac
  fi
  if [ -n "$extra" ]; then
    echo "SKIP not in lock $dir  (${extra:0:80})"; n_skip=$((n_skip+1)); continue
  fi
  if [ $apply = 0 ]; then
    echo "CANDIDATE $kind $(printf '%*s' $((6 - ${#kind})) '')$dir"; continue
  fi
  # Copy-on-write backup outside the repository, restored if the install fails.
  bk="$HOME/.cache/matchapack/backup/$(printf '%s' "$dir" | shasum | cut -c1-16)"
  rm -rf "${bk:?}"
  if [ $kind = npm ]; then
    backup "$dir/node_modules" "$bk"
    out=$(cd "$dir" && MATCHAPACK_NO_FALLBACK=1 matchapack npm ci --no-audit --no-fund 2>&1); rc=$?
    # (npm ci under MATCHAPACK_NO_FALLBACK=1 hands over, exit 3, before it
    # changes anything: only a failure needs the backup.)
    restore() { rm -rf "${dir:?}/node_modules"; mv "$bk" "$dir/node_modules"; }
    done_line=$(printf '%s\n' "$out" | grep -o 'matchapack: added.*' | sed 's/^matchapack: //')
  else
    # Each node_modules to replace: backed up as $bk/<n>, its path in $bk.paths.
    nms=(); while IFS= read -r p; do [ -n "$p" ] && nms+=("$p"); done <<< "$yplan"
    mkdir -p "$bk"; : > "$bk.paths"
    for i in "${!nms[@]}"; do backup "${nms[$i]}" "$bk/$i"; printf '%s\n' "${nms[$i]}" >> "$bk.paths"; done
    for p in "${nms[@]}"; do rm -rf "${p:?}"; done
    out=$(cd "$dir" && MATCHAPACK_NO_FALLBACK=1 matchapack yarn install --frozen-lockfile --non-interactive 2>&1); rc=$?
    restore() {
      local i=0 p
      while IFS= read -r p; do rm -rf "${p:?}"; mv "$bk/$i" "$p"; i=$((i+1)); done < "$bk.paths"
      rm -rf "${bk:?}" "$bk.paths"
    }
    done_line=$(printf '%s\n' "$out" | grep -o 'matchapack: [0-9]* files cloned.*\|matchapack: no clones in this install.*' | sed 's/^matchapack: //' | tr '\n' ' ')
  fi
  case $rc in
    0) echo "CONVERTED        $dir  ($done_line)"; n_conv=$((n_conv+1)); rm -rf "${bk:?}" "$bk.paths" ;;
    3) [ $kind = yarn ] && restore; echo "SKIP not handled $dir  ($(printf '%s\n' "$out" | grep -o 'not handled, left unchanged.*' | sed 's/^not handled, left unchanged //'))"; n_skip=$((n_skip+1)); rm -rf "${bk:?}" ;;
    *) echo "FAILED (rc=$rc)  $dir  (previous node_modules restored)"; printf '%s\n' "$out" | grep -E 'matchapack|npm error|^error' | head -3 | sed 's/^/    /'; n_fail=$((n_fail+1))
       restore ;;
  esac
# -prune: never descend into node_modules or .git (a -not -path filter would
# still walk millions of files and take minutes).
done < <(for r in "${roots[@]}"; do find "$r" -maxdepth "$depth" \( -name node_modules -o -name .git \) -prune -o \( -name package-lock.json -o -name yarn.lock \) -print 2>/dev/null; done | sort)

if [ $apply = 1 ]; then
  sync; sleep 2
  after=$(free_kb "${roots[0]}")
  echo "converted $n_conv, skipped $n_skip, failed $n_fail; free space $((before/1048576)) GiB → $((after/1048576)) GiB"
else
  echo "dry run: nothing changed. Candidates matchapack cannot handle are found and skipped at --apply."
fi
