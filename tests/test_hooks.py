from sandfs import VirtualFileSystem
from sandfs.integrations import PathEvent


def test_path_hook_receives_create_update_delete():
    vfs = VirtualFileSystem()
    events: list[PathEvent] = []

    def handler(event: PathEvent) -> None:
        events.append(event)

    vfs.register_path_hook("/blue/inbox", handler)

    vfs.write_file("/blue/inbox/note.txt", "hello")
    vfs.write_file("/blue/inbox/note.txt", "world")
    vfs.remove("/blue/inbox/note.txt")

    assert [e.event for e in events] == ["create", "update", "delete"]
    assert events[0].content == "hello"
    assert events[1].content == "world"
    assert events[2].content is None
