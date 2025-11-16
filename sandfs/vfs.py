"""Virtual filesystem implementation."""

from __future__ import annotations

import contextlib
import tempfile
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Dict, Iterable, Iterator, List, Optional, Tuple

from .exceptions import InvalidOperation, NodeExists, NodeNotFound
from .hooks import WriteEvent, WriteHook
from .integrations import PathEvent, PathHook
from .nodes import VirtualDirectory, VirtualFile, VirtualNode
from .policies import NodePolicy, VisibilityView
from .providers import ContentProvider, DirectoryProvider
from .adapters import StorageAdapter


@dataclass
class DirEntry:
    name: str
    path: PurePosixPath
    is_dir: bool
    metadata: Dict[str, object]
    policy: NodePolicy


@dataclass
class NodeSnapshot:
    is_dir: bool
    metadata: Dict[str, object]
    policy: NodePolicy
    version: int
    content: Optional[str] = None


@dataclass
class VFSSnapshot:
    nodes: Dict[str, NodeSnapshot]
    cwd: PurePosixPath
    storage_mounts: Dict[str, StorageAdapter]


class VirtualFileSystem:
    """In-memory filesystem that supports dynamic nodes."""

    def __init__(self) -> None:
        self.root = VirtualDirectory(name="")
        self.cwd = self.root
        self._write_hooks: List[Tuple[PurePosixPath, WriteHook]] = []
        self._storage_mounts: Dict[PurePosixPath, StorageAdapter] = {}
        self._path_hooks: List[Tuple[PurePosixPath, PathHook]] = []

    # ------------------------------------------------------------------
    # Path helpers
    # ------------------------------------------------------------------
    def _normalize(self, path: str | PurePosixPath | None) -> PurePosixPath:
        if path is None or str(path) == "":
            base = self.cwd.path()
            raw = base
        else:
            raw = PurePosixPath(path)
            if not raw.is_absolute():
                base = self.cwd.path()
                raw = base.joinpath(raw)
        parts: List[str] = []
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
        for part in self._iterate_parts(target):
            if not isinstance(current, VirtualDirectory):
                raise InvalidOperation(f"{current.path()} is not a directory")
            current = current.get_child(part, self)
        return current

    def _resolve_dir(self, path: str | PurePosixPath, *, create: bool = False) -> VirtualDirectory:
        target = self._normalize(path)
        if target == PurePosixPath("/"):
            return self.root
        current = self.root
        for part in self._iterate_parts(target):
            if not isinstance(current, VirtualDirectory):
                raise InvalidOperation(f"{current.path()} is not a directory")
            try:
                next_node = current.get_child(part, self)
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
        try:
            node = parent.get_child(name, self)
        except NodeNotFound:
            if not create:
                raise
            node = VirtualFile(name=name, parent=parent)
            parent.add_child(node)
            return node
        if not isinstance(node, VirtualFile):
            raise InvalidOperation(f"{node.path()} is not a file")
        return node

    def _ensure_read_allowed(self, node: VirtualNode) -> None:
        if not node.policy.readable:
            raise InvalidOperation(f"{node.path()} is not readable")

    def _ensure_write_allowed(self, node: VirtualNode, *, append: bool = False) -> None:
        if not node.policy.writable:
            raise InvalidOperation(f"{node.path()} is read-only")
        if node.policy.append_only and not append:
            raise InvalidOperation(f"{node.path()} is append-only")

    def _check_version(self, node: VirtualNode, expected_version: Optional[int]) -> None:
        if expected_version is None:
            return
        if node.version != expected_version:
            raise InvalidOperation(
                f"Version mismatch for {node.path()}: expected {expected_version}, current {node.version}"
            )

    def _emit_write_event(self, node: VirtualFile, *, append: bool, event_type: str) -> None:
        if self._write_hooks:
            path = node.path()
            content = node.read(self)
            event = WriteEvent(path=str(path), content=content, version=node.version, append=append)
            for prefix, hook in self._write_hooks:
                if self._path_matches_prefix(path, prefix):
                    hook(event)

        self._emit_path_event(node.path(), event_type, node.read(self))

    def _emit_path_event(self, path: PurePosixPath, event_type: str, content: Optional[str]) -> None:
        if not self._path_hooks:
            return
        payload = PathEvent(path=str(path), event=event_type, content=content)
        for prefix, hook in self._path_hooks:
            if self._path_matches_prefix(path, prefix):
                hook(payload)

    def _path_matches_prefix(self, path: PurePosixPath, prefix: PurePosixPath) -> bool:
        if prefix == PurePosixPath("/"):
            return True
        try:
            path.relative_to(prefix)
            return True
        except ValueError:
            return False

    def _clone_policy(self, policy: NodePolicy) -> NodePolicy:
        return NodePolicy(
            readable=policy.readable,
            writable=policy.writable,
            append_only=policy.append_only,
            classification=policy.classification,
            principals=set(policy.principals),
        )

    def _find_storage_mount(
        self, path: PurePosixPath
    ) -> Optional[Tuple[PurePosixPath, StorageAdapter]]:
        matches: List[Tuple[PurePosixPath, StorageAdapter]] = []
        for prefix, adapter in self._storage_mounts.items():
            if self._path_matches_prefix(path, prefix):
                matches.append((prefix, adapter))
        if not matches:
            return None
        return max(matches, key=lambda item: len(item[0].parts))

    def _relative_storage_path(self, path: PurePosixPath, prefix: PurePosixPath) -> str:
        rel = path.relative_to(prefix)
        return rel.as_posix()

    def _persist_storage(self, node: VirtualFile, previous_version: int) -> None:
        mount = self._find_storage_mount(node.path())
        if not mount:
            return
        prefix, adapter = mount
        relative = self._relative_storage_path(node.path(), prefix)
        try:
            adapter.write(relative, node.read(self), version=previous_version)
        except ValueError as exc:
            node.version = previous_version
            raise InvalidOperation(f"Storage conflict for {node.path()}") from exc

    def _delete_storage_entry(self, node: VirtualFile) -> None:
        mount = self._find_storage_mount(node.path())
        if not mount:
            return
        prefix, adapter = mount
        relative = self._relative_storage_path(node.path(), prefix)
        adapter.delete(relative)

    def _load_storage_mount(self, prefix: PurePosixPath, adapter: StorageAdapter) -> None:
        directory = self._resolve_dir(prefix)
        directory.children.clear()
        directory._loaded = True
        for rel_path, entry in adapter.list().items():
            absolute = prefix.joinpath(PurePosixPath(rel_path))
            file_node = self._ensure_file(absolute, create=True)
            file_node.write(entry.content)
            file_node.version = entry.version

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------
    def pwd(self) -> str:
        return str(self.cwd.path())

    def cd(self, path: str | PurePosixPath) -> str:
        node = self._resolve_node(path)
        if not isinstance(node, VirtualDirectory):
            raise InvalidOperation(f"{node.path()} is not a directory")
        self._ensure_read_allowed(node)
        self.cwd = node
        return self.pwd()

    def ls(
        self,
        path: str | PurePosixPath | None = None,
        *,
        view: Optional[VisibilityView] = None,
    ) -> List[DirEntry]:
        directory = self._resolve_dir(path or self.cwd.path())
        self._ensure_read_allowed(directory)
        directory.ensure_loaded(self)
        entries: List[DirEntry] = []
        for child in directory.iter_children(self):
            if view and not view.allows(child.policy):
                continue
            entries.append(
                DirEntry(
                    name=child.name,
                    path=child.path(),
                    is_dir=isinstance(child, VirtualDirectory),
                    metadata=child.metadata,
                    policy=child.policy,
                )
            )
        entries.sort(key=lambda entry: (not entry.is_dir, entry.name))
        return entries

    def mkdir(
        self,
        path: str | PurePosixPath,
        *,
        parents: bool = False,
        exist_ok: bool = False,
    ) -> VirtualDirectory:
        normalized = self._normalize(path)
        if normalized == PurePosixPath("/"):
            return self.root
        parent = self._resolve_dir(normalized.parent or PurePosixPath("/"), create=parents)
        self._ensure_write_allowed(parent)
        name = normalized.name
        if not name:
            raise InvalidOperation("Directory name missing")
        try:
            existing = parent.get_child(name, self)
        except NodeNotFound:
            node = VirtualDirectory(name=name, parent=parent)
            parent.add_child(node)
            return node
        if not isinstance(existing, VirtualDirectory):
            raise InvalidOperation(f"{existing.path()} is not a directory")
        if not exist_ok:
            raise NodeExists(f"Directory {existing.path()} already exists")
        return existing

    def write_file(
        self,
        path: str | PurePosixPath,
        data: str,
        *,
        append: bool = False,
        expected_version: Optional[int] = None,
    ) -> VirtualFile:
        node = self._ensure_file(path, create=True)
        self._ensure_write_allowed(node, append=append)
        self._check_version(node, expected_version)
        previous_version = node.version
        node.write(data, append=append)
        node.version += 1
        self._persist_storage(node, previous_version)
        event_type = "create" if previous_version == 0 else "update"
        self._emit_write_event(node, append=append, event_type=event_type)
        return node

    def append_file(
        self,
        path: str | PurePosixPath,
        data: str,
        *,
        expected_version: Optional[int] = None,
    ) -> VirtualFile:
        return self.write_file(path, data, append=True, expected_version=expected_version)

    def read_file(self, path: str | PurePosixPath) -> str:
        node = self._ensure_file(path, create=False)
        self._ensure_read_allowed(node)
        return node.read(self)

    def touch(self, path: str | PurePosixPath) -> VirtualFile:
        node = self._ensure_file(path, create=True)
        self._ensure_write_allowed(node, append=True)
        return node

    def remove(self, path: str | PurePosixPath, *, recursive: bool = False) -> None:
        target = self._normalize(path)
        if target == PurePosixPath("/"):
            raise InvalidOperation("Cannot remove root directory")
        node = self._resolve_node(target)
        if isinstance(node, VirtualDirectory):
            node.ensure_loaded(self)
        parent = node.parent
        if parent is None:
            raise InvalidOperation("Cannot remove node without parent")
        self._ensure_write_allowed(node)
        self._ensure_write_allowed(parent)
        if isinstance(node, VirtualDirectory) and node.children and not recursive:
            raise InvalidOperation("Directory not empty; pass recursive=True")
        if isinstance(node, VirtualDirectory) and recursive:
            names = list(node.children.keys())
            for child_name in names:
                child_node = node.children.get(child_name)
                if child_node is None:
                    continue
                self.remove(child_node.path(), recursive=True)
        parent.remove_child(node.name)
        if isinstance(node, VirtualFile):
            self._delete_storage_entry(node)
            self._emit_path_event(node.path(), "delete", None)

    def walk(self, path: str | PurePosixPath | None = None) -> Iterator[Tuple[PurePosixPath, VirtualNode]]:
        start_node = self._resolve_node(path or self.cwd.path())

        def _walk(node: VirtualNode) -> Iterator[Tuple[PurePosixPath, VirtualNode]]:
            yield (node.path(), node)
            if isinstance(node, VirtualDirectory):
                node.ensure_loaded(self)
                for child in node.iter_children(self):
                    yield from _walk(child)

        return _walk(start_node)

    def iter_files(
        self,
        path: str | PurePosixPath | None = None,
        *,
        recursive: bool = True,
    ) -> Iterator[Tuple[PurePosixPath, VirtualFile]]:
        start_node = self._resolve_node(path or self.cwd.path())
        self._ensure_read_allowed(start_node)
        if isinstance(start_node, VirtualFile):
            yield (start_node.path(), start_node)
            return

        directory = self._resolve_dir(start_node.path())
        directory.ensure_loaded(self)

        def _walk_dir(dir_node: VirtualDirectory) -> Iterator[Tuple[PurePosixPath, VirtualFile]]:
            for child in dir_node.iter_children(self):
                if isinstance(child, VirtualFile):
                    yield (child.path(), child)
                elif isinstance(child, VirtualDirectory) and recursive:
                    child.ensure_loaded(self)
                    yield from _walk_dir(child)

        yield from _walk_dir(directory)

    def snapshot(self) -> VFSSnapshot:
        nodes: Dict[str, NodeSnapshot] = {}
        for path, node in self.walk("/"):
            if isinstance(node, VirtualFile):
                content = node.read(self)
            else:
                content = None
            nodes[str(path)] = NodeSnapshot(
                is_dir=isinstance(node, VirtualDirectory),
                metadata=dict(node.metadata),
                policy=self._clone_policy(node.policy),
                version=node.version,
                content=content,
            )
        storage_mounts = {str(path): adapter for path, adapter in self._storage_mounts.items()}
        return VFSSnapshot(nodes=nodes, cwd=self.cwd.path(), storage_mounts=storage_mounts)

    def restore(self, snapshot: VFSSnapshot) -> None:
        self.root = VirtualDirectory(name="")
        self.cwd = self.root
        self._storage_mounts = {PurePosixPath(path): adapter for path, adapter in snapshot.storage_mounts.items()}
        ordered = sorted(snapshot.nodes.items(), key=lambda item: len(PurePosixPath(item[0]).parts))
        for path_str, node_state in ordered:
            path = PurePosixPath(path_str)
            if path == PurePosixPath("/"):
                target = self.root
                target.metadata = dict(node_state.metadata)
                target.policy = self._clone_policy(node_state.policy)
                target.version = node_state.version
                continue
            if node_state.is_dir:
                directory = self.mkdir(path, parents=True, exist_ok=True)
                directory.metadata = dict(node_state.metadata)
                directory.policy = self._clone_policy(node_state.policy)
                directory.version = node_state.version
            else:
                file_node = self._ensure_file(path, create=True)
                file_node.metadata = dict(node_state.metadata)
                file_node.policy = self._clone_policy(node_state.policy)
                file_node.write(node_state.content or "")
                file_node.version = node_state.version
        self.cwd = self._resolve_dir(snapshot.cwd)
    def export_to_path(
        self,
        target: Path,
        *,
        source: str | PurePosixPath | None = None,
    ) -> Path:
        node = self._resolve_dir(source or self.cwd.path())
        target = Path(target)
        target.mkdir(parents=True, exist_ok=True)
        self._export_directory(node, target)
        return target

    @contextlib.contextmanager
    def materialize(self, path: str | PurePosixPath | None = None):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.export_to_path(root, source=path)
            yield root

    def _export_directory(self, node: VirtualDirectory, dest: Path) -> None:
        node.ensure_loaded(self)
        dest.mkdir(parents=True, exist_ok=True)
        for child in node.iter_children(self):
            target = dest / child.name
            if isinstance(child, VirtualDirectory):
                self._export_directory(child, target)
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(child.read(self))

    def exists(self, path: str | PurePosixPath) -> bool:
        try:
            self._resolve_node(path)
            return True
        except (NodeNotFound, InvalidOperation):
            return False

    def get_version(self, path: str | PurePosixPath) -> int:
        node = self._resolve_node(path)
        if not isinstance(node, VirtualFile):
            raise InvalidOperation(f"{node.path()} is not a file")
        return node.version

    def is_dir(self, path: str | PurePosixPath) -> bool:
        try:
            return isinstance(self._resolve_node(path), VirtualDirectory)
        except (NodeNotFound, InvalidOperation):
            return False

    def is_file(self, path: str | PurePosixPath) -> bool:
        try:
            return isinstance(self._resolve_node(path), VirtualFile)
        except (NodeNotFound, InvalidOperation):
            return False

    def mount_file(
        self,
        path: str | PurePosixPath,
        provider: ContentProvider,
        *,
        metadata: Optional[Dict[str, object]] = None,
    ) -> VirtualFile:
        node = self._ensure_file(path, create=True)
        node.set_provider(provider)
        if metadata:
            node.metadata.update(metadata)
        return node

    def mount_directory(
        self,
        path: str | PurePosixPath,
        provider: DirectoryProvider,
        *,
        metadata: Optional[Dict[str, object]] = None,
    ) -> VirtualDirectory:
        node = self.mkdir(path, parents=True, exist_ok=True)
        node.loader = provider
        node._loaded = False  # allow reload
        if metadata:
            node.metadata.update(metadata)
        return node

    def mount_storage(
        self,
        path: str | PurePosixPath,
        adapter: StorageAdapter,
        *,
        policy: Optional[NodePolicy] = None,
    ) -> VirtualDirectory:
        normalized = self._normalize(path)
        directory = self.mkdir(normalized, parents=True, exist_ok=True)
        if policy is not None:
            directory.policy = policy
        self._storage_mounts[normalized] = adapter
        self._load_storage_mount(normalized, adapter)
        return directory

    def sync_storage(self, path: str | PurePosixPath) -> None:
        normalized = self._normalize(path)
        adapter = self._storage_mounts.get(normalized)
        if adapter is None:
            raise InvalidOperation(f"No storage mount at {normalized}")
        self._load_storage_mount(normalized, adapter)

    def register_write_hook(self, prefix: str | PurePosixPath, hook: WriteHook) -> None:
        normalized = self._normalize(prefix)
        self._write_hooks.append((normalized, hook))

    def register_path_hook(self, prefix: str | PurePosixPath, hook: PathHook) -> None:
        normalized = self._normalize(prefix)
        self._path_hooks.append((normalized, hook))

    def set_policy(self, path: str | PurePosixPath, policy: NodePolicy) -> None:
        node = self._resolve_node(path)
        node.policy = policy

    def get_policy(self, path: str | PurePosixPath) -> NodePolicy:
        node = self._resolve_node(path)
        return node.policy

    def get_node(self, path: str | PurePosixPath) -> VirtualNode:
        return self._resolve_node(path)

    def tree(
        self,
        path: str | PurePosixPath | None = None,
        *,
        view: Optional[VisibilityView] = None,
    ) -> str:
        root_dir = self._resolve_dir(path or self.cwd.path())
        self._ensure_read_allowed(root_dir)
        lines: List[str] = []

        def render(directory: VirtualDirectory, prefix: str = "") -> None:
            entries = sorted(
                (
                    child
                    for child in directory.iter_children(self)
                    if not view or view.allows(child.policy)
                ),
                key=lambda node: (not isinstance(node, VirtualDirectory), node.name),
            )
            for idx, node in enumerate(entries):
                connector = "└──" if idx == len(entries) - 1 else "├──"
                lines.append(f"{prefix}{connector} {node.name}/" if isinstance(node, VirtualDirectory) else f"{prefix}{connector} {node.name}")
                if isinstance(node, VirtualDirectory):
                    extension = "    " if idx == len(entries) - 1 else "│   "
                    render(node, prefix + extension)

        render(root_dir)
        header = str(root_dir.path())
        return "\n".join([header] + lines)


__all__ = ["VirtualFileSystem", "DirEntry"]
