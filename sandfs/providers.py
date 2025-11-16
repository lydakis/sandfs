"""Provider protocols and helper dataclasses."""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import PurePosixPath
from typing import Any, Callable, Dict, Iterable, Literal, Mapping, MutableMapping, Optional

if False:  # pragma: no cover - for type checkers only
    from .vfs import VirtualFileSystem
from .policies import NodePolicy


@dataclass(frozen=True)
class NodeContext:
    """Context passed to providers when materializing nodes."""

    path: PurePosixPath
    metadata: Mapping[str, Any]
    vfs: Optional["VirtualFileSystem"] = None


ContentProvider = Callable[[NodeContext], str]
DirectoryProvider = Callable[[NodeContext], "DirectorySnapshot"]
DirectorySnapshot = Mapping[str, "ProvidedNode"]


@dataclass
class ProvidedNode:
    """Represents a node returned by a directory provider."""

    kind: Literal["file", "dir"]
    content: Optional[str] = None
    content_provider: Optional[ContentProvider] = None
    directory_provider: Optional[DirectoryProvider] = None
    children: Optional[MutableMapping[str, "ProvidedNode"]] = None
    metadata: Dict[str, Any] = field(default_factory=dict)
    policy: Optional[NodePolicy] = None

    @staticmethod
    def file(
        *,
        content: Optional[str] = None,
        content_provider: Optional[ContentProvider] = None,
        metadata: Optional[Mapping[str, Any]] = None,
        policy: Optional[NodePolicy] = None,
    ) -> "ProvidedNode":
        return ProvidedNode(
            kind="file",
            content=content,
            content_provider=content_provider,
            metadata=dict(metadata or {}),
            policy=policy,
        )

    @staticmethod
    def directory(
        *,
        children: Optional[Mapping[str, "ProvidedNode"]] = None,
        directory_provider: Optional[DirectoryProvider] = None,
        metadata: Optional[Mapping[str, Any]] = None,
        policy: Optional[NodePolicy] = None,
    ) -> "ProvidedNode":
        frozen_children = None
        if children is not None:
            frozen_children = {name: child for name, child in children.items()}
        return ProvidedNode(
            kind="dir",
            children=frozen_children,
            directory_provider=directory_provider,
            metadata=dict(metadata or {}),
            policy=policy,
        )


__all__ = [
    "NodeContext",
    "ContentProvider",
    "DirectoryProvider",
    "DirectorySnapshot",
    "ProvidedNode",
]
