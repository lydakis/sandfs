"""Helpers for working with POSIX paths inside the VFS."""

from __future__ import annotations

from collections.abc import Iterable
from pathlib import PurePosixPath
from typing import TYPE_CHECKING, cast

from .exceptions import InvalidOperation, NodeNotFound
from .nodes import VirtualDirectory, VirtualFile, VirtualNode

if TYPE_CHECKING:
    from .vfs import VirtualFileSystem


class PathResolverMixin:
    """Utilities for resolving and normalizing POSIX paths."""

    root: VirtualDirectory
    cwd: VirtualDirectory

    def _normalize(self, path: str | PurePosixPath | None) -> PurePosixPath:
        if path is None or str(path) == "":
            base = self.cwd.path()
            raw = base
        else:
            raw = PurePosixPath(path)
            if not raw.is_absolute():
                base = self.cwd.path()
                raw = base.joinpath(raw)
        parts: list[str] = []
        for part in raw.parts:
            if part in ("", "/", "."):
                continue
            if part == "..":
                if parts:
                    parts.pop()
                continue
            parts.append(part)
        return PurePosixPath("/" + "/".join(parts)) if parts else PurePosixPath("/")

    def _iterate_parts(self, path: PurePosixPath) -> Iterable[str]:
        for part in path.parts:
            if part in ("", "/"):
                continue
            yield part

    def _resolve_node(self, path: str | PurePosixPath) -> VirtualNode:
        target = self._normalize(path)
        current: VirtualNode = self.root
        if target == PurePosixPath("/"):
            return self.root
        vfs = cast("VirtualFileSystem", self)
        for part in self._iterate_parts(target):
            if not isinstance(current, VirtualDirectory):
                raise InvalidOperation(f"{current.path()} is not a directory")
            current = current.get_child(part, vfs)
        return current

    def _resolve_dir(self, path: str | PurePosixPath, *, create: bool = False) -> VirtualDirectory:
        target = self._normalize(path)
        if target == PurePosixPath("/"):
            return self.root
        current = self.root
        vfs = cast("VirtualFileSystem", self)
        for part in self._iterate_parts(target):
            if not isinstance(current, VirtualDirectory):
                raise InvalidOperation(f"{current.path()} is not a directory")
            try:
                next_node = current.get_child(part, vfs)
            except NodeNotFound:
                if not create:
                    raise
                next_node = VirtualDirectory(name=part, parent=current)
                current.add_child(next_node)
            if not isinstance(next_node, VirtualDirectory):
                raise InvalidOperation(f"{next_node.path()} is not a directory")
            current = next_node
        return current

    def _ensure_file(self, path: str | PurePosixPath, *, create: bool = False) -> VirtualFile:
        target = self._normalize(path)
        parent_path = target.parent
        if parent_path == target:
            raise InvalidOperation("Cannot create file at root path")
        parent = self._resolve_dir(parent_path or PurePosixPath("/"), create=create)
        name = target.name
        if not name:
            raise InvalidOperation("Missing file name")
        vfs = cast("VirtualFileSystem", self)
        try:
            node = parent.get_child(name, vfs)
        except NodeNotFound:
            if not create:
                raise
            node = VirtualFile(name=name, parent=parent)
            parent.add_child(node)
            return node
        if not isinstance(node, VirtualFile):
            raise InvalidOperation(f"{node.path()} is not a file")
        return node

    def _path_matches_prefix(self, path: PurePosixPath, prefix: PurePosixPath) -> bool:
        if prefix == PurePosixPath("/"):
            return True
        try:
            path.relative_to(prefix)
            return True
        except ValueError:
            return False


__all__ = ["PathResolverMixin"]
