"""Integration helpers for path-scoped events."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Callable, Dict, List, TYPE_CHECKING

if TYPE_CHECKING:  # pragma: no cover
    from .vfs import VirtualFileSystem


@dataclass(frozen=True)
class PathEvent:
    path: str
    event: str  # "create", "update", "delete"
    content: str | None


PathHook = Callable[[PathEvent], None]


@dataclass
class InboxRecorder:
    events: List[Dict[str, str | None]] = field(default_factory=list)

    def attach(self, vfs: "VirtualFileSystem", prefix: str) -> None:
        vfs.register_path_hook(prefix, self._handle)

    def _handle(self, event: PathEvent) -> None:
        self.events.append({"path": event.path, "event": event.event, "content": event.content})


__all__ = ["PathEvent", "PathHook", "InboxRecorder"]
