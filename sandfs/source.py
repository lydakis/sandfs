"""The interface a source written in Python implements."""

from collections.abc import Iterable
from typing import Protocol

from ._core import DirEntry


class Source(Protocol):
    """A read-only tree, such as an API, an object store or a database.

    Paths are absolute within the source, so its root is ``"/"``. The workspace calls
    ``list`` the first time something in a directory is touched and ``read`` the first
    time a file is read, then caches both and shares them with forks, so a source must
    return the same answer every time. An exception raised here surfaces as ``OSError``.
    """

    def list(self, path: str) -> Iterable[DirEntry]:
        """The entries of the directory at ``path``."""
        ...

    def read(self, path: str) -> bytes | str:
        """The contents of the file at ``path``; text is stored as UTF-8."""
        ...
