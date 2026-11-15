#!/usr/bin/env bash
# Step harness for the npm install family (npm install, update, uninstall,
# and ci): the same sequence of npm commands in two copies of a project,
# plain npm in A and `matchapack npm` (the path the shell function takes) in
# B, compared after every step.
#
#   verify-steps.sh <source project> <work dir> [options] -- <step> [-- <step> ...]
#     --start bare|ci|nolock   the starting state (default bare): the source
#                              as is; after plain `npm ci` in A and
#                              `matchapack npm ci` in B; without its
#                              package-lock.json
#     --pre '<shell>'          run in A and in B after the start, before the
#                              first step (the same change on both sides;
#                              `$NPM` is plain npm)
#   step: [@<subdirectory>] [NAME=value ...] <npm arguments ...>
#     @<subdirectory>          where npm starts (a workspace member, a
#                              directory next to --prefix)
#     NAME=value               set for both sides (npm_config_before=...,
#                              MATCHAPACK_INJECT_HOOK=none: B only reads it)
#     RUN=<shell>              run in A and in B (at the project) before the
#                              step: the same change on both sides
#     CLONES=some|none|<n>     an expectation, not passed on: what B's hook
#                              cloned in this step
#     UNSEEN=<n>               the "unseen" count B must log (default 0)
#   Every step also gets --no-audit --no-fund.
#
# After every step, all of these must hold (the floor, NOTES "The install
# family"):
#   - the two exit statuses are equal;
#   - `matchapack diff --normalize-roots A B`: identical, and the hidden
#     lockfile byte-identical (every file of the project is compared, so
#     package.json, package-lock.json and the workspaces' package.json too);
#   - `cmp` of package.json and package-lock.json at the root and of every
#     package.json outside node_modules, so that a failure names the file;
#   - npm trusts B's hidden lockfile exactly when it trusts A's
#     (scripts/hidden-lockfile-trusted.js);
#   - `npm ls --all`: the same output and status;
#   - no path under node_modules is older than the step in B but not in A
#     (verify.sh's mtime rule);
#   - the step's CLONES expectation;
#   - B's log record: outcome (installed, or npm_failed with npm's status),
#     npm's command, the root (B), the unseen count (UNSEEN, default 0), and
#     the project record for `store gc`: every entry of it is a package in B's
#     hidden lockfile, and for the install family every entry of the record
#     before the step whose package is still there is still in it.
# MATCHAPACK_VERIFY_META=1: also `diff --meta`, with each side's whole
# sequence as its run (times written during the steps may differ; anything
# else, times of files outside the runs included, must be equal).
#
# The store is the caller's (MATCHAPACK_STORE); scripts/verify-npm.sh gives
# one in its work directory. The trial log goes to <work>/logs. Records of
# the copies are removed at the end, as verify.sh does.
# MATCHAPACK_VERIFY_STEPS_CORRUPT=<path> (the harness's self-test): after
# B's first step, change one byte of B's <path>; the run must fail and name
# it. MATCHAPACK_VERIFY_STEPS_SELFTEST=checker-dies (the log checker exits
# before printing anything), wrong-command (it expects another npm command
# than the step's), touch-b (after B's step, one file plain npm leaves alone
# is touched in B), extra-record (B's step leaves two log records) or
# untrust-b (a package directory in B made newer than its hidden lockfile):
# the run must fail.
set -uo pipefail
src=${1:?usage: verify-steps.sh <source> <work> [--start bare|ci|nolock] [--pre cmd] -- <step> [-- <step> ...]}
work=$2; shift 2
start=bare; pre=
while [ $# -gt 0 ] && [ "$1" != -- ]; do
  case $1 in
    --start) start=$2; shift 2 ;;
    --pre) pre=$2; shift 2 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
[ "${1-}" = -- ] || { echo "no steps (-- <step>)" >&2; exit 2; }
shift
# The steps, each an array of words in steps_<n>.
nsteps=0; cur=()
flush() { eval "steps_$nsteps=(\"\${cur[@]}\")"; nsteps=$((nsteps+1)); cur=(); }
for w in "$@"; do if [ "$w" = -- ]; then flush; else cur+=("$w"); fi; done
flush

