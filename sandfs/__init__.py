"""sandfs: a lazy, copy-on-write filesystem for agents.

``Workspace`` is the Rust-backed core. The other names are the 0.3 pure-Python design.
"""

from ._core import Change, DirEntry, EditFailed, Stat, VersionMismatch, Workspace
from .adapters import FileSystemAdapter, MemoryStorageAdapter, StorageAdapter
from .hooks import WriteEvent, WriteHook
from .integrations import PathEvent, PathHook
from .policies import NodePolicy, VisibilityView
from .providers import ContentProvider, DirectoryProvider, NodeContext, ProvidedNode
from .pyexec import PythonExecutionResult, PythonExecutor
from .search import FullTextIndex, SearchQuery, SearchResult
from .shell import CommandResult, SandboxShell
from .vfs import VirtualFileSystem

__all__ = [
    "Workspace",
    "DirEntry",
    "Stat",
    "Change",
    "VersionMismatch",
    "EditFailed",
    "VirtualFileSystem",
    "SandboxShell",
    "CommandResult",
    "PythonExecutor",
    "PythonExecutionResult",
    "ContentProvider",
    "DirectoryProvider",
    "NodeContext",
    "ProvidedNode",
    "NodePolicy",
    "VisibilityView",
    "WriteEvent",
    "WriteHook",
    "PathEvent",
    "PathHook",
    "StorageAdapter",
    "MemoryStorageAdapter",
    "FileSystemAdapter",
    "SearchQuery",
    "SearchResult",
    "FullTextIndex",
]
