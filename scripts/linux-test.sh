#!/usr/bin/env bash
# Linux FICLONE test, run inside a privileged container:
#   docker run --rm --privileged -v "$PWD":/src:ro rust:1-bookworm bash /src/scripts/linux-test.sh
# Builds matchapack, then for btrfs and XFS (reflink=1) on loop devices runs the
# harness on fixtures/small and fixtures/optional-upward with both engines
# (classic, then MATCHAPACK_ENGINE=inject) and the clone server's unit tests
# with their scratch directory on that filesystem, checks that cloned
# files share extents with the store, and checks that a filesystem WITHOUT
# reflink (the container's overlayfs) makes matchapack fail loudly.
set -euo pipefail
log() { echo; echo "######## $*"; }
# Run a check, print only the lines that matter, keep the check's status: a
# failed check is counted and makes the runner fail at the end (the filter
# alone would hide it).
failures=0
filtered() { # pattern command...
  local pat=$1 out rc=0; shift
  out=$("$@" 2>&1) || rc=$?
  grep -E "$pat" <<<"$out" || true
  if [ "$rc" -ne 0 ]; then echo "CHECK FAILED (status $rc): $*"; failures=$((failures+1)); fi
}

log "setup"
apt-get update -qq >/dev/null
apt-get install -y -qq btrfs-progs xfsprogs python3 xz-utils >/dev/null
arch=$(uname -m); case $arch in aarch64) narch=arm64;; x86_64) narch=x64;; esac
curl -fsSL "https://nodejs.org/dist/v24.22.1/node-v24.22.1-linux-$narch.tar.xz" | tar -xJ -C /opt
export PATH=/opt/node-v24.22.1-linux-$narch/bin:$PATH
node -v; npm -v; uname -r
mkdir -p /build && cp -r /src /build/src && rm -rf /build/src/target
(cd /build/src && CARGO_TARGET_DIR=/build/target cargo build --release -q)
bin=/build/target/release/matchapack
mkdir -p /build/src/target/release && cp $bin /build/src/target/release/matchapack   # verify.sh looks here

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
  export MATCHAPACK_STORE=$mnt/store npm_config_cache=$mnt/npmcache
  local w=$mnt/work; mkdir -p $w/src-small $w/src-ou
  cp /src/fixtures/small/package.json /src/fixtures/small/package-lock.json $w/src-small/
  cp /src/fixtures/optional-upward/package.json /src/fixtures/optional-upward/package-lock.json $w/src-ou/
  for f in small ou; do
    echo "== $fs / $f"
    filtered 'RESULT|only in|differ:|^A:|^B:|npm ls:|clone:|touch' \
      /build/src/scripts/verify.sh $w/src-$f $w/v-$f --npm-cache $mnt/npmcache/_cacache
    # The inject engine: npm's own npm ci, packages placed by the clone
    # server with per-file FICLONE (no directory reflink on Linux).
    echo "== $fs / $f, inject engine"
    filtered 'RESULT|only in|differ:|^A:|^B:|npm ls:|inject:|cloned nothing|byte-identical' \
      env MATCHAPACK_ENGINE=inject MATCHAPACK_VERIFY_CLONES=some MATCHAPACK_VERIFY_BYTES=1 /build/src/scripts/verify.sh $w/src-$f $w/vi-$f
  done
  # The clone server's unit tests place packages only where the filesystem
  # can clone: run them with the temporary directory on this filesystem.
  echo "== $fs: cargo test serve:: (placement tests on a reflink filesystem)"
  mkdir -p $mnt/tmp
  filtered 'test result|skipped|FAILED|panicked' \
    env -C /build/src TMPDIR=$mnt/tmp CARGO_TARGET_DIR=/build/target cargo test --release -q serve::
  echo "== $fs: placed esbuild binary shares extents with the store? (classic, then inject)"
  for v in v-small vi-small; do
    f=$(find $w/$v/B/node_modules/@esbuild -name esbuild -path '*bin*' -type f | head -1)
    echo "file: ${f#$w/}"
    case $fs in
      btrfs) btrfs filesystem du -s "$f" 2>/dev/null | tail -1 ;;
      xfs) xfs_io -c "fiemap -v" "$f" | sed -n 3p ;;
    esac
  done
  sf=$(find $MATCHAPACK_STORE/v2 -path "*bin/esbuild" -type f | head -1)
  [ "$fs" = xfs ] && xfs_io -c "fiemap -v" "$sf" | sed -n 3p
  df -k $mnt | tail -1
}
run_fs btrfs
run_fs xfs

log "negative: store and project on overlayfs (no reflink) must fail, not copy"
mkdir -p /neg/p && cp /src/fixtures/small/package*.json /neg/p/
if MATCHAPACK_STORE=/neg/store npm_config_cache=/mnt/btrfs/npmcache "$bin" install /neg/p --npm-cache /mnt/btrfs/npmcache/_cacache --no-rebuild 2>&1 | tail -2; then
  echo "NEGATIVE TEST FAILED: install succeeded on overlayfs"; failures=$((failures+1))
else
  echo "refused as expected; files placed: $(find /neg/p/node_modules -type f 2>/dev/null | wc -l)"
fi
log "negative: store on btrfs, project on overlayfs (cross-device) must fail"
rm -rf /neg/p/node_modules
if MATCHAPACK_STORE=/mnt/btrfs/store "$bin" install /neg/p --npm-cache /mnt/btrfs/npmcache/_cacache --no-rebuild 2>&1 | tail -2; then
  echo "NEGATIVE TEST FAILED: cross-device install succeeded"; failures=$((failures+1))
else
  echo "refused as expected; files placed: $(find /neg/p/node_modules -type f 2>/dev/null | wc -l)"
fi
log "checks failed: $failures"
[ "$failures" -eq 0 ]
