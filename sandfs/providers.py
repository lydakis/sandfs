"""Provider protocols and helper dataclasses."""

from __future__ import annotations

from collections.abc import Callable, Mapping, MutableMapping
from dataclasses import dataclass, field
from pathlib import PurePosixPath
from typing import Any, Literal

if False:  # pragma: no cover - for type checkers only
    from .vfs import VirtualFileSystem
from .policies import NodePolicy


@dataclass(frozen=True)
class NodeContext:
    """Context passed to providers when materializing nodes."""

    path: PurePosixPath
    metadata: Mapping[str, Any]
    vfs: "VirtualFileSystem" | None = None


ContentProvider = Callable[[NodeContext], str]
DirectoryProvider = Callable[[NodeContext], "DirectorySnapshot"]
DirectorySnapshot = Mapping[str, "ProvidedNode"]


@dataclass
class ProvidedNode:
    """Represents a node returned by a directory provider."""

    kind: Literal["file", "dir"]
    content: str | None = None
    content_provider: ContentProvider | None = None
    directory_provider: DirectoryProvider | None = None
    children: MutableMapping[str, "ProvidedNode"] | None = None
    metadata: dict[str, Any] = field(default_factory=dict)
    policy: NodePolicy | None = None

    @staticmethod
    def file(
        *,
        content: str | None = None,
        content_provider: ContentProvider | None = None,
        metadata: Mapping[str, Any] | None = None,
        policy: NodePolicy | None = None,
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
        children: Mapping[str, "ProvidedNode"] | None = None,
        directory_provider: DirectoryProvider | None = None,
        metadata: Mapping[str, Any] | None = None,
        policy: NodePolicy | None = None,
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
