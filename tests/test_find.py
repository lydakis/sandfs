import pytest

from sandfs import SandboxShell, VirtualFileSystem
from sandfs.policies import NodePolicy, VisibilityView


@pytest.fixture
def shell() -> SandboxShell:
    vfs = VirtualFileSystem()
    vfs.mkdir("/a")
    vfs.mkdir("/a/b")
    vfs.write_file("/a/file1.txt", "content")
    vfs.write_file("/a/b/file2.py", "print('hello')")
    vfs.write_file("/root_file.md", "# Root")
    return SandboxShell(vfs)


def test_find_all(shell):
    result = shell.exec("find /")
    assert result.exit_code == 0
    lines = result.stdout.splitlines()
    assert "/" in lines
    assert "/a" in lines
    assert "/a/b" in lines
    assert "/a/file1.txt" in lines
    assert "/a/b/file2.py" in lines
    assert "/root_file.md" in lines


def test_find_name(shell):
    result = shell.exec("find / -name *.py")
    assert result.exit_code == 0
    lines = result.stdout.splitlines()
    assert len(lines) == 1
    assert lines[0] == "/a/b/file2.py"

    result = shell.exec("find / -name file*")
    assert result.exit_code == 0
    lines = sorted(result.stdout.splitlines())
    assert len(lines) == 2
    assert lines[0] == "/a/b/file2.py"
    assert lines[1] == "/a/file1.txt"


def test_find_type_file(shell):
    result = shell.exec("find / -type f")
    assert result.exit_code == 0
    lines = sorted(result.stdout.splitlines())
    assert len(lines) == 3
    assert "/a/b/file2.py" in lines
    assert "/a/file1.txt" in lines
    assert "/root_file.md" in lines
    assert "/a" not in lines


def test_find_type_dir(shell):
    result = shell.exec("find / -type d")
    assert result.exit_code == 0
    lines = sorted(result.stdout.splitlines())
    assert "/" in lines
    assert "/a" in lines
    assert "/a/b" in lines
    assert "/a/file1.txt" not in lines


def test_find_subdir(shell):
    result = shell.exec("find /a/b")
    assert result.exit_code == 0
    lines = result.stdout.splitlines()
    assert "/a/b" in lines
    assert "/a/b/file2.py" in lines
    assert "/a/file1.txt" not in lines


def test_find_missing_path(shell):
    result = shell.exec("find /nonexistent")
    assert result.exit_code == 1
    assert "No such file or directory" in result.stderr


def test_find_combined(shell):
    result = shell.exec("find / -type f -name *.txt")
    assert result.exit_code == 0
    lines = result.stdout.splitlines()
    assert len(lines) == 1
    assert lines[0] == "/a/file1.txt"


def test_find_respects_hidden_directories():
    vfs = VirtualFileSystem()
    vfs.mkdir("/hidden")
    vfs.write_file("/hidden/visible.txt", "data")
    vfs.set_policy("/hidden", NodePolicy(classification="secret"))
    shell = SandboxShell(vfs, view=VisibilityView(classifications={"public"}))

    result = shell.exec("find /")
    lines = result.stdout.splitlines()
    assert "/" in lines
    assert "/hidden" not in lines
    assert "/hidden/visible.txt" not in lines
