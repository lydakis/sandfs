"""The Rust-backed Workspace through its Python bindings."""

import subprocess
import threading
from pathlib import Path

import pytest

from sandfs import EditFailed, VersionMismatch, Workspace


def git(repo: Path, *args: str) -> str:
    result = subprocess.run(
        [
            "git",
            "-C",
            str(repo),
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            *args,
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    return result.stdout


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    path = tmp_path / "repo"
    path.mkdir()
    git(path, "init", "-q", "-b", "main")
    (path / "src").mkdir()
    (path / "README.md").write_text("hello sandfs\n")
    (path / "src" / "lib.py").write_text("x = 1\n")
    git(path, "add", "-A")
    git(path, "commit", "-q", "-m", "first")
    git(path, "tag", "v1")
    (path / "README.md").write_text("hello again\n")
    git(path, "commit", "-q", "-am", "second")
    return path


def test_mount_git_reads_any_revision_without_a_checkout(repo: Path) -> None:
    ws = Workspace()
    ws.mount_git("/head", repo)
    ws.mount_git("/v1", str(repo), rev="v1")

    assert ws.read_text("/head/README.md") == "hello again\n"
    assert ws.read_text("/v1/README.md") == "hello sandfs\n"
    assert [(e.name, e.kind) for e in ws.list("/head")] == [("README.md", "file"), ("src", "dir")]


def test_unknown_revisions_raise_os_error(repo: Path) -> None:
    with pytest.raises(OSError, match="source error"):
        Workspace().mount_git("/repo", repo, rev="no-such-branch")


def test_fork_is_isolated_and_diff_lists_changes(repo: Path) -> None:
    base = Workspace()
    base.mount_git("/repo", repo)

    attempt = base.fork()
    attempt.edit("/repo/README.md", "again", "fork")
    attempt.write("/repo/NEW.md", b"new\n")
    attempt.remove("/repo/src/lib.py")

    assert base.read_text("/repo/README.md") == "hello again\n"
    assert base.diff() == []
    assert [(c.path, c.kind) for c in attempt.diff()] == [
        ("/repo/NEW.md", "added"),
        ("/repo/README.md", "modified"),
        ("/repo/src/lib.py", "deleted"),
    ]


def test_mount_dir_never_writes_to_disk(tmp_path: Path) -> None:
    (tmp_path / "notes.txt").write_text("on disk\n")
    ws = Workspace()
    ws.mount_dir("/host", tmp_path)

    ws.write("/host/notes.txt", "in the overlay\n")

    assert ws.read_text("/host/notes.txt") == "in the overlay\n"
    assert (tmp_path / "notes.txt").read_text() == "on disk\n"


def test_mount_files_accepts_text_and_bytes() -> None:
    ws = Workspace()
    ws.mount_files("/data", {"a.txt": "alpha", "nested/b.bin": b"\x00\xff"})

    assert ws.read_text("/data/a.txt") == "alpha"
    assert ws.read("/data/nested/b.bin") == b"\x00\xff"
    assert ws.stat("/data/nested/b.bin").size == 2
    with pytest.raises(ValueError, match="not valid UTF-8"):
        ws.read_text("/data/nested/b.bin")


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


def test_expected_version_guards_writes_and_edits() -> None:
    ws = Workspace()
    first = ws.write("/notes.md", "draft", expected_version=0)
    second = ws.edit("/notes.md", "draft", "final", expected_version=first)
    assert ws.stat("/notes.md").version == second > first

    with pytest.raises(VersionMismatch) as caught:
        ws.write("/notes.md", "stale", expected_version=first)
    assert (caught.value.path, caught.value.expected, caught.value.actual) == (
        "/notes.md",
        first,
        second,
    )
    with pytest.raises(VersionMismatch):
        ws.edit("/notes.md", "final", "stale", expected_version=first)
    assert ws.read_text("/notes.md") == "final"


def test_edit_requires_a_unique_match_unless_replace_all() -> None:
    ws = Workspace()
    ws.write("/a.txt", "one one two")

    with pytest.raises(EditFailed):
        ws.edit("/a.txt", "one", "1")
    with pytest.raises(EditFailed):
        ws.edit("/a.txt", "three", "3")
    ws.edit("/a.txt", "one", "1", replace_all=True)

    assert ws.read_text("/a.txt") == "1 1 two"


def test_copy_move_symlink_and_mkdir() -> None:
    ws = Workspace()
    ws.mount_files("/base", {"src/a.txt": "a", "src/b.txt": "b"})

    ws.copy("/base/src", "/work", recursive=True)
    ws.move("/work/a.txt", "/work/renamed.txt")
    ws.symlink("/link", "/work/b.txt")
    ws.mkdir("/deep/er", parents=True)

    assert [e.name for e in ws.list("/work")] == ["b.txt", "renamed.txt"]
    assert ws.read_link("/link") == "/work/b.txt"
    assert ws.read_text("/link") == "b"
    assert ws.stat("/link").kind == "symlink"
    assert ws.exists("/deep/er")
    with pytest.raises(IsADirectoryError):
        ws.copy("/base/src", "/other")


def test_reads_release_the_gil(repo: Path) -> None:
    ws = Workspace()
    ws.mount_git("/repo", repo)
    results: list[str] = []

    threads = [
        threading.Thread(target=lambda: results.append(ws.read_text("/repo/README.md")))
        for _ in range(4)
    ]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    assert results == ["hello again\n"] * 4
