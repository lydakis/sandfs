"""sandfs: a lazy, copy-on-write filesystem for agents.

A ``Workspace`` mounts read-only sources (a git commit, a host directory, in-memory
files), keeps every change in a copy-on-write overlay, forks in O(1) and reports what
changed with ``diff()``. The implementation is the Rust core in ``sandfs._core``.
"""

from importlib.metadata import version

from ._core import Change, DirEntry, EditFailed, Stat, VersionMismatch, Workspace

__version__ = version("sandfs")

__all__ = [
    "Workspace",
    "DirEntry",
    "Stat",
    "Change",
    "VersionMismatch",
    "EditFailed",
    "__version__",
]
