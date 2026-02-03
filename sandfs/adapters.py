"""Storage adapter interfaces."""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from pathlib import Path, PurePosixPath


@dataclass
class StorageEntry:
    content: str
    version: int = 0


class StorageAdapter:
    def read(self, path: str) -> StorageEntry:
        raise NotImplementedError

    def write(self, path: str, content: str, *, version: int) -> StorageEntry:
        raise NotImplementedError

    def list(self) -> dict[str, StorageEntry]:
        raise NotImplementedError

    def delete(self, path: str) -> None:
        raise NotImplementedError


@dataclass
class MemoryStorageAdapter(StorageAdapter):
    initial: Mapping[str, str] = field(default_factory=dict)

    def __post_init__(self) -> None:
        self._entries: dict[str, StorageEntry] = {
            path: StorageEntry(content=text, version=0) for path, text in self.initial.items()
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

    def list(self) -> dict[str, StorageEntry]:
        return {
            path: StorageEntry(content=entry.content, version=entry.version)
            for path, entry in self._entries.items()
        }

    def delete(self, path: str) -> None:
        self._entries.pop(path, None)


@dataclass
class FileSystemAdapter(StorageAdapter):
    root: Path
    encoding: str = "utf-8"

    def __post_init__(self) -> None:
        self.root = Path(self.root)
        self.root.mkdir(parents=True, exist_ok=True)
        self._root_resolved = self.root.resolve()
        self._versions: dict[str, int] = {}
        self._mtimes: dict[str, int] = {}

    def _resolve(self, path: str) -> Path:
        rel = PurePosixPath(path)
        if rel.is_absolute():
            rel = rel.relative_to("/")
        target = (self.root / Path(rel.as_posix())).resolve()
        if not target.is_relative_to(self._root_resolved):
            raise ValueError(f"Path escapes adapter root: {path}")
        return target

    def _refresh_version(self, rel_path: str, *, target: Path | None = None) -> int:
        if target is None:
            target = self._resolve(rel_path)
        if not target.exists():
            self._versions.pop(rel_path, None)
            self._mtimes.pop(rel_path, None)
            return 0
        mtime = target.stat().st_mtime_ns
        if rel_path not in self._versions:
            self._versions[rel_path] = 0
            self._mtimes[rel_path] = mtime
            return 0
        if self._mtimes.get(rel_path) != mtime:
            self._versions[rel_path] = self._versions.get(rel_path, 0) + 1
            self._mtimes[rel_path] = mtime
        return self._versions.get(rel_path, 0)

    def read(self, path: str) -> StorageEntry:
        target = self._resolve(path)
        if not target.exists():
            raise FileNotFoundError(path)
        version = self._refresh_version(path, target=target)
        content = target.read_text(encoding=self.encoding, errors="replace")
        return StorageEntry(content=content, version=version)

    def write(self, path: str, content: str, *, version: int) -> StorageEntry:
        target = self._resolve(path)
        current = self._refresh_version(path, target=target)
        if current != version:
            raise ValueError("version mismatch")
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding=self.encoding, errors="replace")
        mtime = target.stat().st_mtime_ns
        new_version = current + 1
        self._versions[path] = new_version
        self._mtimes[path] = mtime
        return StorageEntry(content=content, version=new_version)

    def list(self) -> dict[str, StorageEntry]:
        entries: dict[str, StorageEntry] = {}
        for path in self.root.rglob("*"):
            if not path.is_file():
                continue
            rel_path = path.relative_to(self.root).as_posix()
            version = self._refresh_version(rel_path, target=path)
            content = path.read_text(encoding=self.encoding, errors="replace")
            entries[rel_path] = StorageEntry(content=content, version=version)
        return entries

    def delete(self, path: str) -> None:
        target = self._resolve(path)
        if target.exists():
            target.unlink()
        self._versions.pop(path, None)
        self._mtimes.pop(path, None)