here=$(cd "$(dirname "$0")/.." && pwd)
bin=$here/target/release/matchapack
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 2; }
# npm's package directory as npm itself reports it (as verify-npm.sh: also
# right behind a version manager's shim).
npm_dir=$(cd "$(dirname "$(npm exec -c 'printf %s "$npm_execpath"')")/.." && pwd)
rm -rf "${work:?}/A" "${work:?}/B"
mkdir -p "$work"; work=$(cd "$work" && pwd -P)
export MATCHAPACK_LOG_DIR=$work/logs
rm -rf "${MATCHAPACK_LOG_DIR:?}"
# As verify.sh: the harness installs on any npm, inside its own trial scope.
export MATCHAPACK_NPM_VERSIONS=${MATCHAPACK_NPM_VERSIONS:-any} MATCHAPACK_NPM_FINGERPRINT=${MATCHAPACK_NPM_FINGERPRINT:-any} MATCHAPACK_PATHS=$work
unset MATCHAPACK MATCHAPACK_ENGINE MATCHAPACK_NO_FALLBACK
store=${MATCHAPACK_STORE:-$HOME/.cache/matchapack/store}
forget_copies() {
  python3 -I - "$store/projects" "$work" <<'EOF' || echo "could not remove this run's project records from $store/projects" >&2
import json, os, re, sys
d, w = sys.argv[1], sys.argv[2].rstrip("/") + "/"
for n in (os.listdir(d) if os.path.isdir(d) else []):
    p = os.path.join(d, n)
    try:
        path = json.load(open(p)).get("path")
    except (OSError, ValueError, AttributeError):
        continue
    if re.fullmatch(r"[0-9a-f]{32}\.json", n) and isinstance(path, str) and path.startswith(w):
        os.remove(p)
EOF
}
trap forget_copies EXIT
if [ "$(uname)" = Darwin ]; then cpcow() { cp -c -R -p "$1" "$2"; }; else cpcow() { cp -a --reflink=auto "$1" "$2"; }; fi
cpcow "$src" "$work/A" || { echo "could not copy $src to $work/A" >&2; exit 2; }
cpcow "$src" "$work/B" || { echo "could not copy $src to $work/B" >&2; exit 2; }
A=$work/A; B=$work/B
ns() { python3 -c 'import time;print(time.time_ns())'; }
common=(--no-audit --no-fund)
NPM="npm"; export NPM
fail=0
say_fail() { fail=1; echo "  FAIL $*"; }

# B's project record for store gc (entry keys, one per line; nothing if none).
record_of() {
  python3 -I - "$store/projects" "$B" <<'EOF'
import json, os, sys
d, want = sys.argv[1], sys.argv[2]
for n in (os.listdir(d) if os.path.isdir(d) else []):
    try:
        r = json.load(open(os.path.join(d, n)))
    except (OSError, ValueError):
        continue
    if r.get("path") == want:
        print("\n".join(r.get("entries", [])))
EOF
}

ta0=; tb0=; ta1=; tb1=
case $start in
  bare) ;;
  nolock) rm -f "$A/package-lock.json" "$B/package-lock.json" ;;
  ci)
    ta0=$(ns); (cd "$A" && npm ci "${common[@]}" > "$work/start-A.log" 2>&1) || { echo "start: plain npm ci failed"; tail -5 "$work/start-A.log"; exit 1; }
    tb0=$(ns); (cd "$B" && "$bin" npm ci "${common[@]}" > "$work/start-B.log" 2>&1) || { echo "start: matchapack npm ci failed"; tail -5 "$work/start-B.log"; exit 1; }
    grep -q '^matchapack: added' "$work/start-B.log" || { echo "start: matchapack npm ci did not install:"; grep '^matchapack' "$work/start-B.log"; exit 1; }
    ;;
  *) echo "unknown --start $start" >&2; exit 2 ;;
esac
if [ -n "$pre" ]; then
  (cd "$A" && eval "$pre") > "$work/pre-A.log" 2>&1 || { echo "--pre failed in A"; tail -5 "$work/pre-A.log"; exit 1; }
  (cd "$B" && eval "$pre") > "$work/pre-B.log" 2>&1 || { echo "--pre failed in B"; tail -5 "$work/pre-B.log"; exit 1; }
