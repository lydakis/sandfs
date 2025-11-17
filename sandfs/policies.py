"""Policy and visibility helpers for sandfs."""

from __future__ import annotations

from collections.abc import Iterable
from dataclasses import dataclass, field


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

    def __init__(
        self,
        classifications: Iterable[str] | None = None,
        principals: Iterable[str] | None = None,
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
