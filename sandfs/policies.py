"""Policy and visibility helpers for sandfs."""

from __future__ import annotations

from dataclasses import dataclass
from typing import FrozenSet, Iterable, Optional


@dataclass
class NodePolicy:
    """Controls access, write semantics, and visibility for a node."""

    readable: bool = True
    writable: bool = True
    append_only: bool = False
    visibility: str = "public"


@dataclass(frozen=True)
class VisibilityView:
    """Filters nodes by visibility labels."""

    allowed: Optional[FrozenSet[str]] = None

    def __init__(self, allowed: Optional[Iterable[str]] = None) -> None:
        object.__setattr__(self, "allowed", frozenset(allowed) if allowed is not None else None)

    def allows(self, policy: NodePolicy) -> bool:
        if self.allowed is None:
            return True
        return policy.visibility in self.allowed


__all__ = ["NodePolicy", "VisibilityView"]
