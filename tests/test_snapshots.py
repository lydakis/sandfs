from sandfs import MemoryStorageAdapter, VirtualFileSystem


def test_export_to_path_writes_expected_tree(tmp_path):
    vfs = VirtualFileSystem()
    vfs.write_file("/root.txt", "root")
    vfs.write_file("/docs/readme.md", "readme contents")
    vfs.write_file("/docs/nested/info.txt", "info")

    export_dir = tmp_path / "export"
    vfs.export_to_path(export_dir)

    assert (export_dir / "root.txt").read_text() == "root"
    assert (export_dir / "docs" / "readme.md").read_text() == "readme contents"
    assert (export_dir / "docs" / "nested" / "info.txt").read_text() == "info"


def test_materialize_context_cleans_up_temp_dir():
    vfs = VirtualFileSystem()
    vfs.write_file("/subdir/note.txt", "data")

    with vfs.materialize("/subdir") as root:
        assert root.is_dir()
        assert (root / "note.txt").read_text() == "data"

    assert not root.exists()


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
