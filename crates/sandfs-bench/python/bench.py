"""Time sandfs through its Python bindings against plain file access from Python.

Three targets serve the same generated tree of small files: ``pathlib`` on the host
directory, ``sandfs`` with a ``DirSource``, and ``sandfs`` with a source written in
Python (``PySource`` below), which shows what a harness-defined source costs per call.

Run it from the repository root after ``uv pip install -e .``::

    uv run python crates/sandfs-bench/python/bench.py [--files 10000] [--scratch /dev/shm]
"""

from __future__ import annotations

import argparse
import os
import random
import shutil
import statistics
import tempfile
import time
from collections.abc import Callable, Iterable
from pathlib import Path
from typing import Protocol

from sandfs import DirEntry, DirSource, Workspace


class PySource:
    """A host directory served by Python code, as a harness-defined source would be."""

    def __init__(self, root: Path) -> None:
        self.root = root

    def list(self, path: str) -> Iterable[DirEntry]:
        out = []
        with os.scandir(self.root / path.lstrip("/")) as entries:
            for entry in entries:
                if entry.is_dir():
                    out.append(DirEntry(entry.name, "dir"))
                else:
                    out.append(DirEntry(entry.name, size=entry.stat().st_size))
        return out

    def read(self, path: str) -> bytes:
        return (self.root / path.lstrip("/")).read_bytes()


class View(Protocol):
    def read(self, path: str) -> int: ...
    def walk(self, path: str) -> int: ...
    def write(self, path: str, data: bytes) -> None: ...
    def fork(self) -> View: ...
    def changes(self) -> int: ...
    def close(self) -> None: ...


class HostView:
    """The tree on the host. Forking copies it; changes compare it with the original."""

    def __init__(self, root: Path, original: Path) -> None:
        self.root = root
        self.original = original

    def read(self, path: str) -> int:
        return len((self.root / path).read_bytes())

    def walk(self, path: str) -> int:
        return sum(len(files) for _, _, files in os.walk(self.root / path))

    def write(self, path: str, data: bytes) -> None:
        (self.root / path).write_bytes(data)

    def fork(self) -> View:
        copy = Path(tempfile.mkdtemp(dir=self.root.parent))
        shutil.copytree(self.root, copy, dirs_exist_ok=True)
        return HostView(copy, self.original)

    def changes(self) -> int:
        def files(root: Path) -> set[str]:
            return {
                os.path.relpath(os.path.join(d, f), root)
                for d, _, names in os.walk(root)
                for f in names
            }

        before, after = files(self.original), files(self.root)
        changed = len(before ^ after)
        for rel in before & after:
            a, b = self.original / rel, self.root / rel
            if a.stat().st_size != b.stat().st_size or a.read_bytes() != b.read_bytes():
                changed += 1
        return changed

    def close(self) -> None:
        if self.root != self.original:
            shutil.rmtree(self.root)


class SandfsView:
    def __init__(self, ws: Workspace) -> None:
        self.ws = ws

    def read(self, path: str) -> int:
        return len(self.ws.read(f"/data/{path}"))

    def walk(self, path: str) -> int:
        count = 0
        for entry in self.ws.list(f"/data/{path}"):
            if entry.kind == "dir":
                count += self.walk(f"{path}/{entry.name}")
            else:
                count += 1
        return count

    def write(self, path: str, data: bytes) -> None:
        self.ws.write(f"/data/{path}", data)

    def fork(self) -> View:
        return SandfsView(self.ws.fork())

    def changes(self) -> int:
        return len(self.ws.changes())

    def close(self) -> None:
        pass


def generate(root: Path, files: int) -> list[str]:
    rng = random.Random(7)
    per_dir = 100
    paths = []
    for i in range(files):
        rel = f"d{i // per_dir:03}/f{i % per_dir:03}.bin"
        (root / rel).parent.mkdir(parents=True, exist_ok=True)
        (root / rel).write_bytes(rng.randbytes(rng.randint(1024, 16 * 1024)))
        paths.append(rel)
    return paths


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--files", type=int, default=10_000)
    parser.add_argument("--writes", type=int, default=1000)
    parser.add_argument("--iters", type=int, default=5)
    parser.add_argument("--scratch", help="where to generate the files (default: system temp)")
    args = parser.parse_args()

    with tempfile.TemporaryDirectory(prefix="sandfs-pybench-", dir=args.scratch) as tmp:
        scratch = Path(tmp)
        data = scratch / "data"
        paths = generate(data, args.files)
        payload = random.Random(1).randbytes(4096)

        def host(writable: bool) -> View:
            if not writable:
                return HostView(data, data)
            copy = Path(tempfile.mkdtemp(dir=scratch))
            shutil.copytree(data, copy, dirs_exist_ok=True)
            return HostView(copy, data)

        def sandfs_dir(writable: bool) -> View:
            ws = Workspace()
            ws.mount("/data", DirSource(data))
            return SandfsView(ws)

        def sandfs_py(writable: bool) -> View:
            ws = Workspace()
            ws.mount("/data", PySource(data))
            return SandfsView(ws)

        targets: dict[str, Callable[[bool], View]] = {
            "pathlib": host,
            "sandfs": sandfs_dir,
            "sandfs + Python source": sandfs_py,
        }

        def read_all(view: View) -> None:
            for path in paths:
                view.read(path)

        def create(view: View) -> None:
            for i in range(args.writes):
                view.write(f"d{i % (args.files // 100 or 1):03}/new{i:05}.bin", payload)

        def nothing(view: View) -> None:
            pass

        n, w = args.files, args.writes
        workloads: list[tuple[str, bool, Callable[[View], object], Callable[[View], object]]] = [
            (f"read {n} files, fresh view", False, nothing, read_all),
            (f"read the {n} files again", False, read_all, read_all),
            ("list every directory, fresh view", False, nothing, lambda v: v.walk(".")),
            (f"create {w} 4 KiB files", True, nothing, create),
            ("fork", True, nothing, lambda v: v.fork()),
            (f"changes after {w} creates", True, create, lambda v: v.changes()),
        ]

        print(f"{n} files, median of {args.iters} runs.\n")
        print("| workload | " + " | ".join(targets) + " |")
        print("|---|" + "---:|" * len(targets))
        for name, writable, prepare, run in workloads:
            cells = []
            for open_view in targets.values():
                times = []
                for _ in range(args.iters):
                    view = open_view(writable)
                    prepare(view)
                    start = time.perf_counter()
                    result = run(view)
                    times.append(time.perf_counter() - start)
                    if isinstance(result, HostView | SandfsView):
                        result.close()
                    view.close()
                cells.append(fmt(statistics.median(times)))
            print(f"| {name} | " + " | ".join(cells) + " |")


def fmt(seconds: float) -> str:
    if seconds < 1e-3:
        return f"{seconds * 1e6:.0f} µs"
    if seconds < 1:
        return f"{seconds * 1e3:.1f} ms"
    return f"{seconds:.2f} s"


if __name__ == "__main__":
    main()
