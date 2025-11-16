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
    classification: str = "public"
    principals: Set[str] = field(default_factory=set)


@dataclass(frozen=True)
class VisibilityView:
    """Filters nodes by classification labels and principals."""

    classifications: Optional[FrozenSet[str]] = None
    principals: Optional[FrozenSet[str]] = None

    def __init__(
        self,
        classifications: Optional[Iterable[str]] = None,
        principals: Optional[Iterable[str]] = None,
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


__all__ = ["NodePolicy", "VisibilityView"]
