from sandfs import VirtualFileSystem
from sandfs.adapters import MemoryStorageAdapter
from sandfs.policies import NodePolicy


def test_storage_manager_persists_and_deletes_entries():
    vfs = VirtualFileSystem()
    adapter = MemoryStorageAdapter()
    vfs.mount_storage("/records", adapter, policy=NodePolicy(writable=True))

    vfs.write_file("/records/a.txt", "hello")
    assert adapter.list()["a.txt"].content == "hello"

    vfs.remove("/records/a.txt")
    assert "a.txt" not in adapter.list()


def test_hook_manager_emits_path_events_on_delete():
    vfs = VirtualFileSystem()
    events: list[tuple[str, str]] = []

    def path_hook(event):
        events.append((event.event, event.path))

    vfs.register_path_hook("/notes", path_hook)
    vfs.write_file("/notes/info.txt", "alpha")
    vfs.remove("/notes/info.txt")

    assert events == [("create", "/notes/info.txt"), ("delete", "/notes/info.txt")]
