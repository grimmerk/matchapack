#!/usr/bin/env bash
# The npm upgrade routine (#11): run the harness on the npm on PATH, and when
# every case is identical to `npm ci`, record its major.minor in
# <config dir>/npm-verified (~/.config/matchapack, or $MATCHAPACK_CONFIG_DIR),
# which matchapack's version gate reads next to the built-in VERIFIED_NPM.
#   verify-npm.sh [--no-record] <work-dir> [extra project dir...]
# Cases: the fixtures (hidden lockfile byte-identical), script-env through the
# shell function, legacy-peer with legacy-peer-deps, then each extra project
# (a real repository is worth adding: the fixtures are small). CI runs it with
# --no-record.
# MATCHAPACK_ENGINE=inject runs the same cases with the inject engine, plus the
# cases only it handles (dep-event), requires every case to have cloned
# something, and never records: npm-verified is the classic engine's gate.
set -uo pipefail
record=1
if [ "${1-}" = --no-record ]; then record=0; shift; fi
work=${1:?usage: verify-npm.sh [--no-record] <work-dir> [extra project dir...]}; shift
here=$(cd "$(dirname "$0")/.." && pwd)
bin=$here/target/release/matchapack
[ -x "$bin" ] || { echo "build first: cargo build --release" >&2; exit 2; }
mkdir -p "$work"
npm_version=$(npm -v) || { echo "no npm on PATH" >&2; exit 2; }
mm=$(echo "$npm_version" | cut -d. -f1-2)
cfg=${MATCHAPACK_CONFIG_DIR:-$HOME/.config/matchapack}
# What is certified is fixed here, never inherited: a caller's settings that
# loosen the comparison (ignored paths, noise filtering, another baseline,
# mtime handling) or route the install differently do not apply.
unset MATCHAPACK_VERIFY_IGNORE MATCHAPACK_VERIFY_NOISE MATCHAPACK_VERIFY_BASELINE MATCHAPACK_VERIFY_MTIME MATCHAPACK_VERIFY_VIA MATCHAPACK_VERIFY_BYTES MATCHAPACK_VERIFY_CLONES
engine=${MATCHAPACK_ENGINE:-classic}
case $engine in
  classic) ;;
  inject)
    if [ $record = 1 ]; then echo "MATCHAPACK_ENGINE=inject: nothing is recorded (npm-verified is the classic engine's gate)"; record=0; fi
    # A run in which the hook never fired would pass without testing it.
    export MATCHAPACK_VERIFY_CLONES=some ;;
  *) echo "MATCHAPACK_ENGINE=$engine is not an engine (classic, inject)" >&2; exit 2 ;;
esac
echo "npm $npm_version, node $(node -v), $("$bin" --version), $engine engine"

# The optional-upward fixture installs from a local tarball server.
server=
if ! curl -sf -o /dev/null http://127.0.0.1:48321/lc-q-1.0.0.tgz; then
  python3 -m http.server 48321 --bind 127.0.0.1 --directory "$here/fixtures/optional-upward/tarballs" >/dev/null 2>&1 &
  server=$!
  for _ in $(seq 100); do curl -sf -o /dev/null http://127.0.0.1:48321/lc-q-1.0.0.tgz && break; sleep 0.1; done
fi
trap '[ -n "$server" ] && kill "$server" 2>/dev/null' EXIT

fail=0
case_() { # name env-assignments... -- source [matchapack install args...]
  local name=$1; shift
  local envs=()
  while [ "$1" != -- ]; do envs+=("$1"); shift; done
  shift
  local out="$work/$name.txt"
  local src=$1; shift
  if env MATCHAPACK_VERIFY_BYTES=1 ${envs[@]+"${envs[@]}"} "$here/scripts/verify.sh" "$src" "$work/$name" "$@" > "$out" 2>&1; then
    echo "ok    $name ($(grep -m1 '^npm ci:' "$out"))"
  else
    fail=1; echo "FAIL  $name (log: $out)"; tail -15 "$out" | sed 's/^/      /'
  fi
}
for f in small stale-flags optional-upward optional-engines legacy-peer bundled; do
  case_ "$f" -- "$here/fixtures/$f"
