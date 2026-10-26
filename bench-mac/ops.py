"""Small-file ops timing: create, rewrite, delete, atomic saves of N files in DIR."""
import os, sys, time, json
d, n = sys.argv[1], int(sys.argv[2])
os.makedirs(d, exist_ok=True)
res = {}
def t(name, f):
    s = time.perf_counter(); c0 = time.process_time(); f(); res[name] = round(time.perf_counter() - s, 3)
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
