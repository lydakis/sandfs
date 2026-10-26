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

git clone -q https://github.com/BurntSushi/ripgrep "$W/rg-src"
git -C "$W/rg-src" checkout -q 14.1.1

suite() { # dir label
  local D=$1 L=$2
  python3 "$OPS" "$D/smallfiles" 1000 | sed "s/^/$L smallfiles: /" | tee -a "$W/results.txt"
  phase "$L git status (first)" git -C "$D" status
  phase "$L git status" git -C "$D" status
}

git clone -q "$W/rg-src" "$W/local"
unset SANDFS_NFS_STATS
suite "$W/local" "local"

variant() { # name, DROP, ADD
  export SANDFS_HOME="$W/home-$1" SANDFS_NFS_DROP="$2" SANDFS_NFS_ADD="$3" SANDFS_NFS_STATS="$W/stats-$1"
  mkdir -p "$W/m-$1"
  if ! "$S" fork "$W/local" "$W/m-$1" --branch "b-$1"; then log "$1: fork failed"; return; fi
  log "== $1: $(mount | grep "m-$1" | sed 's/.*(//')"
  export DAEMON_PID=$(pgrep -x sandfs | head -1)
  suite "$W/m-$1" "$1"
  unset DAEMON_PID
  phase "$1 unmount" "$S" unmount "$W/m-$1"
  phase "$1 rm" "$S" rm "m-$1"
}
variant base "" ""

# The floor: Apple's own NFS server, exporting a local folder to this machine.
mkdir -p "$HOME/exp"
echo "$HOME/exp -alldirs -maproot=root 127.0.0.1" | sudo tee /etc/exports
sudo nfsd enable; sudo nfsd restart || sudo nfsd start
sleep 5
showmount -e 127.0.0.1 || true
mkdir -p "$W/apple"
if sudo mount -t nfs -o vers=3,tcp,locallocks,soft,intr,sync,nobrowse 127.0.0.1:"$HOME/exp" "$W/apple"; then
  sudo chmod 777 "$W/apple"
  export DAEMON_PID=$(pgrep -x nfsd | head -1)
  log "== apple nfsd: $(mount | grep "$W/apple")"
  python3 "$OPS" "$W/apple/smallfiles" 1000 | sed "s/^/apple nfsd smallfiles: /" | tee -a "$W/results.txt"
  unset DAEMON_PID
  sudo umount "$W/apple"
else
  log "apple nfsd: mount failed"
fi
log "done"
cat "$W/results.txt" >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
exit 0
# First full read: golang/go
unset SANDFS_NFS_DROP SANDFS_NFS_ADD
log "== full read"
log "shallow clone go: $(secs git clone -q --depth 1 https://github.com/golang/go "$W/go-clone")s"
log "read all, clone: $(secs bash -c "cd '$W/go-clone' && tar cf - --exclude .git . | wc -c")s"
export SANDFS_HOME="$W/home-go" SANDFS_NFS_STATS="$W/stats-go"
mkdir -p "$W/m-go"
log "mount git: local go: $(secs "$S" mount "git:$W/go-clone" "$W/m-go")s"
phase "read all, mount of local go" bash -c "cd '$W/m-go' && tar cf - . | wc -c"
"$S" unmount "$W/m-go"
export SANDFS_HOME="$W/home-go2" SANDFS_NFS_STATS="$W/stats-go2"
mkdir -p "$W/m-go2"
log "mount remote go: $(secs "$S" mount https://github.com/golang/go "$W/m-go2")s"
phase "read all, mount of remote go" bash -c "cd '$W/m-go2' && tar cf - . | wc -c"
phase "read all again, mount of remote go" bash -c "cd '$W/m-go2' && tar cf - . | wc -c"
"$S" unmount "$W/m-go2"
cat "$W/results.txt" >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