done
if [ $engine = inject ]; then
  # The root's predependencies / dependencies / postdependencies scripts,
  # which npm ci runs and the classic engine does not (NOTES "Inject engine").
  case_ dep-event -- "$here/fixtures/dep-event"
  # A failing optional dependency script: npm ci removes the package. The
  # classic engine hands that install to npm (routing case 28).
  case_ optional-script-fail -- "$here/fixtures/optional-script-fail"
  # The same package placed by per-file clones (the Linux placement).
  case_ small-file-clones -- "$here/fixtures/small" --clone-mode file
  # Tarballs where an extractor other than pacote's can give another tree
  # (a path listed twice; .gitignore next to .npmignore): the store is
  # filled through pacote.
  case_ odd-tarballs -- "$here/fixtures/odd-tarballs"
fi
# script-env records the npm environment scripts see; the user's flags
# (--no-audit) reach matchapack only through the shell function.
case_ script-env MATCHAPACK_VERIFY_VIA=wrapper -- "$here/fixtures/script-env"
case_ legacy-peer-on npm_config_legacy_peer_deps=true -- "$here/fixtures/legacy-peer"
for p in "$@"; do
  case_ "project-$(basename "$p")" -- "$p"
done

# Add or remove this version's line in npm-verified: under a lock, written to
# a fresh temporary file and renamed over the old one, so concurrent runs never
# lose each other's lines and an error never leaves a partial file. Prints
# what it did; exits 3 on any error (the file is then unchanged).
#   update add|remove <line to add>
update() {
  python3 -c '
import fcntl, os, re, sys, tempfile
cfg, mm, action, line = sys.argv[1:]
f = os.path.join(cfg, "npm-verified")
try:
    os.makedirs(cfg, exist_ok=True)
    with open(f + ".lock", "a") as lk:
        fcntl.flock(lk, fcntl.LOCK_EX)
        try:
            with open(f) as h:
                lines = h.read().splitlines(keepends=True)
        except FileNotFoundError:
            lines = []
        mine = re.compile(r"^\s*" + re.escape(mm) + r"(\s|#|$)")
        present = any(mine.match(l) for l in lines)
        if action == "add":
            if present:
                print("already"); sys.exit(0)
            if lines and not lines[-1].endswith("\n"):
                lines[-1] += "\n"
            lines.append(line + "\n")
        else:
            if not present:
                print("absent"); sys.exit(0)
            lines = [l for l in lines if not mine.match(l)]
        fd, tmp = tempfile.mkstemp(dir=cfg, prefix=".npm-verified.")
        with os.fdopen(fd, "w") as h:
            h.writelines(lines)
            h.flush()
            os.fsync(h.fileno())
        os.replace(tmp, f)
        print("added" if action == "add" else "removed")
except Exception as e:
    print(f"error: {e}", file=sys.stderr); sys.exit(3)
' "$cfg" "$mm" "$1" "${2-}"
}

if [ $fail != 0 ]; then
  # A version recorded by an earlier run is taken back: this run found a
  # difference (perhaps in a project the earlier run did not include).
  if [ $record = 1 ]; then
    r=$(update remove) || { echo "could NOT take $mm back out of $cfg/npm-verified: remove that line by hand" >&2; exit 3; }
    [ "$r" = removed ] && echo "removed $mm from $cfg/npm-verified (recorded by an earlier run)"
  fi
  echo "npm $npm_version: NOT verified; matchapack keeps handing npm ci to npm on it"
  exit 1
fi
if [ $record = 0 ]; then
  echo "npm $npm_version: every case identical (not recorded: --no-record)"
  exit 0
fi
r=$(update add "$mm  # $(date +%F): scripts/verify-npm.sh, npm $npm_version, $("$bin" --version)${*:+, plus $*}") \
  || { echo "npm $npm_version: every case identical, but recording it in $cfg/npm-verified FAILED" >&2; exit 3; }
if [ "$r" = already ]; then
  echo "npm $npm_version: every case identical; $mm was already in $cfg/npm-verified"
else
  echo "npm $npm_version: every case identical; recorded $mm in $cfg/npm-verified"
fi
echo "(for every machine: add \"$mm\" to VERIFIED_NPM in src/wrapper.rs)"
