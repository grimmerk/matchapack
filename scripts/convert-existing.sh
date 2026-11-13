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
#     project (a dev server, a watcher, a shell), checked at the start and
#     again just before anything in the project changes;
#   - projects with packages in node_modules that the lockfile does not list
#     (`npm link`, `yarn link`, `--no-save`): a reinstall would drop them;
#     for Yarn also any package that is a symlink other than a workspace's
#     (`yarn link` replaces an installed package with one);
#   - projects whose plan (lockfile, workspaces) or backup could not be made.
# An install that cloned nothing (Yarn's cache on another volume, say) is
# not a conversion: the previous node_modules is put back.
#
# Each project goes through these states; every failure leaves it in a known
# one (NOTES "Yarn 1 shell route", decision 5):
#   plan      read-only checks, the list of node_modules to replace;
#             a failure: SKIP, nothing changed
#   backing   each node_modules copied (copy-on-write) to a new folder of
#             ~/.cache/matchapack/backup (one per attempt; a backup an
#             earlier run kept for the project: SKIP, it is never removed or
#             overwritten) and checked against the original (paths, types,
#             sizes, link targets, owner, group, mode); workspace
#             node_modules that do not exist yet are recorded;
#             a failure or a signal: the backups are removed, nothing changed
#   armed     checked again that no process is in the project, then the
#             node_modules removed (Yarn) and the install run; a signal
#             (SIGINT, SIGTERM, SIGHUP): wait for the install's processes to
#             leave the project, restore, exit 130
#   outcome   cloned: the backups are removed; declined, failed or nothing
#             cloned: restore (and remove the node_modules that did not
#             exist before)
#   restoring signals are ignored (also by `rm` and `mv`); each node_modules
#             is put back at most once, only into a place that is empty; a
#             backup that cannot be put back is kept, and its place said.
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
all_cwds() { lsof -a -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | sort -u; }
cwds=$(all_cwds)
[ -n "$cwds" ] || { echo "lsof reported no working directories; refusing to run" >&2; exit 2; }
# in_use <dir> [<cwds>]: a process has its working directory at or below
# <dir> (exact prefix: /x/app does not match /x/app-review). Without <cwds>,
# asked of lsof now; an empty answer counts as in use (cannot tell).
in_use() {
  local c=${2-}
  if [ $# -lt 2 ]; then c=$(all_cwds); [ -n "$c" ] || return 0; fi
  printf '%s\n' "$c" | awk -v d="$1" '$0 == d || index($0, d "/") == 1 { f = 1 } END { exit !f }'
}

# A copy of tree $1 at $2, checked (same_tree). On macOS an APFS clone (`cp
# -c -pR`), whose files take the group of the directory they are cloned
# into, so the original groups are set back on them; if the clone still
# differs (or fails: another volume, a filesystem that cannot clone), a full
# `cp -pR`. Elsewhere `cp -a --reflink=auto`. On failure $why says why.
copy_tree() {
  if [ "$(uname)" = Darwin ]; then
    if cp -c -pR "$1" "$2" 2>/dev/null && set_groups "$1" "$2" && same_tree "$1" "$2"; then return 0; fi
    rm -rf "${2:?}"
    cp -pR "$1" "$2" 2>/dev/null || { why="copying $1 failed"; return 1; }
  else
    cp -a --reflink=auto "$1" "$2" 2>/dev/null || { why="copying $1 failed"; return 1; }
  fi
  same_tree "$1" "$2" || { why="the copy of $1 does not match it"; return 1; }
}
# Give each path in tree $2 the group of the same path in tree $1 (lchown;
# the user's own groups only, as `cp -p` would).
set_groups() {
  python3 -I - "$1" "$2" <<'EOF'
import os, sys
a, b = sys.argv[1], sys.argv[2]
for d, dirs, files in os.walk(a, onerror=lambda e: sys.exit(1)):
    for n in dirs + files:
        p = os.path.join(d, n)
        q = os.path.join(b, os.path.relpath(p, a))
        g = os.lstat(p).st_gid
        if os.lstat(q).st_gid != g:
            os.lchown(q, -1, g)
EOF
}
# Whether tree $2 has the same paths as $1, each of the same type, owner,
# group and mode, regular files of the same size, symlinks with the same
# target (lstat; links are not followed).
same_tree() {
  python3 -I - "$1" "$2" <<'EOF'
import os, stat, sys
def scan(top):
    out = {}
    for d, dirs, files in os.walk(top, onerror=lambda e: sys.exit(1)):
        for n in dirs + files:
            p = os.path.join(d, n)
            st = os.lstat(p)
            out[os.path.relpath(p, top)] = (stat.S_IFMT(st.st_mode), stat.S_IMODE(st.st_mode), st.st_uid, st.st_gid,
                                            st.st_size if stat.S_ISREG(st.st_mode) else 0, os.readlink(p) if stat.S_ISLNK(st.st_mode) else "")
    return out
sys.exit(0 if scan(sys.argv[1]) == scan(sys.argv[2]) else 1)
EOF
}
# Back up each of ${nms[@]} as $bk/<n>, its path as line n of $bk.paths,
# each copy checked; ${news[@]} (node_modules that do not exist yet) go to
# $bk.new. On failure $why says what failed.
make_backups() {
  local i
  mkdir -p "$bk" || { why="cannot create $bk"; return 1; }
  : > "$bk.paths" || { why="cannot write $bk.paths"; return 1; }
  : > "$bk.done" || { why="cannot write $bk.done"; return 1; }
  : > "$bk.new" || { why="cannot write $bk.new"; return 1; }
  for i in "${!news[@]}"; do printf '%s\n' "${news[$i]}" >> "$bk.new" || { why="cannot write $bk.new"; return 1; }; done
  for i in "${!nms[@]}"; do
    copy_tree "${nms[$i]}" "$bk/$i" || return 1
    printf '%s\n' "${nms[$i]}" >> "$bk.paths" || { why="cannot write $bk.paths"; return 1; }
  done
  [ "$(wc -l < "$bk.paths" | tr -d ' ')" = "${#nms[@]}" ] || { why="$bk.paths is incomplete"; return 1; }
}
discard_backups() { rm -rf "${bk:?}" "$bk.paths" "$bk.done" "$bk.new"; }
# Put every backed-up node_modules back, each at most once ($bk.done lists
# the ones already back), and only into a place that is empty: if the
# current tree cannot be removed completely, that backup is kept and its
# place said. Signals are ignored meanwhile (also by rm and mv, which
# inherit that). Returns 1 when a backup was kept.
restore() {
  trap '' INT TERM HUP
  state=restoring
  local i=0 p ok=1
  while IFS= read -r p; do
    if ! grep -qx "$i" "$bk.done" 2>/dev/null; then
      rm -rf "${p:?}" 2>/dev/null
      if [ -e "$p" ] || [ -L "$p" ]; then
        ok=0; echo "    could not remove $p: its previous node_modules is kept at $bk/$i"
      elif ! mv "$bk/$i" "$p" || [ -e "$bk/$i" ] || [ ! -d "$p" ]; then
        ok=0; echo "    could not put back $p: its previous node_modules is kept at $bk/$i"
      else
        echo "$i" >> "$bk.done"
        # tests: a signal arriving between two put-backs
        [ -n "${MATCHAPACK_CONVERT_TEST_PAUSE_RESTORE-}" ] && sleep "$MATCHAPACK_CONVERT_TEST_PAUSE_RESTORE"
      fi
    fi
    i=$((i+1))
  done < "$bk.paths"
  # node_modules the install made where there was none before.
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    rm -rf "${p:?}" 2>/dev/null
    if [ -e "$p" ] || [ -L "$p" ]; then ok=0; echo "    could not remove $p (made by the install; there was none before)"; fi
  done < "$bk.new"
  if [ $ok = 1 ]; then discard_backups; else echo "    (manifest: $bk.paths)"; fi
  state=idle
  trap on_signal INT TERM HUP
  [ $ok = 1 ]
}
# A signal: in `backing`, remove the backups made so far; in `armed`, wait
# (up to 20 s) for the processes of the install to leave the project, then
# restore; if some are still there, restoring under them is not safe: the
# backups are kept and their place said. Then stop.
state=idle
on_signal() {
  trap '' INT TERM HUP
  case $state in
    backing) discard_backups; echo "INTERRUPTED      $dir  (nothing changed)" ;;
    armed)
      local t=0
      while [ $t -lt "${MATCHAPACK_CONVERT_TEST_WAIT_TICKS:-40}" ] && in_use "$dir"; do sleep 0.5; t=$((t+1)); done
      if in_use "$dir"; then
        echo "INTERRUPTED      $dir  (processes are still in the project: not restored automatically; the previous node_modules is kept at $bk, manifest $bk.paths)"
        exit 130
      fi
      if restore; then echo "INTERRUPTED      $dir  (previous node_modules restored)"
      else echo "INTERRUPTED      $dir  (previous node_modules NOT fully restored: see above)"; fi
      trap '' INT TERM HUP ;;
  esac
  exit 130
}
trap on_signal INT TERM HUP
free_kb() { df -k "$1" | tail -1 | awk '{print $4}'; }
before=$(free_kb "${roots[0]}")
n_conv=0; n_skip=0; n_fail=0
skip() { echo "$1"; n_skip=$((n_skip+1)); }
errf=$(mktemp "${TMPDIR:-/tmp}/convert-existing.XXXXXX") || exit 2
while IFS= read -r lock; do
  # Canonical (lsof reports physical paths), for every check below.
  dir=$(cd "$(dirname "$lock")" 2>/dev/null && pwd -P) || continue
  case $lock in */yarn.lock) kind=yarn ;; *) kind=npm ;; esac
  [ -f "$dir/package.json" ] && [ -d "$dir/node_modules" ] || continue
  # A project with both lockfiles is converted as an npm one (as before).
  [ $kind = yarn ] && [ -f "$dir/package-lock.json" ] && continue
  if [ $kind = yarn ] && [ "$(uname)" != Darwin ]; then
    skip "SKIP yarn, not macOS $dir  (plain Yarn 1 already clones here)"; continue
  fi
  if in_use "$dir" "$cwds"; then skip "SKIP in use      $dir"; continue; fi
  # A backup an earlier run kept (its rollback could not finish) is the
  # only copy of that tree: the project waits until the owner has dealt
  # with it, and nothing here removes or reuses it.
  key=$(printf '%s' "$dir" | shasum | cut -c1-16)
  old=$(ls -d "$HOME/.cache/matchapack/backup/$key"* 2>/dev/null | head -1)
  if [ -n "$old" ]; then skip "SKIP old backup  $dir  (an earlier run kept a backup of it: $old; put it back or remove it by hand)"; continue; fi

  # --- plan (read-only; any failure: SKIP) ---
  news=()
  if [ $kind = npm ]; then
  # Packages in node_modules that the lockfile does not list (`npm link`,
  # `npm install --no-save`) would be dropped by npm ci semantics: skip.
  if ! extra=$(python3 -I - "$dir" 2>"$errf" <<'EOF'
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
); then skip "SKIP no plan     $dir  (reading package-lock.json: $(tail -1 "$errf" | cut -c1-80))"; continue; fi
  nms=("$dir/node_modules")
  else
  # Yarn: a Yarn 2+ project is not matchapack's (told from files, as
  # `matchapack yarn` tells it).
  if ! y2=$(python3 -I - "$dir" 2>"$errf" <<'EOF'
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
); then skip "SKIP no plan     $dir  (reading package.json: $(tail -1 "$errf" | cut -c1-80))"; continue; fi
  if [ -n "$y2" ]; then skip "SKIP Yarn 2+     $dir  ($y2)"; continue; fi
  # The workspaces, as Yarn lists them (its own pattern rules): the JSON on
  # stdout only (Yarn's warnings go to stderr).
  ws_json='{}'
  if python3 -I -c 'import json,sys; sys.exit(0 if json.load(open(sys.argv[1])).get("workspaces") else 1)' "$dir/package.json" 2>/dev/null; then
    if ! ws_json=$(cd "$dir" && matchapack yarn --silent workspaces info 2>"$errf" < /dev/null); then
      skip "SKIP workspaces  $dir  (yarn workspaces info: $(tail -1 "$errf" | cut -c1-80))"; continue
    fi
  fi
  # The node_modules directories to replace (the root's first, then each
  # workspace's); any package in them that is neither in yarn.lock nor a
  # workspace (a manual copy), and any package that is a symlink other than
  # to a workspace (`yarn link`), either of which a reinstall would drop.
  # Nested node_modules (node_modules/<dep>/node_modules/…) are checked
  # the same way. A workspace node_modules (or a nested one) that is a
  # symlink or not a directory cannot be backed up as planned: "ODD".
  # Prints "ODD <paths>", "LINKED <names>", "EXTRA <names>", or "PLAN", one node_modules
  # path per line, and "NEW <path>" for each workspace node_modules that
  # does not exist yet (Yarn may make it; a rollback removes it); exits
  # non-zero when it cannot plan (the workspace list is not Yarn's JSON,
  # say).
  if ! yplan=$(WS_JSON=$ws_json python3 -I - "$dir" 2>"$errf" <<'EOF'
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
ws = json.loads(os.environ["WS_JSON"])
if not isinstance(ws, dict):
    sys.exit("the workspace list is not an object")
dirs, ws_real = [d], set()
for name, info in ws.items():
    w = os.path.join(d, info["location"])
    if not os.path.isfile(os.path.join(w, "package.json")):
        sys.exit(f"workspace {name}: no package.json at {w}")
    dirs.append(w)
    names.add(name)
    ws_real.add(os.path.realpath(w))
nms, extra, linked, new, odd = [], [], [], [], []
def check(nm):
    # Each package of this node_modules, and of the node_modules nested in
    # its packages (stat-level; package symlinks are not followed).
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
        if os.path.islink(p):
            if os.path.realpath(p) not in ws_real:
                linked.append(os.path.relpath(p, d))
            continue
        if e not in names:
            extra.append(os.path.relpath(p, d))
        sub = os.path.join(p, "node_modules")
        if os.path.islink(sub) or (os.path.lexists(sub) and not os.path.isdir(sub)):
            odd.append(os.path.relpath(sub, d))
        elif os.path.isdir(sub):
            check(sub)
for w in dirs:
    nm = os.path.join(w, "node_modules")
    if not os.path.lexists(nm):
        new.append(nm)
        continue
    if os.path.islink(nm) or not os.path.isdir(nm):
        if w != d:
            odd.append(os.path.relpath(nm, d))
        continue
    nms.append(nm)
    check(nm)
if odd:
    sys.exit(print("ODD " + " ".join(sorted(set(odd)))))
if linked:
    sys.exit(print("LINKED " + " ".join(sorted(set(linked)))))
if extra:
    sys.exit(print("EXTRA " + " ".join(sorted(set(extra)))))
print("PLAN")
print("\n".join(nms))
print("\n".join("NEW " + n for n in new))
EOF
); then skip "SKIP no plan     $dir  ($(tail -1 "$errf" | cut -c1-80))"; continue; fi
  extra=
  case $yplan in
    LINKED*) skip "SKIP linked      $dir  (${yplan#LINKED }: a symlink, not a workspace; yarn link?)"; continue ;;
    ODD*) skip "SKIP odd         $dir  (${yplan#ODD }: a node_modules that is not a plain directory)"; continue ;;
    EXTRA*) extra=${yplan#EXTRA } ;;
    PLAN*) nms=(); news=()
      while IFS= read -r p; do case $p in "") ;; "NEW "*) news+=("${p#NEW }") ;; *) nms+=("$p") ;; esac; done <<< "${yplan#PLAN}" ;;
    *) skip "SKIP no plan     $dir  (unexpected plan output)"; continue ;;
  esac
  fi
  if [ -n "$extra" ]; then skip "SKIP not in lock $dir  (${extra:0:80})"; continue; fi
  # The plan always names the project's own node_modules first.
  if [ "${nms[0]-}" != "$dir/node_modules" ]; then skip "SKIP no plan     $dir  (the plan does not name $dir/node_modules)"; continue; fi
  if [ $apply = 0 ]; then
    echo "CANDIDATE $kind $(printf '%*s' $((6 - ${#kind})) '')$dir"; continue
  fi

  # --- backing (a backup of its own per attempt, never one that exists) ---
  bk="$HOME/.cache/matchapack/backup/$key-$(date +%Y%m%d%H%M%S)-$$"
  [ -e "$bk" ] && { skip "SKIP no backup   $dir  ($bk exists)"; continue; }
  state=backing
  why=
  if ! make_backups; then discard_backups; state=idle; skip "SKIP no backup   $dir  ($why)"; continue; fi
  # --- armed: checked again, just before the first change ---
  if in_use "$dir"; then discard_backups; state=idle; skip "SKIP in use      $dir  (a process entered it during this run)"; continue; fi
  state=armed
  if [ $kind = npm ]; then
    # npm ci removes node_modules itself (under MATCHAPACK_NO_FALLBACK=1 it
    # hands over, exit 3, before changing anything).
    out=$(cd "$dir" && MATCHAPACK_NO_FALLBACK=1 matchapack npm ci --no-audit --no-fund 2>&1 < /dev/null); rc=$?
    done_line=$(printf '%s\n' "$out" | grep -o 'matchapack: added.*' | sed 's/^matchapack: //')
    # Cloned: "added N packages from the store", where N is what was placed
    # as clones (src/wrapper.rs: the classic engine's placed entries, the
    # inject engine's stats.cloned; packages npm extracted itself are the
    # separate "npm extracted E").
    if [[ $done_line =~ ^added\ ([0-9]+)\ packages\ from\ the\ store ]] && [ "${BASH_REMATCH[1]}" -gt 0 ]; then cloned=yes; else cloned=no; fi
  else
    for p in "${nms[@]}"; do rm -rf "${p:?}"; done
    out=$(cd "$dir" && MATCHAPACK_NO_FALLBACK=1 matchapack yarn install --frozen-lockfile --non-interactive 2>&1 < /dev/null); rc=$?
    done_line=$(printf '%s\n' "$out" | grep -o 'matchapack: [0-9]* files cloned.*\|matchapack: no clones in this install.*' | sed 's/^matchapack: //' | tr '\n' ' ')
    n=$(printf '%s\n' "$out" | sed -n 's/^matchapack: \([0-9]*\) files cloned.*/\1/p' | tail -1)
    if [ "${n:-0}" -gt 0 ]; then cloned=yes; else cloned=no; fi
  fi
  # --- outcome ---
  # (Not in a subshell: restore changes this shell's traps and state.)
  put_back() { if restore; then res="previous node_modules restored"; else res="previous node_modules NOT fully restored: see above"; fi; }
  case $rc in
    0) if [ $cloned = yes ]; then
         state=idle; discard_backups; echo "CONVERTED        $dir  ($done_line)"; n_conv=$((n_conv+1))
       else
         # Installed, but nothing shares blocks: the new tree gains nothing,
         # so the user's previous tree is put back as it was.
         put_back; skip "NOT CONVERTED    $dir  (nothing was cloned: ${done_line:-no clone count}; $res)"
       fi ;;
    3) if [ $kind = yarn ]; then put_back; else state=idle; discard_backups; res="nothing changed"; fi
       skip "SKIP not handled $dir  ($(printf '%s\n' "$out" | grep -o 'not handled, left unchanged.*' | sed 's/^not handled, left unchanged //'); $res)" ;;
    *) put_back; echo "FAILED (rc=$rc)  $dir  ($res)"; printf '%s\n' "$out" | grep -E 'matchapack|npm error|^error' | head -3 | sed 's/^/    /'; n_fail=$((n_fail+1)) ;;
  esac
  state=idle
# -prune: never descend into node_modules or .git (a -not -path filter would
# still walk millions of files and take minutes).
done < <(for r in "${roots[@]}"; do find "$r" -maxdepth "$depth" \( -name node_modules -o -name .git \) -prune -o \( -name package-lock.json -o -name yarn.lock \) -print 2>/dev/null; done | sort)
rm -f "$errf"

if [ $apply = 1 ]; then
  sync; sleep 2
  after=$(free_kb "${roots[0]}")
  echo "converted $n_conv, skipped $n_skip, failed $n_fail; free space $((before/1048576)) GiB → $((after/1048576)) GiB"
else
  echo "dry run: nothing changed. Candidates matchapack cannot handle are found and skipped at --apply."
fi
