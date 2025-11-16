import pytest

from sandfs import VirtualFileSystem
from sandfs.exceptions import InvalidOperation
from sandfs.hooks import WriteEvent


def test_write_versions_and_expected_version():
    vfs = VirtualFileSystem()
    vfs.write_file("/notes/a.txt", "hello")
    assert vfs.get_version("/notes/a.txt") == 1
    vfs.write_file("/notes/a.txt", "world", expected_version=1)
    assert vfs.get_version("/notes/a.txt") == 2
    with pytest.raises(InvalidOperation):
        vfs.write_file("/notes/a.txt", "boom", expected_version=1)


def test_write_hooks_receive_events():
    vfs = VirtualFileSystem()
    events: list[WriteEvent] = []

    def hook(event: WriteEvent) -> None:
        events.append(event)

    vfs.register_write_hook("/notes", hook)
    vfs.write_file("/notes/a.txt", "alpha")
    vfs.append_file("/notes/a.txt", "beta")

    assert [event.path for event in events] == ["/notes/a.txt", "/notes/a.txt"]
    assert events[0].version == 1
    assert events[1].append is True
    assert "beta" in events[1].content
