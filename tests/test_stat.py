import time

import pytest

from sandfs import SandboxShell, VirtualFileSystem


@pytest.fixture
def vfs() -> VirtualFileSystem:
    return VirtualFileSystem()


@pytest.fixture
def shell(vfs: VirtualFileSystem) -> SandboxShell:
    return SandboxShell(vfs)


def test_stat_file(vfs: VirtualFileSystem, shell: SandboxShell) -> None:
    vfs.write_file("/test.txt", "hello world")

    result = shell.exec("stat /test.txt")
    assert result.exit_code == 0
    assert "File: /test.txt" in result.stdout
    assert "Size: 11" in result.stdout
    assert "Type: regular file" in result.stdout
    assert "Birth:" in result.stdout
    assert "Modify:" in result.stdout


def test_stat_directory(vfs: VirtualFileSystem, shell: SandboxShell) -> None:
    vfs.mkdir("/data")

    result = shell.exec("stat /data")
    assert result.exit_code == 0
    assert "File: /data" in result.stdout
    assert "Type: directory" in result.stdout


def test_timestamps_update_on_write(vfs: VirtualFileSystem) -> None:
    vfs.write_file("/test.txt", "initial")
    node = vfs.get_node("/test.txt")
    created_at = node.created_at
    modified_at = node.modified_at

    time.sleep(0.1)
    vfs.write_file("/test.txt", "updated")

    node = vfs.get_node("/test.txt")
    assert node.created_at == created_at
    assert node.modified_at > modified_at


def test_timestamps_update_on_touch(vfs: VirtualFileSystem) -> None:
    vfs.write_file("/test.txt", "initial")
    node = vfs.get_node("/test.txt")
    modified_at = node.modified_at

    time.sleep(0.1)
    vfs.touch("/test.txt")

    node = vfs.get_node("/test.txt")
    assert node.modified_at > modified_at
