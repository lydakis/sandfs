"""Storage adapter interfaces."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Dict, Mapping, Optional


@dataclass
class StorageEntry:
    content: str
    version: int = 0


class StorageAdapter:
    def read(self, path: str) -> StorageEntry:
        raise NotImplementedError

    def write(self, path: str, content: str, *, version: int) -> StorageEntry:
        raise NotImplementedError

    def list(self) -> Dict[str, StorageEntry]:
        raise NotImplementedError

    def delete(self, path: str) -> None:
        raise NotImplementedError


@dataclass
class MemoryStorageAdapter(StorageAdapter):
    initial: Mapping[str, str] = field(default_factory=dict)

    def __post_init__(self) -> None:
        self._entries: Dict[str, StorageEntry] = {
            path: StorageEntry(content=text, version=0)
            for path, text in self.initial.items()
        }

    def read(self, path: str) -> StorageEntry:
        entry = self._entries.get(path)
        if entry is None:
            raise FileNotFoundError(path)
        return StorageEntry(content=entry.content, version=entry.version)

    def write(self, path: str, content: str, *, version: int) -> StorageEntry:
        entry = self._entries.get(path)
        if entry and entry.version != version:
            raise ValueError("version mismatch")
        next_version = version + 1
        entry = StorageEntry(content=content, version=next_version)
        self._entries[path] = entry
        return StorageEntry(content=entry.content, version=entry.version)

    def list(self) -> Dict[str, StorageEntry]:
        return {path: StorageEntry(content=entry.content, version=entry.version) for path, entry in self._entries.items()}

    def delete(self, path: str) -> None:
        self._entries.pop(path, None)
