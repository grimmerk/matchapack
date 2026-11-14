#!/usr/bin/env bash
# A btrfs filesystem in a loop-mounted image file, owned by the caller, for
# the npm suites on Linux (PR CI's linux-npm job and the npm canary):
#   btrfs-scratch.sh <mount-point> [size, default 6G]
# The image is sparse and sits next to the mount point (<mount-point>.img).
# Needs root or passwordless sudo (a GitHub-hosted ubuntu runner has both,
# and so does the privileged container scripts/linux-test.sh uses). Prints
# the filesystem type and fails unless it is btrfs, so a missing kernel module
# never turns into a run on the runner's ext4, where nothing can be cloned.
set -euo pipefail
mnt=${1:?usage: btrfs-scratch.sh <mount-point> [size]}; size=${2:-6G}
mnt=${mnt%/}
sudo=; [ "$(id -u)" = 0 ] || sudo=sudo
if ! command -v mkfs.btrfs >/dev/null; then
  $sudo apt-get install -y -qq btrfs-progs >/dev/null || { $sudo apt-get update -qq && $sudo apt-get install -y -qq btrfs-progs >/dev/null; }
fi
img=$mnt.img
[ ! -e "$img" ] || { echo "$img exists already" >&2; exit 2; }
mkdir -p "$(dirname "$mnt")"
truncate -s "$size" "$img"
mkfs.btrfs -q "$img"
$sudo mkdir -p "$mnt"
$sudo mount -o loop "$img" "$mnt"
$sudo chown "$(id -u):$(id -g)" "$mnt"
fs=$(stat -f -c %T "$mnt")
echo "$mnt: $fs, $(df -h "$mnt" | awk 'NR==2 {print $2}')"
[ "$fs" = btrfs ] || { echo "not btrfs: $fs" >&2; exit 1; }
