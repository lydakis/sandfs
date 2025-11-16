from sandfs import MemoryStorageAdapter, VirtualFileSystem


def test_snapshot_restore_in_memory():
    vfs = VirtualFileSystem()
    vfs.write_file("/notes/a.txt", "hello")
    vfs.write_file("/notes/b.txt", "world")

    snap = vfs.snapshot()
    vfs.write_file("/notes/a.txt", "boom")
    vfs.remove("/notes/b.txt")

    vfs.restore(snap)
    assert vfs.read_file("/notes/a.txt") == "hello"
    assert vfs.read_file("/notes/b.txt") == "world"


def test_snapshot_restore_with_storage_mount():
    adapter = MemoryStorageAdapter(initial={"a.txt": "hello"})
    vfs = VirtualFileSystem()
    vfs.mount_storage("/data", adapter)
    snap = vfs.snapshot()

    vfs.write_file("/data/a.txt", "local change")
    adapter.write("a.txt", "external", version=adapter.read("a.txt").version)

    vfs.restore(snap)
    assert vfs.read_file("/data/a.txt") == "hello"
    vfs.sync_storage("/data")
    assert vfs.read_file("/data/a.txt") == "external"
