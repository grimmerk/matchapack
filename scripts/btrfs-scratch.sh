#!/usr/bin/env bash
# A btrfs filesystem (or XFS with reflink=1) in a loop-mounted image file,
# owned by the caller, for the npm suites on Linux (PR CI's linux-npm job,
# the npm canary) and the large-repository job (linux-large.yml):
#   btrfs-scratch.sh <mount-point> [size, default 6G] [btrfs|xfs, default btrfs]
# The image is sparse and sits next to the mount point (<mount-point>.img).
# Needs root or passwordless sudo (a GitHub-hosted ubuntu runner has both,
# and so does the privileged container scripts/linux-test.sh uses). Prints
# the filesystem type and fails unless it is the one asked for, so a missing
# kernel module never turns into a run on the runner's ext4, where nothing
# can be cloned.
set -euo pipefail
mnt=${1:?usage: btrfs-scratch.sh <mount-point> [size] [btrfs|xfs]}; size=${2:-6G}; want=${3:-btrfs}
mnt=${mnt%/}
sudo=; [ "$(id -u)" = 0 ] || sudo=sudo
case $want in
  btrfs) mkfs=mkfs.btrfs; pkg=btrfs-progs ;;
  xfs) mkfs=mkfs.xfs; pkg=xfsprogs ;;
  *) echo "not a filesystem this script makes: $want (btrfs or xfs)" >&2; exit 2 ;;
esac
if ! command -v "$mkfs" >/dev/null; then
  $sudo apt-get install -y -qq "$pkg" >/dev/null || { $sudo apt-get update -qq && $sudo apt-get install -y -qq "$pkg" >/dev/null; }
fi
img=$mnt.img
[ ! -e "$img" ] || { echo "$img exists already" >&2; exit 2; }
mkdir -p "$(dirname "$mnt")"
truncate -s "$size" "$img"
case $want in
  btrfs) mkfs.btrfs -q "$img" ;;
  # reflink=1 is the default of current xfsprogs; said anyway, so an older
  # one cannot make a filesystem without it.
  xfs) mkfs.xfs -q -m reflink=1 "$img" ;;
esac
$sudo mkdir -p "$mnt"
$sudo mount -o loop "$img" "$mnt"
$sudo chown "$(id -u):$(id -g)" "$mnt"
fs=$(stat -f -c %T "$mnt")
echo "$mnt: $fs, $(df -h "$mnt" | awk 'NR==2 {print $2}')"
[ "$fs" = "$want" ] || { echo "not $want: $fs" >&2; exit 1; }
