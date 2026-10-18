import pytest

from sandfs import NodePolicy, VirtualFileSystem
from sandfs.adapters import FileSystemAdapter, MemoryStorageAdapter
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


def test_storage_adapter_sync_refreshes_vfs():
    adapter = MemoryStorageAdapter(initial={"a.txt": "hello"})
    vfs = VirtualFileSystem()
    vfs.mount_storage("/sync", adapter)

    current = adapter.read("a.txt")
    adapter.write("a.txt", "external", version=current.version)
    vfs.sync_storage("/sync")
    assert vfs.read_file("/sync/a.txt") == "external"


def test_filesystem_adapter_read_write_list(tmp_path):
    root = tmp_path / "fs"
    root.mkdir()
    (root / "a.txt").write_text("hello")
    adapter = FileSystemAdapter(root)
    vfs = VirtualFileSystem()
    vfs.mount_storage("/data", adapter, policy=NodePolicy(writable=True))

    assert vfs.read_file("/data/a.txt") == "hello"
    vfs.write_file("/data/a.txt", "world")
    assert (root / "a.txt").read_text() == "world"
    listing = adapter.list()
    assert "a.txt" in listing


def test_filesystem_adapter_sync_refreshes_vfs(tmp_path):
    root = tmp_path / "fs"
    root.mkdir()
    (root / "a.txt").write_text("hello")
    adapter = FileSystemAdapter(root)
    vfs = VirtualFileSystem()
    vfs.mount_storage("/data", adapter, policy=NodePolicy(writable=True))

    (root / "a.txt").write_text("external")
    vfs.sync_storage("/data")
    assert vfs.read_file("/data/a.txt") == "external"


def test_filesystem_adapter_blocks_path_escape(tmp_path):
    root = tmp_path / "fs"
    root.mkdir()
    adapter = FileSystemAdapter(root)

    with pytest.raises(ValueError):
        adapter.write("../escape.txt", "nope", version=0)


def test_filesystem_adapter_version_mismatch(tmp_path):
    root = tmp_path / "fs"
    root.mkdir()
    adapter = FileSystemAdapter(root)

    adapter.write("a.txt", "one", version=0)
    with pytest.raises(ValueError):
        adapter.write("a.txt", "two", version=0)


def test_filesystem_adapter_delete_removes_file(tmp_path):
    root = tmp_path / "fs"
    root.mkdir()
    adapter = FileSystemAdapter(root)

    adapter.write("a.txt", "hello", version=0)
    adapter.delete("a.txt")
    assert not (root / "a.txt").exists()
    with pytest.raises(FileNotFoundError):
        adapter.read("a.txt")


def test_storage_conflict_rolls_back_content_and_version():
    adapter = MemoryStorageAdapter(initial={"f.txt": "orig"})
    vfs = VirtualFileSystem()
    vfs.mount_storage("/d", adapter)
    adapter.write("f.txt", "external", version=0)

    with pytest.raises(InvalidOperation):
        vfs.write_file("/d/f.txt", "mine")

    assert vfs.read_file("/d/f.txt") == "orig"
    assert vfs.get_version("/d/f.txt") == 0


def test_storage_conflict_on_new_file_removes_it():
    class RejectingAdapter(MemoryStorageAdapter):
        def write(self, path, content, *, version):
            raise ValueError("version mismatch")

    vfs = VirtualFileSystem()
    vfs.mount_storage("/d", RejectingAdapter())

    with pytest.raises(InvalidOperation):
        vfs.write_file("/d/new.txt", "x")

    assert not vfs.exists("/d/new.txt")


def test_move_within_storage_mount_persists():
    adapter = MemoryStorageAdapter(initial={"a.txt": "A", "dir/b.txt": "B"})
    vfs = VirtualFileSystem()
    vfs.mount_storage("/d", adapter)

    vfs.move("/d/a.txt", "/d/renamed.txt")
    vfs.move("/d/dir", "/d/moved")

    assert sorted(adapter.list()) == ["moved/b.txt", "renamed.txt"]
    assert adapter.read("renamed.txt").content == "A"


def test_move_out_of_storage_mount_deletes_entry():
    adapter = MemoryStorageAdapter(initial={"a.txt": "A"})
    vfs = VirtualFileSystem()
    vfs.mount_storage("/d", adapter)

    vfs.move("/d/a.txt", "/scratch.txt")

    assert adapter.list() == {}
    assert vfs.read_file("/scratch.txt") == "A"


def test_copy_into_storage_mount_persists():
    adapter = MemoryStorageAdapter(initial={"a.txt": "A"})
    vfs = VirtualFileSystem()
    vfs.mount_storage("/d", adapter)
    vfs.write_file("/outside/c.txt", "C")

    vfs.copy("/d/a.txt", "/d/b.txt")
    vfs.copy("/outside", "/d/imported", recursive=True)
    vfs.copy("/outside/c.txt", "/d/a.txt")

    entries = adapter.list()
    assert sorted(entries) == ["a.txt", "b.txt", "imported/c.txt"]
    assert entries["a.txt"].content == "C"
    assert entries["b.txt"].content == "A"


def test_copy_and_move_emit_path_events():
    vfs = VirtualFileSystem()
    vfs.write_file("/src/a.txt", "A")
    events = []
    vfs.register_path_hook("/", events.append)

    vfs.copy("/src/a.txt", "/src/b.txt")
    vfs.move("/src/b.txt", "/src/c.txt")

    assert [(e.path, e.event) for e in events] == [
        ("/src/b.txt", "create"),
        ("/src/b.txt", "delete"),
        ("/src/c.txt", "create"),
    ]


def test_move_of_storage_mount_root_is_rejected():
    adapter = MemoryStorageAdapter(initial={"a.txt": "A"})
    vfs = VirtualFileSystem()
    vfs.mount_storage("/d", adapter)
    vfs.mkdir("/outer")
    vfs.mount_storage("/outer/inner", MemoryStorageAdapter(initial={"b.txt": "B"}))

    with pytest.raises(InvalidOperation):
        vfs.move("/d", "/renamed")
    with pytest.raises(InvalidOperation):
        vfs.move("/outer", "/elsewhere")

    assert sorted(adapter.list()) == ["a.txt"]
    assert vfs.read_file("/d/a.txt") == "A"
