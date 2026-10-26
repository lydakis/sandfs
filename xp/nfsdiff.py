"""Diff two `nfsstat -c` dumps: prints name=delta for every counter that moved."""
import re, sys
def parse(path):
    out, section, lines = {}, "", open(path).read().splitlines()
    for i, line in enumerate(lines):
        if line.strip().endswith(":") and not re.search(r"\d", line):
            section = line.strip().rstrip(":")
            continue
        names = line.split()
        if i + 1 < len(lines) and names and all(re.match(r"^[A-Za-z][\w/-]*$", n) for n in names):
            values = lines[i + 1].split()
            if len(values) == len(names) and all(re.match(r"^\d+$", v) for v in values):
                for n, v in zip(names, values):
                    out[f"{section}:{n}"] = int(v)
    return out
a, b = parse(sys.argv[1]), parse(sys.argv[2])
moved = [(k, b[k] - a.get(k, 0)) for k in b if b[k] - a.get(k, 0)]
skip = ("Cache Info", "Rpc Info", "RPC Info")
print(" ".join(f"{k.split(':')[-1]}={v}" for k, v in moved if not k.startswith(skip) and "Hits" not in k and "Misses" not in k))
