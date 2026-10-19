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

    A source that can read part of a file (an object store with range requests) may
    also define ``read_range(path, offset, length)``. Without it, a ranged read of a
    file not yet read whole calls ``read`` and slices the result.

    A source where each call is a slow trip may also define
    ``prefetch(path, recursive)``: a hint that the files in the directory at ``path``
    (with ``recursive``, everything below it) are about to be read, so it can fetch
    them in one trip. The workspace calls it when a walk reads files, and when a
    second file in a directory needs a size its listing left out; it then lists the
    directory again. A prefetch must not change any answer, except that a listing may
    now include sizes. It may run on any thread, and an exception it raises is
    reported, not raised.
    """

    def list(self, path: str) -> Iterable[DirEntry]:
        """The entries of the directory at ``path``."""
        ...

    def read(self, path: str) -> bytes | str:
        """The contents of the file at ``path``; text is stored as UTF-8."""
        ...
