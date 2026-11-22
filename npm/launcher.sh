#!/bin/sh
# The `matchapack` command of the npm package (`npm i -g matchapack`). The
# package holds the four native binaries, native/<os>-<cpu>/matchapack; this
# script picks the one for this machine and execs it. No install script and
# no download: it works under --ignore-scripts and offline, and exec leaves
# the signals and the exit status to the binary. Built into the package by
# scripts/npm-packages.js.
#
# It runs at every `npm ci` routed through matchapack and at every shell
# start (the init line), so the usual case starts no other program: the
# platform comes from the shell's own variables where it sets them, and the
# package from fixed paths next to npm's link to this file.

# The platform. bash (macOS's /bin/sh) sets OSTYPE and HOSTTYPE, from the
# CPU the shell itself runs as; elsewhere (dash, busybox) one `uname -sm`.
# shellcheck disable=SC3028 # read only where set; unset falls to uname
case ${OSTYPE-}:${HOSTTYPE-} in
  darwin*:arm64) sys="Darwin arm64" ;;
  darwin*:x86_64) sys="Darwin x86_64" ;;
  linux*:x86_64) sys="Linux x86_64" ;;
  linux*:aarch64) sys="Linux aarch64" ;;
  *) sys=$(uname -sm) ;;
esac
case $sys in
  "Darwin arm64") plat=darwin-arm64 ;;
  "Darwin x86_64")
    # An Intel Mac, or an Apple silicon Mac running this shell under Rosetta
    # (an Intel terminal or Node): Rosetta's runtime exists only on Apple
    # silicon, and there the native binary runs, faster than the Intel one
    # would under translation.
    if [ -e /Library/Apple/usr/libexec/oah/libRosettaRuntime ]; then plat=darwin-arm64; else plat=darwin-x64; fi
    ;;
  "Linux x86_64") plat=linux-x64 ;;
  "Linux aarch64" | "Linux arm64") plat=linux-arm64 ;;
  *)
    echo "matchapack: the npm package has binaries for macOS (Apple silicon, Intel) and Linux (x86_64, arm64), not for $sys." >&2
    echo "matchapack: other ways to install, or to build it: https://github.com/grimmerk/matchapack#install" >&2
    exit 1
    ;;
esac

# The package: this file is <package>/bin/matchapack. npm runs it through a
# link it made in a bin directory: <prefix>/bin for a global install (to
# ../lib/node_modules/matchapack/bin/matchapack), node_modules/.bin for a
# local one or npx (to ../matchapack/bin/matchapack). Those two layouts are
# tried as fixed paths, each only when the link really leads to that
# package's launcher (`-ef`: the same file, in the test builtin of bash, dash
# and busybox), so a link to another install, a newer one elsewhere, never
# runs a package that merely sits next to it. Any other link is followed
# with readlink.
self=$0
case $self in */*) ;; *) self=./$self ;; esac
bin=native/$plat/matchapack
if [ ! -L "$self" ]; then
  pkg=${self%/*}/..
elif [ "$self" -ef "${self%/*}/../lib/node_modules/matchapack/bin/matchapack" ]; then
  pkg=${self%/*}/../lib/node_modules/matchapack
elif [ "$self" -ef "${self%/*}/../matchapack/bin/matchapack" ]; then
  pkg=${self%/*}/../matchapack
else
  # At most 40 links, as the kernel allows, so that a loop cannot hang.
  hops=0
  while [ -L "$self" ] && [ "$hops" -lt 40 ]; do
    hops=$((hops + 1))
    link=$(readlink "$self") || break
    case $link in
      /*) self=$link ;;
      *) self=${self%/*}/$link ;;
    esac
  done
  pkg=${self%/*}/..
fi

if [ ! -x "$pkg/$bin" ]; then
  echo "matchapack: no $bin next to $0 (looked in $pkg)." >&2
  echo "matchapack: reinstall it (npm i -g matchapack); a global install is the supported way to use the npm package." >&2
  exit 1
fi
exec "$pkg/$bin" "$@"
