import pytest

from sandfs import NodePolicy, VirtualFileSystem
from sandfs.adapters import MemoryStorageAdapter
from sandfs.exceptions import InvalidOperation


def test_storage_adapter_delete_propagates_and_emits_event():
    adapter = MemoryStorageAdapter()
    vfs = VirtualFileSystem()
    events = []
    vfs.register_path_hook("/records", events.append)

    vfs.mount_storage("/records", adapter, policy=NodePolicy(writable=True))
    vfs.write_file("/records/report.txt", "contents")

    entries = adapter.list()
    assert set(entries.keys()) == {"report.txt"}

    vfs.remove("/records/report.txt")

    assert adapter.list() == {}
    assert any(event.event == "delete" and event.path == "/records/report.txt" for event in events)


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


def test_storage_adapter_conflict_preserves_cached_state():
    adapter = MemoryStorageAdapter(initial={"conflict.txt": "initial"})
    vfs = VirtualFileSystem()
    vfs.mount_storage("/data", adapter)

    vfs.write_file("/data/conflict.txt", "local-one")
    original_content = vfs.read_file("/data/conflict.txt")
    original_version = vfs.get_version("/data/conflict.txt")

    current = adapter.read("conflict.txt")
    adapter.write("conflict.txt", "external", version=current.version)

    with pytest.raises(InvalidOperation):
        vfs.write_file(
            "/data/conflict.txt",
            "local-two",
            expected_version=original_version,
        )

    assert vfs.read_file("/data/conflict.txt") == original_content
    assert vfs.get_version("/data/conflict.txt") == original_version


def test_storage_adapter_sync_refreshes_vfs():
    adapter = MemoryStorageAdapter(initial={"a.txt": "hello"})
    vfs = VirtualFileSystem()
    vfs.mount_storage("/sync", adapter)

    current = adapter.read("a.txt")
    adapter.write("a.txt", "external", version=current.version)
    vfs.sync_storage("/sync")
    assert vfs.read_file("/sync/a.txt") == "external"
