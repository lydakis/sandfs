"""Hook dataclasses and type hints for persistence adapters."""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass


@dataclass(frozen=True)
class WriteEvent:
    path: str
    content: str
    version: int
    append: bool


WriteHook = Callable[[WriteEvent], None]


__all__ = ["WriteEvent", "WriteHook"]
