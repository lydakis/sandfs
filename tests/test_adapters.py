import pytest

from sandfs import NodePolicy, VirtualFileSystem
from sandfs.adapters import MemoryStorageAdapter
from sandfs.exceptions import InvalidOperation


def test_storage_adapter_mount_and_persist():
    adapter = MemoryStorageAdapter(
        initial={
            "a.txt": "hello",
            "dir/b.txt": "nested",
        }
    )
    vfs = VirtualFileSystem()
    vfs.mount_storage("/data", adapter, policy=NodePolicy(writable=True))

    assert vfs.read_file("/data/a.txt") == "hello"
    vfs.write_file("/data/a.txt", "world")
    vfs.write_file("/data/dir/new.txt", "fresh")

    assert adapter.read("a.txt").content == "world"
    assert adapter.read("dir/new.txt").content == "fresh"


def test_storage_adapter_conflict_detection():
    adapter = MemoryStorageAdapter(initial={"a.txt": "hello"})
    vfs = VirtualFileSystem()
    vfs.mount_storage("/logs", adapter)

    vfs.write_file("/logs/a.txt", "one")
    current = adapter.read("a.txt")
    adapter.write("a.txt", "external", version=current.version)

    with pytest.raises(InvalidOperation):
        vfs.write_file("/logs/a.txt", "two", expected_version=1)


def test_storage_adapter_sync_refreshes_vfs():
    adapter = MemoryStorageAdapter(initial={"a.txt": "hello"})
    vfs = VirtualFileSystem()
    vfs.mount_storage("/sync", adapter)

    current = adapter.read("a.txt")
    adapter.write("a.txt", "external", version=current.version)
    vfs.sync_storage("/sync")
    assert vfs.read_file("/sync/a.txt") == "external"
