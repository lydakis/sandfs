from sandfs import VirtualFileSystem
from sandfs.providers import ProvidedNode


def test_write_and_read_file():
    vfs = VirtualFileSystem()
    vfs.write_file("/notes/todo.txt", "- build VFS\n")
    assert vfs.read_file("/notes/todo.txt") == "- build VFS\n"
    entries = vfs.ls("/notes")
    assert entries[0].name == "todo.txt"


def test_dynamic_file_provider():
    vfs = VirtualFileSystem()

    def provider(ctx):
        return f"dynamic at {ctx.path}"

    vfs.mount_file("/dynamic/info.txt", provider)
    assert "dynamic at /dynamic/info.txt" == vfs.read_file("/dynamic/info.txt")


def test_directory_provider_populates_children():
    vfs = VirtualFileSystem()

    def loader(ctx):
        return {
            "README.md": ProvidedNode.file(content="hello"),
            "src": ProvidedNode.directory(
                children={
                    "main.py": ProvidedNode.file(content="print('hi')"),
                }
            ),
        }

    vfs.mount_directory("/templates/demo", loader)
    entries = {entry.name for entry in vfs.ls("/templates/demo")}
    assert entries == {"README.md", "src"}
    assert vfs.read_file("/templates/demo/src/main.py") == "print('hi')"


def test_tree_representation(tmp_path):
    vfs = VirtualFileSystem()
    vfs.write_file("/a/b/c.txt", "data")
    tree = vfs.tree("/a")
    assert "c.txt" in tree
