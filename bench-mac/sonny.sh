#!/bin/bash
# Bench-only: Mac mount vs local disk for builds, small files, git status, full reads.
set -uo pipefail
S=$PWD/target/release/sandfs
OPS=$PWD/bench-mac/ops.py
W=${RUNNER_TEMP:-/tmp}/b; rm -rf "$W"; mkdir -p "$W"
TIMEFORMAT=%R
log() { echo "$*" | tee -a "$W/results.txt"; }
secs() { local s=$(python3 -c 'import time;print(time.time())'); "$@" >"$W/out" 2>&1; python3 -c "import time;print(round(time.time()-$s,3))"; }
stats() { cat "${SANDFS_NFS_STATS:-/nonexistent}" 2>/dev/null || true; }
diffstats() { # before-file after-file: proc count us per call
  python3 - "$1" "$2" <<'PY'
import sys
names={0:"NULL",1:"GETATTR",2:"SETATTR",3:"LOOKUP",4:"ACCESS",5:"READLINK",6:"READ",7:"WRITE",8:"CREATE",9:"MKDIR",10:"SYMLINK",12:"REMOVE",13:"RMDIR",14:"RENAME",15:"LINK",16:"READDIR",17:"READDIRPLUS",18:"FSSTAT",19:"FSINFO",20:"PATHCONF",21:"COMMIT"}
def rd(p):
    d={}
    try:
        for l in open(p):
            a,b,c=map(int,l.split()); d[a]=(b,c)
    except FileNotFoundError: pass
    return d
a,b=rd(sys.argv[1]),rd(sys.argv[2])
out=[]
for k,(n,us) in sorted(b.items()):
    n0,us0=a.get(k,(0,0)); dn=n-n0; du=us-us0
    if dn: out.append(f"{names.get(k,k)}={dn}({du/dn:.0f}us)")
print("  calls:", " ".join(out))
PY
}
phase() { # label, command...
  local label=$1; shift
  stats > "$W/s0"
  local t; t=$(secs "$@")
  stats > "$W/s1"
  log "$label: ${t}s $(tail -c 300 "$W/out" | tail -1) $( [ -n "${SANDFS_NFS_STATS:-}" ] && diffstats "$W/s0" "$W/s1")"
}

suite() { # dir label
  local D=$1 L=$2
  phase "$L smallfiles" python3 "$OPS" "$D/smallfiles" 1000
  phase "$L git status (first)" git -C "$D" status
  phase "$L git status" git -C "$D" status
  phase "$L git status" git -C "$D" status
  phase "$L cargo build clean" bash -c "cd '$D' && cargo build -q --offline"
  phase "$L cargo build no-op" bash -c "cd '$D' && cargo build -q --offline"
  phase "$L cargo build no-op" bash -c "cd '$D' && cargo build -q --offline"
  for i in 1 2; do
    echo "// $i" >> "$D/crates/core/main.rs"
    phase "$L cargo build incremental" bash -c "cd '$D' && cargo build -q --offline"
  done
}


git clone -q https://github.com/BurntSushi/ripgrep "$W/clone"
git -C "$W/clone" checkout -q 14.1.1
(cd "$W/clone" && cargo fetch -q)
# Some uncommitted work in the clone, as an agent's starting point would have.
echo "// local edit" >> "$W/clone/crates/core/main.rs"
unset SANDFS_NFS_STATS

log "== create one copy (x3)"
for i in 1 2 3; do
  log "worktree: $(secs git -C "$W/clone" worktree add -q -b wt$i "$W/wt$i")s"
  log "apfs cp -c: $(secs bash -c "cp -c -R '$W/clone' '$W/apfs$i' && git -C '$W/apfs$i' checkout -q -b a$i")s"
  export SANDFS_HOME="$W/home"
  log "sandfs fork: $(secs "$S" fork "$W/clone" "$W/fork$i" --branch f$i)s"
done
log "disk free after: $(df -k "$W" | tail -1 | awk '{print $4}') KiB"

suite "$W/wt1" "worktree"
suite "$W/apfs1" "apfs"
export SANDFS_NFS_STATS="$W/stats-fork"
# The daemon started before SANDFS_NFS_STATS was set, so start a fresh one for stats.
export SANDFS_HOME="$W/home2"
"$S" fork "$W/clone" "$W/forkS" --branch fS
suite "$W/forkS" "fork"
cat "$W/results.txt" >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
