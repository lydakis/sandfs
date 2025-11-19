"""Collaborator classes that extend VirtualFileSystem behavior."""

from __future__ import annotations

from pathlib import PurePosixPath
from typing import TYPE_CHECKING

from .adapters import StorageAdapter
from .exceptions import InvalidOperation
from .hooks import WriteEvent, WriteHook
from .integrations import PathEvent, PathHook
from .nodes import VirtualDirectory, VirtualFile
from .policies import NodePolicy

if TYPE_CHECKING:
    from .vfs import VirtualFileSystem


class HookManager:
    """Dispatch write/path events to registered callbacks."""

    def __init__(self, vfs: "VirtualFileSystem") -> None:
        self.vfs = vfs
        self._write_hooks: list[tuple[PurePosixPath, WriteHook]] = []
        self._path_hooks: list[tuple[PurePosixPath, PathHook]] = []

    def register_write_hook(self, prefix: str | PurePosixPath, hook: WriteHook) -> None:
        normalized = self.vfs._normalize(prefix)
        self._write_hooks.append((normalized, hook))

    def register_path_hook(self, prefix: str | PurePosixPath, hook: PathHook) -> None:
        normalized = self.vfs._normalize(prefix)
        self._path_hooks.append((normalized, hook))

    def emit_write_event(self, node: VirtualFile, *, append: bool, event_type: str) -> None:
        if self._write_hooks:
            path = node.path()
            content = node.read(self.vfs)
            event = WriteEvent(path=str(path), content=content, version=node.version, append=append)
            for prefix, hook in self._write_hooks:
                if self.vfs._path_matches_prefix(path, prefix):
                    hook(event)
            self.emit_path_event(path, event_type, content)
        else:
            self.emit_path_event(node.path(), event_type, node.read(self.vfs))

    def emit_path_event(self, path: PurePosixPath, event_type: str, content: str | None) -> None:
        if not self._path_hooks:
            return
        payload = PathEvent(path=str(path), event=event_type, content=content)
        for prefix, hook in self._path_hooks:
            if self.vfs._path_matches_prefix(path, prefix):
                hook(payload)


class StorageManager:
    """Handle storage mounts and persistence for VirtualFileSystem."""

    def __init__(self, vfs: "VirtualFileSystem") -> None:
        self.vfs = vfs
        self._storage_mounts: dict[PurePosixPath, StorageAdapter] = {}

    @property
    def mounts(self) -> dict[PurePosixPath, StorageAdapter]:
        return self._storage_mounts

    def replace_mounts(self, mounts: dict[PurePosixPath, StorageAdapter]) -> None:
        self._storage_mounts = dict(mounts)

    def mount(
        self,
        path: str | PurePosixPath,
        adapter: StorageAdapter,
        *,
        policy: NodePolicy | None = None,
    ) -> VirtualDirectory:
        normalized = self.vfs._normalize(path)
        directory = self.vfs.mkdir(normalized, parents=True, exist_ok=True)
        if policy is not None:
            directory.policy = policy
        self._storage_mounts[normalized] = adapter
        self._load_mount(normalized, adapter)
        return directory

    def sync(self, path: str | PurePosixPath) -> None:
        normalized = self.vfs._normalize(path)
        adapter = self._storage_mounts.get(normalized)
        if adapter is None:
            raise InvalidOperation(f"No storage mount at {normalized}")
        self._load_mount(normalized, adapter)

    def persist(self, node: VirtualFile, previous_version: int) -> None:
        mount = self._find_mount(node.path())
        if not mount:
            return
        prefix, adapter = mount
        relative = self._relative_path(node.path(), prefix)
        try:
            adapter.write(relative, node.read(self.vfs), version=previous_version)
        except ValueError as exc:
            node.version = previous_version
            raise InvalidOperation(f"Storage conflict for {node.path()}") from exc

    def delete_entry(self, node: VirtualFile) -> None:
        mount = self._find_mount(node.path())
        if not mount:
            return
        prefix, adapter = mount
        relative = self._relative_path(node.path(), prefix)
        adapter.delete(relative)

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------
    def _load_mount(self, prefix: PurePosixPath, adapter: StorageAdapter) -> None:
        directory = self.vfs._resolve_dir(prefix)
        directory.children.clear()
        directory._loaded = True
        for rel_path, entry in adapter.list().items():
            absolute = prefix.joinpath(PurePosixPath(rel_path))
            file_node = self.vfs._ensure_file(absolute, create=True)
            file_node.write(entry.content)
            file_node.version = entry.version

    def _find_mount(self, path: PurePosixPath) -> tuple[PurePosixPath, StorageAdapter] | None:
        matches: list[tuple[PurePosixPath, StorageAdapter]] = []
        for prefix, adapter in self._storage_mounts.items():
            if self.vfs._path_matches_prefix(path, prefix):
                matches.append((prefix, adapter))
        if not matches:
            return None
        return max(matches, key=lambda item: len(item[0].parts))

    def _relative_path(self, path: PurePosixPath, prefix: PurePosixPath) -> str:
        rel = path.relative_to(prefix)
        return rel.as_posix()


__all__ = ["HookManager", "StorageManager"]
