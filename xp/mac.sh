#!/bin/bash
# Experiment: the same small-file, git and build workloads on local APFS, the sandfs
# NFSv3 mount, and a minimal passthrough FUSE file system served by fuse-t over NFSv4,
# SMB and FSKit (and macFUSE's FSKit backend when MACFUSE=1).
set -uo pipefail
X=$(cd "$(dirname "$0")" && pwd)
S=${SANDFS_BIN:-$PWD/target/release/sandfs}
W=${RUNNER_TEMP:-/tmp}/xp; rm -rf "$W"; mkdir -p "$W"
R=$W/results.txt
log() { echo "$*" | tee -a "$R"; }
now() { python3 -c 'import time;print(time.time())'; }
phase() { # label dir command...
  local label=$1 dir=$2; shift 2
  nfsstat -c > "$W/n0" 2>/dev/null
  local s; s=$(now)
  (cd "$dir" && "$@") > "$W/out" 2>&1; local rc=$?
  local e; e=$(now)
  nfsstat -c > "$W/n1" 2>/dev/null
  local calls; calls=$(python3 "$X/nfsdiff.py" "$W/n0" "$W/n1" 2>/dev/null)
  log "$label: $(python3 -c "print(round($e-$s,3))")s rc=$rc ${calls:+nfs: $calls}"
  [ $rc = 0 ] || tail -5 "$W/out" | sed 's/^/    /' | tee -a "$R"
}
smallfiles() { # label dir
  nfsstat -c > "$W/n0" 2>/dev/null
  python3 "$X/ops.py" "$2/smallfiles" 1000 > "$W/ops" 2>&1 || cat "$W/ops"
  nfsstat -c > "$W/n1" 2>/dev/null
  log "$1 smallfiles: $(head -1 "$W/ops") nfs: $(python3 "$X/nfsdiff.py" "$W/n0" "$W/n1" 2>/dev/null)"
  rm -rf "$2/smallfiles"
}
suite() { # label dir
  local L=$1 D=$2
  log "== $L: $(mount | grep -F " $D " | sed 's/^[^(]*//')"
  smallfiles "$L" "$D"
  phase "$L git status (first)" "$D" git status --porcelain
  phase "$L git status" "$D" git status --porcelain
  phase "$L cargo build (first)" "$D" cargo build --release --offline -q
  phase "$L cargo build (no-op)" "$D" cargo build --release --offline -q
  echo "// edit" >> "$D/crates/core/main.rs"
  phase "$L cargo build (one edit)" "$D" cargo build --release --offline -q
  phase "$L git status (after build)" "$D" git status --porcelain
  phase "$L rm -rf target" "$D" rm -rf target
}

sw_vers | tee -a "$R"; sysctl -n hw.ncpu machdep.cpu.brand_string | tee -a "$R"
git clone -q https://github.com/BurntSushi/ripgrep "$W/rg-src"
git -C "$W/rg-src" checkout -q 14.1.1
(cd "$W/rg-src" && cargo fetch -q)

# 1. Local disk.
git clone -q "$W/rg-src" "$W/local"
suite local "$W/local"
rm -rf "$W/local"

# 2. The sandfs NFSv3 mount: a fork of the local clone, mounted.
export SANDFS_HOME="$W/sandfs-home"
mkdir -p "$W/m-sandfs"
if "$S" fork "$W/rg-src" "$W/m-sandfs" --mount --branch xp > "$W/out" 2>&1; then
  suite "sandfs nfsv3" "$W/m-sandfs"
  "$S" rm --unmount "$W/m-sandfs" > /dev/null 2>&1 || "$S" unmount "$W/m-sandfs"
else
  log "sandfs fork failed: $(tail -3 "$W/out")"
fi

# 3. The passthrough file system over each fuse-t backend.
passthrough() { # label lib-flags backend-options...
  local label=$1 flags=$2; shift 2
  local B="$W/b-$label" M="$W/m-$label"
  git clone -q "$W/rg-src" "$B"; mkdir -p "$M"
  "$W/pt-$LIB" "$B" "$M" "$@" > "$W/pt.log" 2>&1 &
  local pid=$!
  for _ in $(seq 1 60); do mount | grep -qF " $M " && break; sleep 0.5; done
  if mount | grep -qF " $M "; then
    suite "$label" "$M"
  else
    log "$label: mount did not appear: $(tail -5 "$W/pt.log" | tr '\n' ' ')"
  fi
  umount "$M" 2>/dev/null || diskutil unmount force "$M" >/dev/null 2>&1
  kill $pid 2>/dev/null; wait $pid 2>/dev/null
  rm -rf "$B"
}
if [ "${FUSET:-1}" = 1 ]; then
  LIB=fuset
  cc -O2 -o "$W/pt-fuset" "$X/passthrough.c" -D_FILE_OFFSET_BITS=64 $(pkg-config --cflags --libs fuse-t) 2>&1 | tail -5
  passthrough "fuse-t nfs" "" -f -o backend=nfs
  passthrough "fuse-t smb" "" -f -o backend=smb
  passthrough "fuse-t fskit" "" -f -o backend=fskit
fi
if [ "${MACFUSE:-0}" = 1 ]; then
  LIB=macfuse
  cc -O2 -o "$W/pt-macfuse" "$X/passthrough.c" -I/usr/local/include/fuse -D_FILE_OFFSET_BITS=64 -L/usr/local/lib -lfuse 2>&1 | tail -5
  passthrough "macfuse fskit" "" -f -o backend=fskit
fi
log "done"
cat "$R" >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
