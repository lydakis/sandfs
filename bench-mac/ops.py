"""Small-file ops timing: create, rewrite, delete, atomic saves of N files in DIR.
With SANDFS_NFS_STATS set, prints the NFS calls each phase made."""
import os, sys, time, json
d, n = sys.argv[1], int(sys.argv[2])
os.makedirs(d, exist_ok=True)
stats = os.environ.get("SANDFS_NFS_STATS")
names = {0:"NULL",1:"GETATTR",2:"SETATTR",3:"LOOKUP",4:"ACCESS",5:"READLINK",6:"READ",7:"WRITE",8:"CREATE",9:"MKDIR",12:"REMOVE",13:"RMDIR",14:"RENAME",16:"READDIR",17:"READDIRPLUS",18:"FSSTAT",21:"COMMIT"}
def rd():
    out = {}
    if stats:
        time.sleep(0.3)
        try:
            for l in open(stats):
                a, b, c = map(int, l.split()); out[a] = (b, c)
        except FileNotFoundError: pass
    return out
pid = os.environ.get("DAEMON_PID")
def cpu():
    if not pid: return 0.0
    import subprocess
    out = subprocess.run(["ps", "-o", "cputime=", "-p", pid], capture_output=True, text=True).stdout.strip()
    if not out: return 0.0
    parts = out.replace("-", ":").split(":")
    secs = 0.0
    for part in parts: secs = secs * 60 + float(part)
    return secs
res = {}
calls = {}
def t(name, f):
    before = rd(); c0 = cpu()
    s = time.perf_counter(); f(); res[name] = round(time.perf_counter() - s, 3)
    c1 = cpu(); after = rd()
    parts = [f"daemon-cpu={c1 - c0:.2f}s"] if pid else []
    for k, (c, us) in sorted(after.items()):
        c0, us0 = before.get(k, (0, 0))
        if c - c0: parts.append(f"{names.get(k, k)}={c - c0}({(us - us0) / (c - c0):.0f}us)")
    if parts: calls[name] = " ".join(parts)
def create():
    for i in range(n):
        with open(f"{d}/f{i}.txt", "w") as fh: fh.write("hello %d\n" % i)
def rewrite():
    for i in range(n):
        with open(f"{d}/f{i}.txt", "w") as fh: fh.write("again %d\n" % i)
def atomic():
    for i in range(n):
        with open(f"{d}/.f{i}.tmp", "w") as fh: fh.write("saved %d\n" % i)
        os.replace(f"{d}/.f{i}.tmp", f"{d}/f{i}.txt")
def stat():
    for i in range(n): os.stat(f"{d}/f{i}.txt")
def delete():
    for i in range(n): os.unlink(f"{d}/f{i}.txt")
for name, f in [("create", create), ("rewrite", rewrite), ("atomic", atomic), ("stat", stat), ("delete", delete)]:
    t(name, f)
print(json.dumps(res))
for name, c in calls.items(): print(f"   {name}: {c}")
