"""Integration helpers for path-scoped events."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Callable


@dataclass(frozen=True)
class PathEvent:
    path: str
    event: str  # "create", "update", "delete"
    content: str | None


PathHook = Callable[[PathEvent], None]


__all__ = ["PathEvent", "PathHook"]