fi

for ((i = 0; i < nsteps; i++)); do
  eval "step=(\"\${steps_$i[@]}\")"
  sub=.; envs=(); clones=; unseen=0; args=(); run=
  for w in "${step[@]}"; do
    if [ ${#args[@]} = 0 ] && [ "${w:0:1}" = @ ] && [ "$sub" = . ]; then sub=${w:1}
    elif [ ${#args[@]} = 0 ] && [[ "$w" =~ ^CLONES=(.*)$ ]]; then clones=${BASH_REMATCH[1]}
    elif [ ${#args[@]} = 0 ] && [[ "$w" =~ ^RUN=(.*)$ ]]; then run=${BASH_REMATCH[1]}
    elif [ ${#args[@]} = 0 ] && [[ "$w" =~ ^UNSEEN=(.*)$ ]]; then unseen=${BASH_REMATCH[1]}
    elif [ ${#args[@]} = 0 ] && [[ "$w" =~ ^[A-Za-z_][A-Za-z0-9_]*= ]]; then envs+=("$w")
    else args+=("$w"); fi
  done
  label="${envs[*]+${envs[*]} }npm ${args[*]}"
  [ "$sub" != . ] && label="(in $sub) $label"
  if [ -n "$run" ]; then
    (cd "$A" && eval "$run") > "$work/run$i-A.log" 2>&1 || { echo "RUN failed in A: $run"; exit 1; }
    (cd "$B" && eval "$run") > "$work/run$i-B.log" 2>&1 || { echo "RUN failed in B: $run"; exit 1; }
    label="(after: $run) $label"
  fi
  record_of > "$work/rec.before"
  # B's hidden lockfile before the step: how many packages left or changed.
  cp "$B/node_modules/.package-lock.json" "$work/hidden.before" 2>/dev/null || : > "$work/hidden.before"
  nlog=$( { wc -l < "$MATCHAPACK_LOG_DIR/log.jsonl"; } 2>/dev/null | tr -d ' '); nlog=${nlog:-0}
  marker=$work/.step-start; touch "$marker"; sleep 1
  t=$(ns); [ -z "$ta0" ] && ta0=$t
  (cd "$A/$sub" && env ${envs[@]+"${envs[@]}"} npm "${args[@]}" "${common[@]}" > "$work/step$i-A.log" 2>&1); sa=$?
  ta1=$(ns); [ -z "$tb0" ] && tb0=$ta1
  (cd "$B/$sub" && env ${envs[@]+"${envs[@]}"} "$bin" npm "${args[@]}" "${common[@]}" > "$work/step$i-B.log" 2>&1); sb=$?
  tb1=$(ns)
  if [ $i = 0 ] && [ -n "${MATCHAPACK_VERIFY_STEPS_CORRUPT-}" ]; then
    python3 -c 'import sys; p=sys.argv[1]; b=bytearray(open(p,"rb").read()); b[len(b)//2]^=1; open(p,"wb").write(b)' "$B/$MATCHAPACK_VERIFY_STEPS_CORRUPT"
  fi
  # The self-tests of the time and log checks: B touched a file plain npm
  # left alone; B's step logged two records.
  case ${MATCHAPACK_VERIFY_STEPS_SELFTEST-} in
    touch-b) f=$(cd "$B" && find node_modules -type f ! -newer "$marker" | LC_ALL=C sort | head -1); [ -n "$f" ] && touch "$B/$f" ;;
    # A package directory newer than the hidden lockfile: npm no longer
    # trusts B's (what the trust check reads).
    untrust-b) f=$(cd "$B" && find node_modules -mindepth 1 -maxdepth 1 -type d ! -name '.*' | LC_ALL=C sort | head -1); [ -n "$f" ] && { sleep 1; touch "$B/$f"; } ;;
    extra-record) tail -1 "$MATCHAPACK_LOG_DIR/log.jsonl" >> "$MATCHAPACK_LOG_DIR/log.jsonl" ;;
  esac
  echo "step $((i+1)): $label"
  echo "  plain: $(grep -E '^(added|removed|changed|up to date)' "$work/step$i-A.log" | head -1) | B: $(grep '^matchapack' "$work/step$i-B.log" | tr '\n' ' ')"
  [ $sa = $sb ] || say_fail "exit status: plain npm $sa, matchapack $sb"
  if "$bin" diff --normalize-roots "$A" "$B" > "$work/step$i.diff" 2>&1; then
    if [ -e "$A/node_modules/.package-lock.json" ] || [ -e "$B/node_modules/.package-lock.json" ]; then
      grep -q 'hidden lockfile byte-identical: true' "$work/step$i.diff" || say_fail "hidden lockfile not byte-identical"
    fi
  else
    say_fail "diff: $(grep -vE '^(RESULT|  )' "$work/step$i.diff" | head -3 | tr '\n' ' ')"; sed -n '1,8p' "$work/step$i.diff" | sed 's/^/      /'
  fi
  # The files npm writes, by name.
  while IFS= read -r f; do
    if [ -e "$A/$f" ] || [ -e "$B/$f" ]; then cmp -s "$A/$f" "$B/$f" || say_fail "cmp: $f differs"; fi
  done < <( (cd "$A" && find . -name node_modules -prune -o -name package.json -print; cd "$B" && find . -name node_modules -prune -o -name package.json -print; echo ./package-lock.json; echo ./npm-shrinkwrap.json) | sed 's#^\./##' | LC_ALL=C sort -u)
  if [ -d "$A/node_modules" ] || [ -d "$B/node_modules" ]; then
    trust=$(node "$here/scripts/hidden-lockfile-trusted.js" "$npm_dir" "$A" "$B" 2>&1) || say_fail "hidden-lockfile-trusted.js: $trust"
    ta=$(echo "$trust" | sed -n 1p | cut -f1 | cut -d' ' -f1); tb=$(echo "$trust" | sed -n 2p | cut -f1 | cut -d' ' -f1)
    [ "$ta" = "$tb" ] || say_fail "npm trusts the hidden lockfile: plain $ta, matchapack $tb"
  fi
  ea=0; (cd "$A" && npm ls --all > "$work/A.ls" 2>&1) || ea=$?
  eb=0; (cd "$B" && npm ls --all > "$work/B.ls" 2>&1) || eb=$?
  sed -e "s#$A#<root>#g" -e 's#/[^ ]*_logs/[^ ]*#<log>#' -e '1s#^A@#<name>@#' "$work/A.ls" > "$work/A.ls.n"
  sed -e "s#$B#<root>#g" -e 's#/[^ ]*_logs/[^ ]*#<log>#' -e '1s#^B@#<name>@#' "$work/B.ls" > "$work/B.ls.n"
  { [ $ea = $eb ] && cmp -s "$work/A.ls.n" "$work/B.ls.n"; } || { say_fail "npm ls differs (status $ea / $eb)"; diff "$work/A.ls.n" "$work/B.ls.n" | head -6 | sed 's/^/      /'; }
  for side in A B; do
    if [ -d "$work/$side/node_modules" ]; then (cd "$work/$side" && find node_modules ! -type l ! -newer "$marker" | LC_ALL=C sort) > "$work/$side.stale"; else : > "$work/$side.stale"; fi
  done
  extra=$(LC_ALL=C comm -13 "$work/A.stale" "$work/B.stale" | wc -l | tr -d ' ')
  [ "$extra" = 0 ] || say_fail "$extra paths older than the step in B only: $(LC_ALL=C comm -13 "$work/A.stale" "$work/B.stale" | head -3 | tr '\n' ' ')"
  # Both ways: plain npm leaves the packages it does not change alone, and
  # so must B (a path B wrote in this step that plain npm did not touch).
  extra=$(LC_ALL=C comm -23 "$work/A.stale" "$work/B.stale" | wc -l | tr -d ' ')
  [ "$extra" = 0 ] || say_fail "$extra paths B wrote in this step that plain npm left alone: $(LC_ALL=C comm -23 "$work/A.stale" "$work/B.stale" | head -3 | tr '\n' ' ')"
  # One run, one log record.
  nlog2=$( { wc -l < "$MATCHAPACK_LOG_DIR/log.jsonl"; } 2>/dev/null | tr -d ' '); nlog2=${nlog2:-0}
  [ "$nlog2" = $((nlog + 1)) ] || say_fail "the trial log grew by $((nlog2 - nlog)) records in this step, want 1"
  # B's log record: the last line of the trial log.
  # (Files, not arguments: a record of a large project is over Linux's
  # 128 KiB limit for one argument.)
  tail -1 "$MATCHAPACK_LOG_DIR/log.jsonl" > "$work/rec.json" 2>/dev/null
  record_of > "$work/rec.after"
  # Fail closed: a checker that could not run, or stopped before printing
  # why, fails the step too.
  if ! python3 -I - "$work/rec.json" "$sb" "$B" "$clones" "$unseen" "$work/rec.before" "$work/rec.after" "${args[0]}" "$here/src/wrapper.rs" "$marker" "$work/hidden.before" <<'EOF' > "$work/rec.check" 2> "$work/rec.log"; then
import json, os, re, sys
recf, status, root, clones, unseen, rbf, raf, word, table, marker, hbf = sys.argv[1:12]
# The harness's self-test: a checker that dies before saying anything.
if os.environ.get("MATCHAPACK_VERIFY_STEPS_SELFTEST") == "checker-dies":
    sys.exit(1)
bad = []
try:
    r = json.loads(open(recf).read())
except ValueError:
    print("no log record"); sys.exit(1)
rb, ra = open(rbf).read(), open(raf).read()
inj = r.get("inject") or {}
want = "installed" if status == "0" else "npm_failed"
if r.get("outcome") != want:
    bad.append(f"outcome {r.get('outcome')} ({r.get('reason')}), want {want}")
# npm's command for the step's first word, by the routing table itself
# (src/wrapper.rs ROUTED: `i`, `add`, `up`, `rm`, ... are aliases).
routed = dict(re.findall(r'^\s*\("([^"]+)", "([^"]+)"\),\s*$', open(table).read(), re.M))
want_cmd = routed.get(word)
if os.environ.get("MATCHAPACK_VERIFY_STEPS_SELFTEST") == "wrong-command":
    want_cmd = "update" if want_cmd != "update" else "install"
if want_cmd is None:
    bad.append(f"the step's first word {word!r} is not a routed npm command")
elif r.get("command") != want_cmd or inj.get("command") != want_cmd:
    bad.append(f"command {r.get('command')} (npm's: {inj.get('command')}), want {want_cmd}")
if r.get("root") != root or inj.get("root") != root:
    bad.append(f"root {r.get('root')}, want {root}")
cloned = inj.get("cloned")
if clones == "some" and not cloned:
    bad.append("cloned nothing (CLONES=some)")
elif clones == "none" and cloned:
    bad.append(f"cloned {cloned} (CLONES=none)")
elif clones not in ("", "some", "none") and str(cloned) != clones:
    bad.append(f"cloned {cloned}, want {clones}")
if status == "0" and str(inj.get("unseen")) != unseen and not (unseen == "0" and inj.get("unseen") is None):
    bad.append(f"unseen {inj.get('unseen')}, want {unseen}")
# The project record (store gc): only packages B has; for the install
# family, nothing of the earlier record that B still has is dropped. Whether
# B's hidden lockfile names the source of every package (an integrity or a
# `resolved`): only then do its integrities say what B has (store.rs
# `update_record`).
def is_pkg(k):
    i = k.rfind("node_modules/")
    if i < 0 or (i > 0 and not k[:i].endswith("/")):
        return False
    name = k[i + len("node_modules/"):]
    if "/" in name:
        sc, _, rest = name.partition("/")
        return sc.startswith("@") and rest != "" and "/" not in rest
    return name != "" and not name.startswith("@")
def packages(path):
    """(version, resolved) by package key, and whether every package has an
    integrity or a resolved (the record's rule, store.rs update_record)."""
    try:
        pk = json.load(open(path)).get("packages", {})
    except (OSError, ValueError):
        return {}, False
    ps = {k: p for k, p in pk.items() if is_pkg(k) and not p.get("link") and not p.get("inBundle")}
    return {k: (p.get("version"), p.get("resolved")) for k, p in ps.items()}, all(p.get("integrity") or p.get("resolved") for p in ps.values())
tree = set()
complete = True
# Entries of the packages whose directory was written in this step.
written = set()
t0 = os.stat(marker).st_mtime_ns
try:
    from base64 import b64decode
    pk = json.load(open(os.path.join(root, "node_modules/.package-lock.json"))).get("packages", {})
    rank = ["sha512", "sha384", "sha256", "sha1"]
    for k, p in pk.items():
        best = None
        for part in (p.get("integrity") or "").split():
            a, _, h = part.partition("-")
            if a in rank and (best is None or rank.index(a) < rank.index(best[0])):
                best = (a, b64decode(h.split("?")[0]).hex())
        if not best and is_pkg(k) and not p.get("link") and not p.get("inBundle") and not p.get("resolved"):
            complete = False
        if best:
            tree.add(f"{best[0]}-{best[1]}")
            try:
                if k and os.lstat(os.path.join(root, k)).st_mtime_ns >= t0:
                    written.add(f"{best[0]}-{best[1]}")
            except OSError:
                pass
    have_tree = True
except (OSError, ValueError):
    have_tree = False
before, after = set(rb.split()), set(ra.split())
if have_tree and complete and not after <= tree:
    bad.append(f"the gc record holds {len(after - tree)} entries B's hidden lockfile does not have")
if have_tree and complete and r.get("command") != "ci" and not (before & tree) <= after:
    bad.append(f"the gc record dropped {len((before & tree) - after)} entries B still has")
# A bound that does not use integrities: the record loses at most as many
# entries as package keys that left B's hidden lockfile or changed version
# in this step (a package still installed keeps its entry).
# Only when B's hidden lockfile before the step named every package's
# source: a record kept as a superset (store.rs, an incomplete tree) may
# lose entries later that no package change of this step explains. A
# changed `resolved` counts as a change (a registry package replaced by the
# same version from git).
(hb, hb_complete), (ha, _) = packages(hbf), packages(os.path.join(root, "node_modules/.package-lock.json"))
moved = sum(1 for k, v in hb.items() if ha.get(k) != v)
if hb_complete and len(before - after) > moved:
    bad.append(f"the gc record lost {len(before - after)} entries, but only {moved} packages left or changed")
# A step that cloned: the record has the entries of what it placed.
if have_tree and status == "0" and cloned and not (after & written):
    bad.append(f"cloned {cloned}, but the gc record has none of the {len(written)} entries placed in this step")
print("\n".join(bad))
print(f"  log: {r.get('outcome')} {r.get('command')}, cloned {cloned}, unseen {inj.get('unseen')} of {inj.get('installed')}, record {len(before)} -> {len(after)} entries", file=sys.stderr)
sys.exit(1 if bad else 0)
EOF
    fail=1
    if [ -s "$work/rec.check" ]; then
      while IFS= read -r l; do say_fail "$l"; done < "$work/rec.check"
    else
      say_fail "the log check could not run: $(tail -3 "$work/rec.log" | tr '\n' ' ')"
    fi
  fi
  cat "$work/rec.log"
done

if [ "${MATCHAPACK_VERIFY_META:-0}" = 1 ]; then
  if "$bin" diff --meta --normalize-roots --a-run "$ta0:$ta1" --b-run "$tb0:$tb1" "$A" "$B" > "$work/meta.txt" 2>&1; then
    echo "meta: identical ($(grep -m1 '^meta: [0-9]* paths' "$work/meta.txt" | cut -d' ' -f2) paths: mtime outside the runs, uid, gid, xattr names)"
  else
    say_fail "meta: DIFFERENT"; sed -n '/by class/,$p' "$work/meta.txt" | head -20
  fi
fi
[ $fail = 0 ] && echo "RESULT: every step identical" || echo "RESULT: DIFFERENT"
exit $fail
