#!/bin/sh
# Tests npm/launcher.sh, the `matchapack` command of the npm package, with
# stub binaries (scripts that print which package and platform they are):
#   scripts/test-npm-launcher.sh [launcher]
# - the package lookup, under each of /bin/sh, dash and bash found: npm's
#   global link and a project's node_modules/.bin link run with no readlink
#   (a fake readlink on PATH counts the calls), for the published scoped
#   name (@grimmerk/matchapack) and the unscoped one; a link to an install
#   elsewhere runs that install, not an older package that sits next to the
#   link; with both names installed in one prefix, the one the link leads
#   to; the launcher's own path; a chain of other links;
# - the platform choice, under dash with a fake uname (dash sets no OSTYPE
#   or HOSTTYPE): each supported `uname -sm`, an unsupported one (exit 1);
# - a missing binary (exit 1).
# Prints one line per case and PASS or FAIL; exit status 1 on a failure.
set -u
here=$(cd "$(dirname "$0")/.." && pwd)
launcher=${1:-$here/npm/launcher.sh}
T=$(mktemp -d "${TMPDIR:-/tmp}/npm-launcher.XXXXXX") || exit 2
trap 'rm -rf "$T"' EXIT
case $(uname -sm) in
  "Darwin arm64") plat=darwin-arm64 ;;
  "Darwin x86_64") if [ -e /Library/Apple/usr/libexec/oah/libRosettaRuntime ]; then plat=darwin-arm64; else plat=darwin-x64; fi ;;
  "Linux x86_64") plat=linux-x64 ;;
  "Linux aarch64" | "Linux arm64") plat=linux-arm64 ;;
  *) echo "test-npm-launcher: no package for $(uname -sm)"; exit 2 ;;
esac

# mkpkg <package dir> <label>: a package with the launcher and four stubs.
mkpkg() {
  mkdir -p "$1/bin"
  cp "$launcher" "$1/bin/matchapack" && chmod 755 "$1/bin/matchapack"
  for p in darwin-arm64 darwin-x64 linux-arm64 linux-x64; do
    mkdir -p "$1/native/$p"
    printf '#!/bin/sh\necho "%s %s $*"\n' "$2" "$p" > "$1/native/$p/matchapack"
    chmod 755 "$1/native/$p/matchapack"
  done
}
# The newer install in npm's global layout, the same in a project, and
# another prefix whose bin/matchapack links to the newer install while an
# older package of its own sits next to it.
mkpkg "$T/new/lib/node_modules/matchapack" newer
mkdir -p "$T/new/bin" "$T/proj/node_modules/.bin" "$T/old/bin" "$T/links/a" "$T/links/b"
ln -s ../lib/node_modules/matchapack/bin/matchapack "$T/new/bin/matchapack"
mkpkg "$T/proj/node_modules/matchapack" project
ln -s ../matchapack/bin/matchapack "$T/proj/node_modules/.bin/matchapack"
mkpkg "$T/old/lib/node_modules/matchapack" older
ln -s "$T/new/lib/node_modules/matchapack/bin/matchapack" "$T/old/bin/matchapack"
ln -s "$T/new/bin/matchapack" "$T/links/a/one"
ln -s ../a/one "$T/links/b/two"
# The same with the published scoped name, and a prefix with both names
# whose link leads to the unscoped one (the scoped one is tried first).
mkpkg "$T/snew/lib/node_modules/@grimmerk/matchapack" scoped
mkdir -p "$T/snew/bin" "$T/sproj/node_modules/.bin" "$T/sold/bin" "$T/both/bin"
ln -s ../lib/node_modules/@grimmerk/matchapack/bin/matchapack "$T/snew/bin/matchapack"
mkpkg "$T/sproj/node_modules/@grimmerk/matchapack" scoped-project
ln -s ../@grimmerk/matchapack/bin/matchapack "$T/sproj/node_modules/.bin/matchapack"
mkpkg "$T/sold/lib/node_modules/@grimmerk/matchapack" scoped-older
ln -s "$T/snew/lib/node_modules/@grimmerk/matchapack/bin/matchapack" "$T/sold/bin/matchapack"
mkpkg "$T/both/lib/node_modules/@grimmerk/matchapack" both-scoped
mkpkg "$T/both/lib/node_modules/matchapack" both-unscoped
ln -s ../lib/node_modules/matchapack/bin/matchapack "$T/both/bin/matchapack"
mkdir -p "$T/fake" "$T/uname"
real_readlink=$(command -v readlink)
printf '#!/bin/sh\necho called >> "%s/readlink.calls"\nexec "%s" "$@"\n' "$T" "$real_readlink" > "$T/fake/readlink"
chmod 755 "$T/fake/readlink"

