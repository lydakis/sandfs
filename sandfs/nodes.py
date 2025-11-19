"""Core node representations for the virtual filesystem."""

from __future__ import annotations

import time
from collections.abc import Iterator
from dataclasses import dataclass, field
from pathlib import PurePosixPath
from typing import TYPE_CHECKING

from .exceptions import InvalidOperation, NodeExists, NodeNotFound, ProviderError
from .policies import NodePolicy
from .providers import ContentProvider, DirectoryProvider, NodeContext, ProvidedNode

if TYPE_CHECKING:  # pragma: no cover
    from .vfs import VirtualFileSystem


@dataclass
class VirtualNode:
    """Base node stored inside the sandbox."""

    name: str
    parent: "VirtualDirectory" | None = None
    metadata: dict[str, object] = field(default_factory=dict)
    policy: NodePolicy = field(default_factory=NodePolicy)
    version: int = 0
    created_at: float = field(default_factory=time.time)
    modified_at: float = field(default_factory=time.time)

    def path(self) -> PurePosixPath:
        if self.parent is None:
            return PurePosixPath("/")
        segments = []
        node: VirtualNode | None = self
        while node and node.parent is not None:
            segments.append(node.name)
            node = node.parent
        return PurePosixPath("/" + "/".join(reversed(segments))) if segments else PurePosixPath("/")

    def build_context(self, vfs: "VirtualFileSystem" | None = None) -> NodeContext:
        return NodeContext(path=self.path(), metadata=self.metadata, vfs=vfs)


class VirtualFile(VirtualNode):
    """Represents a file backed by either static text or a provider."""

    def __init__(
        self,
        name: str,
        *,
        parent: "VirtualDirectory" | None = None,
        content: str | None = None,
        provider: ContentProvider | None = None,
        metadata: dict[str, object] | None = None,
    ) -> None:
        super().__init__(name=name, parent=parent, metadata=dict(metadata or {}))
        self._content = content or ""
        self._provider = provider

    def read(self, vfs: "VirtualFileSystem" | None = None) -> str:
        if self._provider is None:
            return self._content
        ctx = self.build_context(vfs)
        try:
            return self._provider(ctx)
        except Exception as exc:  # pragma: no cover - rewrap provider failures
            raise ProviderError(str(exc)) from exc

    def write(self, data: str, *, append: bool = False) -> None:
        if append:
            self._content += data
        else:
            self._content = data
        self._provider = None

    def set_provider(self, provider: ContentProvider) -> None:
        self._provider = provider


class VirtualDirectory(VirtualNode):
    """Directories store children lazily when a loader is present."""

    def __init__(
        self,
        name: str,
        *,
        parent: "VirtualDirectory" | None = None,
        loader: DirectoryProvider | None = None,
        metadata: dict[str, object] | None = None,
    ) -> None:
        super().__init__(name=name, parent=parent, metadata=dict(metadata or {}))
        self.loader = loader
        self._loaded = loader is None
        self.children: dict[str, VirtualNode] = {}

    def ensure_loaded(self, vfs: "VirtualFileSystem" | None = None) -> None:
        if self._loaded:
            return
        if self.loader is None:
            self._loaded = True
            return
        ctx = self.build_context(vfs)
        snapshot = self.loader(ctx)
        for name, provided in snapshot.items():
            if name in self.children:
                continue
            self.add_child(instantiate_provided_node(name, provided, parent=self))
        self._loaded = True

    def add_child(self, node: VirtualNode) -> None:
        if node.name in self.children:
            raise NodeExists(f"Node {node.name} already exists in {self.path()}")
        node.parent = self
        self.children[node.name] = node

    def remove_child(self, name: str) -> None:
        if name not in self.children:
            raise NodeNotFound(f"Child {name} not found in {self.path()}")
        del self.children[name]

    def get_child(self, name: str, vfs: "VirtualFileSystem" | None = None) -> VirtualNode:
        self.ensure_loaded(vfs)
        try:
            return self.children[name]
        except KeyError as exc:
            raise NodeNotFound(f"Child {name} not found in {self.path()}") from exc

    def iter_children(self, vfs: "VirtualFileSystem" | None = None) -> Iterator[VirtualNode]:
        self.ensure_loaded(vfs)
        return iter(self.children.values())


def instantiate_provided_node(
    name: str,
    provided: ProvidedNode,
    *,
    parent: VirtualDirectory | None,
) -> VirtualNode:
    if provided.kind == "file":
        node = VirtualFile(name=name, parent=parent, metadata=provided.metadata)
        if provided.content_provider:
            node.set_provider(provided.content_provider)
        else:
            node.write(provided.content or "")
        if provided.policy is not None:
            node.policy = provided.policy
        return node
    if provided.kind == "dir":
        node = VirtualDirectory(
            name=name,
            parent=parent,
            loader=provided.directory_provider,
            metadata=provided.metadata,
        )
        if provided.children:
            for child_name, child in provided.children.items():
                node.add_child(instantiate_provided_node(child_name, child, parent=node))
        if provided.policy is not None:
            node.policy = provided.policy
        return node
    raise InvalidOperation(f"Unknown provided node kind: {provided.kind}")


__all__ = [
    "VirtualNode",
    "VirtualFile",
    "VirtualDirectory",
    "instantiate_provided_node",
]
