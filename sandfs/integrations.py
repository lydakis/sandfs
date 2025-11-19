"""Integration helpers for path-scoped events."""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass, field
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .vfs import VirtualFileSystem


@dataclass(frozen=True)
class PathEvent:
    path: str
    event: str  # "create", "update", "delete"
    content: str | None


PathHook = Callable[[PathEvent], None]


@dataclass
class InboxRecorder:
    events: list[dict[str, str | None]] = field(default_factory=list)

    def attach(self, vfs: "VirtualFileSystem", prefix: str) -> None:
        vfs.register_path_hook(prefix, self._handle)

    def _handle(self, event: PathEvent) -> None:
        self.events.append({"path": event.path, "event": event.event, "content": event.content})


__all__ = ["PathEvent", "PathHook", "InboxRecorder"]
