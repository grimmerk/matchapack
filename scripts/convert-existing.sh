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
#     (`npm link`, `yarn link`, `--no-save`): a reinstall would drop them;
#     for Yarn also any package that is a symlink other than a workspace's
#     (`yarn link` replaces an installed package with one);
#   - projects whose backup could not be made (below).
# Each node_modules replaced is first backed up as a copy-on-write copy
# (~/.cache/matchapack/backup), checked against the original (the same
# paths, types, sizes and link targets), and put back if the install fails
# or the script is interrupted (SIGINT, SIGTERM, SIGHUP) before it ends.
# Nothing is removed or installed until every backup of the project is made
# and checked. Yarn's workspaces are the ones Yarn lists (`yarn workspaces
# info`), so its own pattern rules apply (`**`, `{a,b}`).
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

# A copy of tree $1 at $2: on macOS an APFS clone (`cp -c`), else (another
# volume, a filesystem that cannot clone) a full copy; elsewhere a reflink
# when the filesystem can, else a copy. Fails when no copy could be made.
copy_tree() {
  if [ "$(uname)" = Darwin ]; then
    cp -c -pR "$1" "$2" 2>/dev/null && return 0
    rm -rf "${2:?}"; cp -pR "$1" "$2"
  else
    cp -a --reflink=auto "$1" "$2"
  fi
}
# Whether tree $2 has the same paths as $1, each of the same type, regular
# files of the same size, symlinks with the same target (lstat; links are
# not followed).
same_tree() {
  python3 -I - "$1" "$2" <<'EOF'
import os, stat, sys
def scan(top):
    out = {}
    for d, dirs, files in os.walk(top, onerror=lambda e: sys.exit(1)):
        for n in dirs + files:
            p = os.path.join(d, n)
            st = os.lstat(p)
            out[os.path.relpath(p, top)] = (stat.S_IFMT(st.st_mode), st.st_size if stat.S_ISREG(st.st_mode) else 0, os.readlink(p) if stat.S_ISLNK(st.st_mode) else "")
    return out
sys.exit(0 if scan(sys.argv[1]) == scan(sys.argv[2]) else 1)
EOF
}
# Back up each of ${nms[@]} as $bk/<n>, its path as line n of $bk.paths,
# each copy checked. On failure $why says what failed (the caller removes
# what was made).
make_backups() {
  local i
  mkdir -p "$bk" || { why="cannot create $bk"; return 1; }
  : > "$bk.paths" || { why="cannot write $bk.paths"; return 1; }
  for i in "${!nms[@]}"; do
    copy_tree "${nms[$i]}" "$bk/$i" 2>/dev/null || { why="copying ${nms[$i]} failed"; return 1; }
    same_tree "${nms[$i]}" "$bk/$i" || { why="the copy of ${nms[$i]} does not match it"; return 1; }
    printf '%s\n' "${nms[$i]}" >> "$bk.paths" || { why="cannot write $bk.paths"; return 1; }
  done
  [ "$(wc -l < "$bk.paths" | tr -d ' ')" = "${#nms[@]}" ] || { why="$bk.paths is incomplete"; return 1; }
}
# Put every backed-up node_modules back; a backup that cannot be moved back
# is kept, and its place said.
restore() {
  local i=0 p ok=1
  while IFS= read -r p; do
    rm -rf "${p:?}"
    mv "$bk/$i" "$p" || { ok=0; echo "    could not put back $p: its backup is $bk/$i"; }
    i=$((i+1))
  done < "$bk.paths"
  [ $ok = 1 ] && rm -rf "${bk:?}" "$bk.paths"
}
# Interrupted while a project's node_modules may be gone: wait (up to 20 s)
# for the processes of its install to leave the project, put the backups
# back, stop.
pending=
on_signal() {
  trap '' INT TERM HUP
  if [ -n "$pending" ]; then
    local t=0
    while [ $t -lt 40 ] && lsof -a -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | awk -v d="$dir" '$0 == d || index($0, d "/") == 1 { f = 1 } END { exit !f }'; do
      sleep 0.5; t=$((t+1))
    done
    restore
    echo "INTERRUPTED      $dir  (previous node_modules restored)"
  fi
  exit 130
}
trap on_signal INT TERM HUP
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
  # `matchapack yarn` tells it).
  y2=$(python3 -I - "$dir" <<'EOF'
import json, os, sys
d = sys.argv[1]
pm = json.load(open(os.path.join(d, "package.json"))).get("packageManager") or ""
if os.path.exists(os.path.join(d, ".yarnrc.yml")):
    print(".yarnrc.yml")
elif pm.startswith("yarn@") and not pm.startswith("yarn@1."):
    print(f"packageManager {pm}")
elif any(l.startswith("__metadata:") for l in open(os.path.join(d, "yarn.lock"), encoding="utf-8", errors="replace")):
    print("a Yarn 2+ lockfile")
EOF
)
  if [ -n "$y2" ]; then echo "SKIP Yarn 2+     $dir  ($y2)"; n_skip=$((n_skip+1)); continue; fi
  # The workspaces, as Yarn lists them (its own pattern rules).
  ws_json='{}'
  if python3 -I -c 'import json,sys; sys.exit(0 if json.load(open(sys.argv[1])).get("workspaces") else 1)' "$dir/package.json"; then
    if ! ws_json=$(cd "$dir" && matchapack yarn --silent workspaces info 2>&1 < /dev/null); then
      echo "SKIP workspaces  $dir  (yarn workspaces info: $(printf '%s' "$ws_json" | tail -1 | cut -c1-80))"; n_skip=$((n_skip+1)); continue
    fi
  fi
  # The node_modules directories to replace (the root's and each
  # workspace's); any package in them that is neither in yarn.lock nor a
  # workspace (a manual copy), and any package that is a symlink other than
  # to a workspace (`yarn link`), either of which a reinstall would drop.
  # Prints "LINKED <names>", "EXTRA <names>", or one node_modules path per
  # line.
  yplan=$(WS_JSON=$ws_json python3 -I - "$dir" <<'EOF'
import json, os, sys
d = sys.argv[1]
lock = open(os.path.join(d, "yarn.lock"), encoding="utf-8", errors="replace").read()
# Names in yarn.lock: each top-level key line, `a@^1, "@s/b@x":`.
names = set()
for line in lock.splitlines():
    if not line or line[0] in " #" or not line.endswith(":"):
        continue
    for k in line[:-1].split(", "):
        k = k.strip('"')
        at = k.find("@", 1)
        names.add(k[:at] if at > 0 else k)
dirs, ws_real = [d], set()
for name, info in json.loads(os.environ["WS_JSON"]).items():
    w = os.path.join(d, info["location"])
    dirs.append(w)
    names.add(name)
    ws_real.add(os.path.realpath(w))
nms, extra, linked = [], [], []
for w in dirs:
    nm = os.path.join(w, "node_modules")
    if not os.path.isdir(nm) or os.path.islink(nm):
        continue
    nms.append(nm)
    entries = []
    for e in os.listdir(nm):
        if e.startswith("."):
            continue
        s = os.path.join(nm, e)
        if e.startswith("@") and os.path.isdir(s) and not os.path.islink(s):
            entries += [f"{e}/{x}" for x in os.listdir(s) if not x.startswith(".")]
        else:
            entries.append(e)
    for e in entries:
        p = os.path.join(nm, e)
        if os.path.islink(p) and os.path.realpath(p) not in ws_real:
            linked.append(e)
        elif e not in names:
            extra.append(e)
if linked:
    sys.exit(print("LINKED " + " ".join(sorted(set(linked)))))
if extra:
    sys.exit(print("EXTRA " + " ".join(sorted(set(extra)))))
print("\n".join(nms))
EOF
)
  case $yplan in
    LINKED*) echo "SKIP linked      $dir  (${yplan#LINKED }: a symlink, not a workspace; yarn link?)"; n_skip=$((n_skip+1)); continue ;;
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
  # Copy-on-write backups outside the repository, made and checked before
  # anything is removed or installed; put back if the install fails or the
  # script is interrupted.
  bk="$HOME/.cache/matchapack/backup/$(printf '%s' "$dir" | shasum | cut -c1-16)"
  rm -rf "${bk:?}" "$bk.paths"
  if [ $kind = npm ]; then nms=("$dir/node_modules")
  else nms=(); while IFS= read -r p; do [ -n "$p" ] && nms+=("$p"); done <<< "$yplan"; fi
  why=
  if ! make_backups; then
    rm -rf "${bk:?}" "$bk.paths"
    echo "SKIP no backup   $dir  ($why)"; n_skip=$((n_skip+1)); continue
  fi
  pending=1
  if [ $kind = npm ]; then
    # npm ci removes node_modules itself (under MATCHAPACK_NO_FALLBACK=1 it
    # hands over, exit 3, before changing anything).
    out=$(cd "$dir" && MATCHAPACK_NO_FALLBACK=1 matchapack npm ci --no-audit --no-fund 2>&1 < /dev/null); rc=$?
    done_line=$(printf '%s\n' "$out" | grep -o 'matchapack: added.*' | sed 's/^matchapack: //')
  else
    for p in "${nms[@]}"; do rm -rf "${p:?}"; done
    out=$(cd "$dir" && MATCHAPACK_NO_FALLBACK=1 matchapack yarn install --frozen-lockfile --non-interactive 2>&1 < /dev/null); rc=$?
    done_line=$(printf '%s\n' "$out" | grep -o 'matchapack: [0-9]* files cloned.*\|matchapack: no clones in this install.*' | sed 's/^matchapack: //' | tr '\n' ' ')
  fi
  case $rc in
    0) pending=; echo "CONVERTED        $dir  ($done_line)"; n_conv=$((n_conv+1)); rm -rf "${bk:?}" "$bk.paths" ;;
    3) if [ $kind = yarn ]; then restore; else rm -rf "${bk:?}" "$bk.paths"; fi; pending=
       echo "SKIP not handled $dir  ($(printf '%s\n' "$out" | grep -o 'not handled, left unchanged.*' | sed 's/^not handled, left unchanged //'))"; n_skip=$((n_skip+1)) ;;
    *) echo "FAILED (rc=$rc)  $dir  (previous node_modules restored)"; printf '%s\n' "$out" | grep -E 'matchapack|npm error|^error' | head -3 | sed 's/^/    /'; n_fail=$((n_fail+1))
       restore; pending= ;;
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
