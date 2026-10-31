#!/usr/bin/env bash
# Time and REAL disk cost (df, not du: du counts clones at full size) of one install.
#   measure.sh <label> <source-project-dir> <dest-dir> -- <install command...>
# The source is clone-copied to <dest>, then the command runs inside <dest>.
# Prints: label, seconds, df delta in MiB. Other processes writing to the same
# volume add noise to the df delta; repeat runs and compare orders of magnitude.
set -euo pipefail
label=$1; src=$2; dest=$3; shift 3
[ "$1" = "--" ] && shift
vol=$(df -k "$(dirname "$dest")" | tail -1 | awk '{print $NF}')
cp -c -R "$src" "$dest"
sync; sleep 2
f0=$(df -k "$vol" | tail -1 | awk '{print $4}')
t0=$(python3 -c 'import time;print(time.time())')
(cd "$dest" && "$@" > "$dest.log" 2>&1) || { echo "$label: FAILED (see $dest.log)"; tail -5 "$dest.log"; exit 1; }
t1=$(python3 -c 'import time;print(time.time())')
sync; sleep 3
f1=$(df -k "$vol" | tail -1 | awk '{print $4}')
python3 -c "print(f'$label: {$t1-$t0:7.2f}s  disk {($f0-$f1)/1024:9.1f} MiB')"
