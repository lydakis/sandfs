import pytest

from sandfs import VirtualFileSystem, SandboxShell

@pytest.fixture
def shell() -> SandboxShell:
    vfs = VirtualFileSystem()
    content = "\n".join([f"line {i}" for i in range(1, 21)])
    vfs.write_file("/lines.txt", content)
    return SandboxShell(vfs)

def test_head_default(shell):
    result = shell.exec("head /lines.txt")
    assert result.exit_code == 0
    lines = result.stdout.splitlines()
    assert len(lines) == 10
    assert lines[0] == "line 1"
    assert lines[-1] == "line 10"

def test_head_lines(shell):
    result = shell.exec("head -n 5 /lines.txt")
    assert result.exit_code == 0
    lines = result.stdout.splitlines()
    assert len(lines) == 5
    assert lines[-1] == "line 5"

def test_head_bytes(shell):
    result = shell.exec("head -c 5 /lines.txt")
    assert result.exit_code == 0
    assert result.stdout == "line "

def test_tail_default(shell):
    result = shell.exec("tail /lines.txt")
    assert result.exit_code == 0
    lines = result.stdout.splitlines()
    assert len(lines) == 10
    assert lines[0] == "line 11"
    assert lines[-1] == "line 20"

def test_tail_lines(shell):
    result = shell.exec("tail -n 5 /lines.txt")
    assert result.exit_code == 0
    lines = result.stdout.splitlines()
    assert len(lines) == 5
    assert lines[0] == "line 16"
    assert lines[-1] == "line 20"

def test_tail_bytes(shell):
    result = shell.exec("tail -c 2 /lines.txt")
    assert result.exit_code == 0
    assert result.stdout == "20"  # last line is "line 20"

def test_multiple_files(shell):
    shell.vfs.write_file("/other.txt", "other content")
    result = shell.exec("head -n 1 /lines.txt /other.txt")
    assert "==> /lines.txt <==" in result.stdout
    assert "line 1" in result.stdout
    assert "==> /other.txt <==" in result.stdout
    assert "other content" in result.stdout

def test_tail_preserves_whitespace(shell):
    shell.vfs.write_file("/whitespace.txt", "\nfoo\n")
    result = shell.exec("tail /whitespace.txt")
    assert result.exit_code == 0
    assert result.stdout == "\nfoo\n"
