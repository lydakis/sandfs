"""Policy and visibility helpers for sandfs."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import FrozenSet, Iterable, Optional, Set


@dataclass
class NodePolicy:
    """Controls access, write semantics, and visibility for a node."""

    readable: bool = True
    writable: bool = True
    append_only: bool = False
    visibility: str = "public"
    contacts: Set[str] = field(default_factory=set)


@dataclass(frozen=True)
class VisibilityView:
    """Filters nodes by visibility labels."""

    labels: Optional[FrozenSet[str]] = None
    contacts: Optional[FrozenSet[str]] = None

    def __init__(
        self,
        labels: Optional[Iterable[str]] = None,
        contacts: Optional[Iterable[str]] = None,
    ) -> None:
        object.__setattr__(self, "labels", frozenset(labels) if labels is not None else None)
        object.__setattr__(self, "contacts", frozenset(contacts) if contacts is not None else None)

    def allows(self, policy: NodePolicy) -> bool:
        if policy.contacts:
            if self.contacts is None:
                return False
            if not (policy.contacts & self.contacts):
                return False
            return True
        if self.labels is None:
            return True
        return policy.visibility in self.labels


__all__ = ["NodePolicy", "VisibilityView"]
