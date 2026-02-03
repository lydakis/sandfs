"""Policy and visibility helpers for sandfs."""

from __future__ import annotations

from collections.abc import Iterable, Mapping
from dataclasses import dataclass, field
from pathlib import PurePosixPath
from typing import TYPE_CHECKING

if TYPE_CHECKING:  # pragma: no cover
    from .nodes import VirtualNode


@dataclass
class NodePolicy:
    """Controls access, write semantics, and visibility for a node."""

    readable: bool = True
    writable: bool = True
    append_only: bool = False
    classification: str = "public"
    principals: set[str] = field(default_factory=set)


@dataclass(frozen=True)
class VisibilityView:
    """Filters nodes by classification labels and principals."""

    classifications: frozenset[str] | None = None
    principals: frozenset[str] | None = None
    path_prefixes: frozenset[PurePosixPath] | None = None
    metadata_filters: Mapping[str, object] | None = None

    def __init__(
        self,
        classifications: Iterable[str] | None = None,
        principals: Iterable[str] | None = None,
        path_prefixes: Iterable[str | PurePosixPath] | None = None,
        metadata_filters: Mapping[str, object] | None = None,
    ) -> None:
        object.__setattr__(
            self,
            "classifications",
            frozenset(classifications) if classifications is not None else None,
        )
        object.__setattr__(
            self,
            "principals",
            frozenset(principals) if principals is not None else None,
        )
        object.__setattr__(
            self,
            "path_prefixes",
            frozenset(PurePosixPath(prefix) for prefix in path_prefixes)
            if path_prefixes is not None
            else None,
        )
        object.__setattr__(
            self,
            "metadata_filters",
            dict(metadata_filters) if metadata_filters is not None else None,
        )

    def allows(self, policy: NodePolicy) -> bool:
        if policy.principals:
            if self.principals is None:
                return False
            if not (policy.principals & self.principals):
                return False
            return True
        if self.classifications is None:
            return True
        return policy.classification in self.classifications

    def allows_node(self, node: "VirtualNode") -> bool:
        if not self.allows(node.policy):
            return False
        if self.path_prefixes is not None:
            node_path = node.path()
            if not any(
                node_path.is_relative_to(prefix) or prefix.is_relative_to(node_path)
                for prefix in self.path_prefixes
            ):
                return False
        if self.metadata_filters is not None:
            for key, value in self.metadata_filters.items():
                if node.metadata.get(key) != value:
                    return False
        return True


__all__ = ["NodePolicy", "VisibilityView"]
