"""sandfs package: virtual filesystem sandbox with a mini shell."""

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
