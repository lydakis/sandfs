"""The Rust-backed Workspace through its Python bindings."""

import json
import os
import subprocess
import threading
from collections.abc import Iterable
from pathlib import Path

import pytest

from sandfs import (
    DirEntry,
    DirSource,
    GitSource,
    MemorySource,
    Source,
    VersionMismatch,
    Workspace,
)


class Tickets:
    """A source over an in-process "API", counting how often it is called."""

    def __init__(self, tickets: dict[str, dict[str, str]]) -> None:
        self.tickets = tickets
        self.calls: list[tuple[str, str]] = []

    def list(self, path: str) -> Iterable[DirEntry]:
        self.calls.append(("list", path))
        if path == "/":
            return [DirEntry("open", "dir"), DirEntry("README.txt", size=6)]
        return [DirEntry(f"{ticket}.json") for ticket in self.tickets]

    def read(self, path: str) -> bytes | str:
        self.calls.append(("read", path))
        if path == "/README.txt":
            return "hello\n"
        ticket = path.removeprefix("/open/").removesuffix(".json")
        return json.dumps(self.tickets[ticket]).encode()


def test_python_sources_are_read_lazily_and_cached_across_forks() -> None:
    api = Tickets({"T-1": {"title": "Broken export"}, "T-2": {"title": "Slow search"}})
    source: Source = api
    base = Workspace()
    base.mount("/tickets", source)
    assert api.calls == []

    assert [(e.name, e.kind, e.size) for e in base.list("/tickets/open")] == [
        ("T-1.json", "file", None),
        ("T-2.json", "file", None),
    ]
    assert json.loads(base.read("/tickets/open/T-1.json")) == {"title": "Broken export"}
    fork = base.fork()
    assert fork.read_text("/tickets/README.txt") == "hello\n"
    fork.read("/tickets/open/T-1.json")

    assert api.calls == [
        ("list", "/"),
        ("list", "/open"),
        ("read", "/open/T-1.json"),
        ("read", "/README.txt"),
    ]


def test_stat_reads_a_file_whose_size_the_source_left_out() -> None:
    ws = Workspace()
    ws.mount("/tickets", Tickets({"T-1": {"title": "x"}}))
    assert ws.stat("/tickets/open/T-1.json").size == len(json.dumps({"title": "x"}))
    assert ws.stat("/tickets/open").size == 0


def test_source_failures_raise_os_error() -> None:
    class Broken:
        def list(self, path: str) -> Iterable[DirEntry]:
            if path == "/bad":
                return [DirEntry("../escape")]
            return [DirEntry("bad", "dir"), DirEntry("file")]

        def read(self, path: str) -> bytes:
            raise PermissionError("token expired")

    ws = Workspace()
    ws.mount("/broken", Broken())
    with pytest.raises(OSError, match="PermissionError: token expired"):
        ws.read("/broken/file")
    with pytest.raises(OSError, match="invalid entry name"):
        ws.list("/broken/bad")
    with pytest.raises(TypeError, match="list"):
        ws.mount("/nope", object())  # type: ignore[arg-type]
    with pytest.raises(ValueError, match="kind"):
        DirEntry("x", "symlink")  # type: ignore[arg-type]


def test_python_sources_work_from_several_threads() -> None:
    ws = Workspace()
    ws.mount("/tickets", Tickets({"T-1": {"title": "x"}}))
    forks = [ws.fork() for _ in range(4)]
    results: list[bytes] = []

    threads = [
        threading.Thread(
            target=lambda fork=fork: results.append(fork.read("/tickets/open/T-1.json"))
        )
        for fork in forks
    ]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    assert results == [json.dumps({"title": "x"}).encode()] * 4


def test_dir_source_never_writes_to_disk_and_skips_symlinks(tmp_path: Path) -> None:
    (tmp_path / "sales.csv").write_text("region,total\neu,10\n")
    os.symlink("/etc/passwd", tmp_path / "link")
    ws = Workspace()
    ws.mount("/data", DirSource(tmp_path))

    ws.write("/data/sales.csv", "region,total\neu,12\n")

    assert ws.read_text("/data/sales.csv") == "region,total\neu,12\n"
    assert (tmp_path / "sales.csv").read_text() == "region,total\neu,10\n"
    assert [e.name for e in ws.list("/data")] == ["sales.csv"]


