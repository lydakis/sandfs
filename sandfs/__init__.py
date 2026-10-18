"""sandfs: a lazy, copy-on-write sandbox filesystem for agents.

A ``Workspace`` mounts read-only sources (a host directory, in-memory files, a git
commit, or any object with ``list`` and ``read`` methods), keeps every change in a
copy-on-write overlay, forks in O(1) and reports which files changed with ``changes()``.
Files are bytes of any kind. The implementation is the Rust core in ``sandfs._core``.
"""

from importlib.metadata import version

from ._core import (
    Change,
    DirEntry,
    DirSource,
    GitSource,
    MemorySource,
    Stat,
    VersionMismatch,
    Workspace,
)
from .source import Source

__version__ = version("sandfs")

__all__ = [
    "Workspace",
    "DirSource",
    "MemorySource",
    "GitSource",
    "Source",
    "DirEntry",
    "Stat",
    "Change",
    "VersionMismatch",
    "__version__",
]