fail=0
# check <name> <want output> <want exit> <want readlink calls|any> <command...>
check() {
  name=$1 want=$2 want_rc=$3 want_calls=$4; shift 4
  rm -f "$T/readlink.calls"
  got=$(PATH="$T/fake:$PATH" "$@" 2>&1); rc=$?
  calls=0; [ -f "$T/readlink.calls" ] && calls=$(wc -l < "$T/readlink.calls" | tr -d ' ')
  case $got in
    "$want"*) ok=1 ;;
    *) ok=0 ;;
  esac
  if [ $ok = 1 ] && [ "$rc" = "$want_rc" ] && { [ "$want_calls" = any ] || [ "$calls" = "$want_calls" ]; }; then
    echo "ok: $name"
  else
    echo "FAIL: $name: got '$got' (exit $rc, readlink calls $calls), want '${want}...' (exit $want_rc, readlink calls $want_calls)"
    fail=1
  fi
}

for sh in /bin/sh /bin/dash /bin/bash; do
  [ -x "$sh" ] || continue
  check "$sh: npm's global link" "newer $plat a b" 0 0 "$sh" "$T/new/bin/matchapack" a b
  check "$sh: a project's .bin link" "project $plat a" 0 0 "$sh" "$T/proj/node_modules/.bin/matchapack" a
  check "$sh: a link to an install elsewhere, an older package next to it" "newer $plat" 0 any "$sh" "$T/old/bin/matchapack"
  check "$sh: the launcher itself" "newer $plat" 0 0 "$sh" "$T/new/lib/node_modules/matchapack/bin/matchapack"
  check "$sh: a chain of other links" "newer $plat" 0 any "$sh" "$T/links/b/two"
  check "$sh: npm's global link, scoped name" "scoped $plat a b" 0 0 "$sh" "$T/snew/bin/matchapack" a b
  check "$sh: a project's .bin link, scoped name" "scoped-project $plat a" 0 0 "$sh" "$T/sproj/node_modules/.bin/matchapack" a
  check "$sh: a link to a scoped install elsewhere, an older one next to it" "scoped $plat" 0 any "$sh" "$T/sold/bin/matchapack"
  check "$sh: both names in one prefix, the link to the unscoped one" "both-unscoped $plat" 0 0 "$sh" "$T/both/bin/matchapack"
  check "$sh: the scoped launcher itself" "scoped $plat" 0 0 "$sh" "$T/snew/lib/node_modules/@grimmerk/matchapack/bin/matchapack"
done

# The platform choice. dash only: bash sets OSTYPE and HOSTTYPE itself, so
# its answer is this machine's, covered above.
if [ -x /bin/dash ]; then
  rosetta=darwin-x64; [ -e /Library/Apple/usr/libexec/oah/libRosettaRuntime ] && rosetta=darwin-arm64
  for case in "Darwin arm64=darwin-arm64" "Darwin x86_64=$rosetta" "Linux x86_64=linux-x64" "Linux aarch64=linux-arm64" "Linux arm64=linux-arm64"; do
    printf '#!/bin/sh\necho "%s"\n' "${case%%=*}" > "$T/uname/uname"; chmod 755 "$T/uname/uname"
    check "dash, uname -sm '${case%%=*}'" "newer ${case#*=}" 0 0 env PATH="$T/uname:$PATH" /bin/dash "$T/new/bin/matchapack"
  done
  printf '#!/bin/sh\necho "FreeBSD amd64"\n' > "$T/uname/uname"
  check "dash, an unsupported platform" "matchapack: the npm package has binaries for macOS" 1 0 env PATH="$T/uname:$PATH" /bin/dash "$T/new/bin/matchapack"
fi

rm "$T/new/lib/node_modules/matchapack/native/$plat/matchapack"
check "a missing binary" "matchapack: no native/$plat/matchapack" 1 0 /bin/sh "$T/new/bin/matchapack"
rm "$T/snew/lib/node_modules/@grimmerk/matchapack/native/$plat/matchapack"
check "a missing binary, scoped name: reinstall by that name" "matchapack: no native/$plat/matchapack next to $T/snew/bin/matchapack (looked in $T/snew/bin/../lib/node_modules/@grimmerk/matchapack).
matchapack: reinstall it (npm i -g @grimmerk/matchapack)" 1 0 /bin/sh "$T/snew/bin/matchapack"

[ $fail = 0 ] && echo PASS || echo FAIL
exit $fail
