from pathlib import PurePosixPath

from sandfs import VirtualFileSystem
from sandfs.search import FullTextIndex, SearchQuery


def test_full_text_index_search_options():
    index = FullTextIndex()
    entries = [
        (PurePosixPath("/notes/a.txt"), "Hello\nworld\n"),
        (PurePosixPath("/notes/b.txt"), "HELLO\nthere\n"),
        (PurePosixPath("/other/c.txt"), "hello\n"),
    ]
    index.build(entries)

    results = index.search(SearchQuery(query="hello", ignore_case=True))
    assert {result.path for result in results} == {
        PurePosixPath("/notes/a.txt"),
        PurePosixPath("/notes/b.txt"),
        PurePosixPath("/other/c.txt"),
    }

    scoped = index.search(
        SearchQuery(
            query="h.llo",
            regex=True,
            ignore_case=True,
            path_prefix=PurePosixPath("/notes"),
        )
    )
    assert {result.path for result in scoped} == {
        PurePosixPath("/notes/a.txt"),
        PurePosixPath("/notes/b.txt"),
    }

    index.remove_file(PurePosixPath("/notes/b.txt"))
    remaining = index.search(SearchQuery(query="hello", ignore_case=True))
    assert {result.path for result in remaining} == {
        PurePosixPath("/notes/a.txt"),
        PurePosixPath("/other/c.txt"),
    }

    index.index_file(PurePosixPath("/notes/b.txt"), "readded")
    readded = index.search(SearchQuery(query="readded"))
    assert {result.path for result in readded} == {PurePosixPath("/notes/b.txt")}


def test_vfs_search_index_updates_on_move_copy_remove():
    vfs = VirtualFileSystem()
    vfs.enable_full_text_index()
    vfs.write_file("/docs/a.txt", "hello")
    query = SearchQuery(query="hello")

    def paths() -> set[PurePosixPath]:
        return {result.path for result in vfs.search(query)}

    assert paths() == {PurePosixPath("/docs/a.txt")}

    vfs.move("/docs/a.txt", "/docs/b.txt")
    assert paths() == {PurePosixPath("/docs/b.txt")}

    vfs.copy("/docs/b.txt", "/docs/c.txt")
    assert paths() == {
        PurePosixPath("/docs/b.txt"),
        PurePosixPath("/docs/c.txt"),
    }

    vfs.remove("/docs/b.txt")
    assert paths() == {PurePosixPath("/docs/c.txt")}

    scoped = vfs.search(SearchQuery(query="hello", path_prefix=PurePosixPath("/other")))
    assert scoped == []
