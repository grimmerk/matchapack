#!/usr/bin/env bash
# Linux FICLONE test, run inside a privileged container:
#   docker run --rm --privileged -v "$PWD":/src:ro rust:1-bookworm bash /src/scripts/linux-test.sh
# Builds lockclone, then for btrfs and XFS (reflink=1) on loop devices runs the
# harness on fixtures/small and fixtures/optional-upward, checks that cloned
# files share extents with the store, and checks that a filesystem WITHOUT
# reflink (the container's overlayfs) makes lockclone fail loudly.
set -euo pipefail
log() { echo; echo "######## $*"; }

log "setup"
apt-get update -qq >/dev/null
apt-get install -y -qq btrfs-progs xfsprogs python3 xz-utils >/dev/null
arch=$(uname -m); case $arch in aarch64) narch=arm64;; x86_64) narch=x64;; esac
curl -fsSL "https://nodejs.org/dist/v24.22.1/node-v24.22.1-linux-$narch.tar.xz" | tar -xJ -C /opt
export PATH=/opt/node-v24.22.1-linux-$narch/bin:$PATH
node -v; npm -v; uname -r
mkdir -p /build && cp -r /src /build/src && rm -rf /build/src/target
(cd /build/src && CARGO_TARGET_DIR=/build/target cargo build --release -q)
bin=/build/target/release/lockclone
mkdir -p /build/src/target/release && cp $bin /build/src/target/release/lockclone   # verify.sh looks here

# Serve the optional-upward tarballs (its lockfile points at 127.0.0.1:48321).
(cd /src/fixtures/optional-upward/tarballs && python3 -m http.server 48321 --bind 127.0.0.1 >/dev/null 2>&1 &)
until curl -sf -o /dev/null http://127.0.0.1:48321/lc-q-1.0.0.tgz; do sleep 0.3; done

run_fs() {
  local fs=$1 mnt=/mnt/$1
  log "$fs"
  truncate -s 6G /img-$fs
  case $fs in
    btrfs) mkfs.btrfs -q /img-$fs ;;
    xfs) mkfs.xfs -q -m reflink=1 /img-$fs ;;
  esac
  mkdir -p $mnt && mount -o loop /img-$fs $mnt
  export LOCKCLONE_STORE=$mnt/store npm_config_cache=$mnt/npmcache
  local w=$mnt/work; mkdir -p $w/src-small $w/src-ou
  cp /src/fixtures/small/package.json /src/fixtures/small/package-lock.json $w/src-small/
  cp /src/fixtures/optional-upward/package.json /src/fixtures/optional-upward/package-lock.json $w/src-ou/
  for f in small ou; do
    echo "== $fs / $f"
    /build/src/scripts/verify.sh $w/src-$f $w/v-$f --npm-cache $mnt/npmcache/_cacache \
      | grep -E 'RESULT|only in|differ:|^A:|^B:|npm ls:|clone:|touch' || true
  done
  echo "== $fs: placed esbuild binary shares extents with the store?"
  f=$(find $w/v-small/B/node_modules/@esbuild -name esbuild -path '*bin*' -type f | head -1)
  echo "file: ${f#$w/}"
  case $fs in
    btrfs) btrfs filesystem du -s "$f" 2>/dev/null | tail -1 ;;
    xfs) xfs_io -c "fiemap -v" "$f" | sed -n 3p ;;
  esac
  sf=$(find $LOCKCLONE_STORE/v2 -path "*bin/esbuild" -type f | head -1)
  [ "$fs" = xfs ] && xfs_io -c "fiemap -v" "$sf" | sed -n 3p
  df -k $mnt | tail -1
}
run_fs btrfs
run_fs xfs

log "negative: store and project on overlayfs (no reflink) must fail, not copy"
mkdir -p /neg/p && cp /src/fixtures/small/package*.json /neg/p/
if LOCKCLONE_STORE=/neg/store npm_config_cache=/mnt/btrfs/npmcache "$bin" install /neg/p --npm-cache /mnt/btrfs/npmcache/_cacache --no-rebuild 2>&1 | tail -2; then
  echo "NEGATIVE TEST FAILED: install succeeded on overlayfs"
else
  echo "refused as expected; files placed: $(find /neg/p/node_modules -type f 2>/dev/null | wc -l)"
fi
log "negative: store on btrfs, project on overlayfs (cross-device) must fail"
rm -rf /neg/p/node_modules
if LOCKCLONE_STORE=/mnt/btrfs/store "$bin" install /neg/p --npm-cache /mnt/btrfs/npmcache/_cacache --no-rebuild 2>&1 | tail -2; then
  echo "NEGATIVE TEST FAILED: cross-device install succeeded"
else
  echo "refused as expected; files placed: $(find /neg/p/node_modules -type f 2>/dev/null | wc -l)"
fi
