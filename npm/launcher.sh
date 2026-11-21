#!/bin/sh
# matchapack's command in the npm package (`npm i -g matchapack`): the
# `matchapack` that npm links onto PATH is this script, which finds the native
# binary of the platform package npm installed with it (an optional
# dependency, @matchapack/<os>-<cpu>, picked by its "os" and "cpu" fields) and
# execs it. No install script and no download: it works under
# --ignore-scripts and offline, and exec leaves the signals and the exit
# status to the binary. Built into the package by scripts/npm-packages.js.

# Follow the links to this file (npm links <prefix>/bin/matchapack to
# ../lib/node_modules/matchapack/bin/matchapack), one at a time: readlink
# without -f works on macOS and Linux alike.
self=$0
while [ -L "$self" ]; do
  link=$(readlink "$self")
  case $link in
    /*) self=$link ;;
    *) self=$(dirname "$self")/$link ;;
  esac
done
pkg=$(cd "$(dirname "$self")/.." && pwd -P) || exit 1

case $(uname -s) in
  Darwin) os=darwin ;;
  Linux) os=linux ;;
  *) os=$(uname -s) ;;
esac
case $(uname -m) in
  arm64 | aarch64) cpu=arm64 ;;
  x86_64 | amd64) cpu=x64 ;;
  *) cpu=$(uname -m) ;;
esac
# On an Apple silicon Mac an Intel Node (Rosetta) installs darwin-x64, which
# runs there too; the native one is tried first.
plats="$os-$cpu"
[ "$os-$cpu" = darwin-arm64 ] && plats="$plats darwin-x64"

for plat in $plats; do
  # Nested under this package (a global install), or next to it (a local
  # install or npx, where npm hoists it; pnpm's layout too).
  for bin in "$pkg/node_modules/@matchapack/$plat/bin/matchapack" "$pkg/../@matchapack/$plat/bin/matchapack"; do
    if [ -f "$bin" ] && [ -x "$bin" ]; then
      exec "$bin" "$@"
    fi
  done
done

echo "matchapack: no binary for $os-$cpu next to $pkg" >&2
echo "matchapack: npm installs it as the optional dependency @matchapack/$os-$cpu; reinstall without --omit=optional (or --no-optional)." >&2
echo "matchapack: builds exist for macOS (arm64, x64) and Linux with glibc (arm64, x64); elsewhere see https://github.com/grimmerk/matchapack#install" >&2
exit 1
