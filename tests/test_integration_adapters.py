from sandfs import VirtualFileSystem
from sandfs.integrations import InboxRecorder


def test_inbox_recorder_collects_events():
    vfs = VirtualFileSystem()
    inbox = InboxRecorder()
    inbox.attach(vfs, "/inbox")

    vfs.write_file("/inbox/item.txt", "hello")
    vfs.write_file("/inbox/item.txt", "world")
    vfs.remove("/inbox/item.txt")

    assert inbox.events == [
        {"path": "/inbox/item.txt", "event": "create", "content": "hello"},
        {"path": "/inbox/item.txt", "event": "update", "content": "world"},
        {"path": "/inbox/item.txt", "event": "delete", "content": None},
    ]
