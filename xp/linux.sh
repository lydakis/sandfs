#!/bin/bash
# Experiment, run as root: what local copies and FUSE transports cost on Linux.
# A. Copies of a local clone: git worktree, full copy, reflink copy (btrfs, XFS),
#    kernel overlayfs, sandfs fork. B. The same workloads through libfuse's
#    passthrough_hp with and without FUSE passthrough and io_uring, and the sandfs mount.
set -uo pipefail
X=$(cd "$(dirname "$0")" && pwd)
S=${SANDFS_BIN:-$PWD/target/release/sandfs}
HP=${HP_BIN:-/tmp/libfuse/build/example/passthrough_hp}
W=${XP_DIR:-/xp}; rm -rf "$W"; mkdir -p "$W"
R=$W/results.txt
log() { echo "$*" | tee -a "$R"; }
now() { date +%s.%N; }
since() { python3 -c "print(round($(now)-$1,3))"; }
phase() { # label dir command...
  local label=$1 dir=$2; shift 2
  local s; s=$(now)
  (cd "$dir" && "$@") > "$W/out" 2>&1; local rc=$?
  log "$label: $(since "$s")s rc=$rc"
  [ $rc = 0 ] || tail -5 "$W/out" | sed 's/^/    /' | tee -a "$R"
}
used() { df -k --output=used "$1" | tail -1; }
suite() { # label dir
  local L=$1 D=$2
  log "== $L: $(findmnt -no FSTYPE,OPTIONS -T "$D" | cut -c1-160)"
  log "$L smallfiles: $(python3 "$X/ops.py" "$D/smallfiles" 1000 2>&1 | head -1)"
  rm -rf "$D/smallfiles"
  phase "$L git status (first)" "$D" git status --porcelain
  phase "$L git status" "$D" git status --porcelain
  phase "$L grep -r" "$D" grep -rq ZZZ_NOT_THERE .
  phase "$L cargo build (first)" "$D" cargo build --offline -q
  phase "$L cargo build (no-op)" "$D" cargo build --offline -q
  echo "// edit" >> "$D/crates/core/main.rs"
  phase "$L cargo build (one edit)" "$D" cargo build --offline -q
  phase "$L git status (after build, no file monitor)" "$D" git -c core.fsmonitor=false status --porcelain
  phase "$L git status (after build)" "$D" git status --porcelain
}

uname -a | tee -a "$R"; nproc | tee -a "$R"; findmnt -no FSTYPE -T "$W" | tee -a "$R"
git config --global --add safe.directory '*'
git config --global advice.detachedHead false
git clone -q https://github.com/BurntSushi/ripgrep "$W/rg-src"
git -C "$W/rg-src" checkout -q 14.1.1
(cd "$W/rg-src" && cargo fetch -q)

# Scratch file systems that can share blocks between copies.
truncate -s 8G "$W/btrfs.img" "$W/xfs.img"
mkfs.btrfs -q "$W/btrfs.img" && mkdir -p /mnt/btrfs && mount -o loop "$W/btrfs.img" /mnt/btrfs
mkfs.xfs -q -m reflink=1 "$W/xfs.img" && mkdir -p /mnt/xfs && mount -o loop "$W/xfs.img" /mnt/xfs

copies() { # fs-label base-dir
  local F=$1 B=$2
  rm -rf "$B/clone"; git clone -q "$W/rg-src" "$B/clone"; sync
  # Each kind: make the copy (timed, with the space it took), run the suite, remove it.
  make() { # kind dir command...
    local kind=$1 dir=$2; shift 2
    sync; local u0; u0=$(used "$B"); local s; s=$(now)
    if "$@" > "$W/out" 2>&1; then
      local t; t=$(since "$s"); sync
      log "[$F] $kind: copy ${t}s, $(( ($(used "$B") - u0) / 1024 )) MiB"
      suite "[$F] $kind" "$dir"
    else
      log "[$F] $kind: copy failed: $(tail -3 "$W/out" | tr '\n' ' ')"
    fi
  }
  make worktree "$B/wt" git -C "$B/clone" worktree add -q -b wt "$B/wt"
  git -C "$B/clone" worktree remove --force "$B/wt"; git -C "$B/clone" branch -qD wt
  if [ "$F" = ext4 ]; then
    make "full copy" "$B/full" cp -a "$B/clone" "$B/full"; rm -rf "$B/full"
  else
    make "reflink copy" "$B/ref" cp -a --reflink=always "$B/clone" "$B/ref"; rm -rf "$B/ref"
  fi
  mkdir -p "$B/ov-upper" "$B/ov-work" "$B/ov"
  make overlayfs "$B/ov" mount -t overlay overlay -o "lowerdir=$B/clone,upperdir=$B/ov-upper,workdir=$B/ov-work" "$B/ov"
  umount "$B/ov"; rm -rf "$B/ov-upper" "$B/ov-work" "$B/ov"
  export SANDFS_HOME="$B/sandfs-home"
  make "sandfs fork" "$B/fork" "$S" fork "$B/clone" "$B/fork" --branch xp --name "xp-$F-$round"
  "$S" rm --unmount "$B/fork" > /dev/null 2>&1
  if [ "$F" = ext4 ]; then
    export SANDFS_XP_WRITEBACK=1
    make "sandfs fork + writeback cache" "$B/fork" "$S" fork "$B/clone" "$B/fork" --branch xp2 --name "xpwb-$F-$round"
    "$S" rm --unmount "$B/fork" > /dev/null 2>&1
    unset SANDFS_XP_WRITEBACK
  fi
  rm -rf "$B/clone" "$SANDFS_HOME"
}
mkdir -p "$W/ext4"
for round in 1 2; do
  log "### round $round"
  copies ext4 "$W/ext4"
  copies btrfs /mnt/btrfs
  copies xfs /mnt/xfs
done

# B. FUSE transports over the same clone.
if [ -x "$HP" ]; then
  "$HP" --help 2>&1 | head -40 | tee -a "$R"
  hp() { # label options...
    local label=$1; shift
    local B="$W/hp-src" M="$W/hp"
    rm -rf "$B"; git clone -q "$W/rg-src" "$B"; mkdir -p "$M"
    "$HP" "$@" "$B" "$M" > "$W/hp.log" 2>&1 &
    local pid=$!
    for _ in $(seq 1 40); do mountpoint -q "$M" && break; sleep 0.25; done
    if mountpoint -q "$M"; then
      suite "$label" "$M"
      grep -i 'uring\|passthrough' "$W/hp.log" | sort | uniq -c | head -5 | sed 's/^/    log: /' | tee -a "$R"
    else
      log "$label: mount failed: $(tail -3 "$W/hp.log" | tr '\n' ' ')"
    fi
    umount "$M"; kill $pid 2>/dev/null; wait $pid 2>/dev/null; rm -rf "$B"
  }
  hp "fuse plain" --nopassthrough
  hp "fuse passthrough" 
  echo Y > /sys/module/fuse/parameters/enable_uring 2>/dev/null; log "enable_uring=$(cat /sys/module/fuse/parameters/enable_uring 2>&1)"
  hp "fuse io_uring" --nopassthrough -o io_uring
  hp "fuse io_uring + passthrough" -o io_uring
  hp "fuse no cache" --nopassthrough --nocache
fi
log "done"
echo "=== RESULTS ==="; grep -v '^    ' "$R" | grep -v '^      \|^  -\|^Usage\|^options\|^Fuse lowlevel\|^$' 
cat "$R" >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