def test_memory_source_and_binary_files() -> None:
    ws = Workspace()
    ws.mount("/brief", MemorySource({"task.md": "Summarise\n", "logo.png": b"\x89PNG\x00\xff"}))
    ws.write("/out/chart.png", b"\x89PNG\r\n\x1a\n")

    assert ws.read("/brief/logo.png") == b"\x89PNG\x00\xff"
    assert ws.stat("/brief/logo.png").size == 6
    assert ws.read("/out/chart.png") == b"\x89PNG\r\n\x1a\n"
    with pytest.raises(ValueError, match="not valid UTF-8"):
        ws.read_text("/out/chart.png")


def test_fork_is_isolated_and_changes_lists_changed_files() -> None:
    base = Workspace()
    base.mount("/docs", MemorySource({"plan.md": "draft\n", "old/notes.txt": "n\n"}))

    attempt = base.fork()
    attempt.write("/docs/plan.md", "final\n")
    attempt.write("/docs/figure.png", b"\x00\x01")
    attempt.remove("/docs/old", recursive=True)

    assert base.read_text("/docs/plan.md") == "draft\n"
    assert base.changes() == []
    assert [(c.path, c.kind) for c in attempt.changes()] == [
        ("/docs/figure.png", "added"),
        ("/docs/old/notes.txt", "deleted"),
        ("/docs/plan.md", "modified"),
    ]


def test_git_source_reads_any_revision_without_a_checkout(tmp_path: Path) -> None:
    def git(*args: str) -> None:
        subprocess.run(
            [
                "git",
                "-C",
                str(tmp_path),
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                *args,
            ],
            check=True,
            capture_output=True,
        )

    git("init", "-q")
    (tmp_path / "report.txt").write_text("v1\n")
    git("add", "-A")
    git("commit", "-q", "-m", "first")
    git("tag", "v1")
    (tmp_path / "report.txt").write_text("v2\n")
    git("commit", "-q", "-am", "second")

    ws = Workspace()
    ws.mount("/head", GitSource(tmp_path))
    ws.mount("/v1", GitSource(str(tmp_path), rev="v1"))
    assert ws.read_text("/head/report.txt") == "v2\n"
    assert ws.read_text("/v1/report.txt") == "v1\n"
    with pytest.raises(OSError, match="source error"):
        GitSource(tmp_path, rev="no-such-branch")


def test_errors_map_to_builtin_exceptions() -> None:
    ws = Workspace()
    ws.write("/dir/file.txt", "x")

    with pytest.raises(FileNotFoundError):
        ws.read("/missing")
    with pytest.raises(FileExistsError):
        ws.mkdir("/dir")
    with pytest.raises(NotADirectoryError):
        ws.list("/dir/file.txt")
    with pytest.raises(IsADirectoryError):
        ws.read("/dir")
    with pytest.raises(OSError, match="not empty"):
        ws.remove("/dir")
    with pytest.raises(ValueError, match="invalid path"):
        ws.read("relative/path")


def test_expected_version_guards_writes() -> None:
    ws = Workspace()
    first = ws.write("/notes.md", "draft", expected_version=0)
    second = ws.write("/notes.md", "final", expected_version=first)
    assert ws.stat("/notes.md").version == second > first

    with pytest.raises(VersionMismatch) as caught:
        ws.write("/notes.md", "stale", expected_version=first)
    assert (caught.value.path, caught.value.expected, caught.value.actual) == (
        "/notes.md",
        first,
        second,
    )
    assert ws.read_text("/notes.md") == "final"


def test_copy_move_and_mkdir() -> None:
    ws = Workspace()
    ws.mount("/base", MemorySource({"photos/a.jpg": b"a", "photos/b.jpg": b"b"}))

    ws.copy("/base/photos", "/work", recursive=True)
    ws.move("/work/a.jpg", "/work/renamed.jpg")
    ws.mkdir("/deep/er", parents=True)

    assert [e.name for e in ws.list("/work")] == ["b.jpg", "renamed.jpg"]
    assert ws.exists("/deep/er")
    with pytest.raises(IsADirectoryError):
        ws.copy("/base/photos", "/other")
    with pytest.raises(FileExistsError):
        ws.move("/work/b.jpg", "/work/renamed.jpg")
