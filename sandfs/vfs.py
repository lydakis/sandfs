"""Virtual filesystem implementation."""

from __future__ import annotations

import contextlib
import tempfile
import time
from collections.abc import Iterator
from dataclasses import dataclass
from pathlib import Path, PurePosixPath

from .adapters import StorageAdapter
from .exceptions import InvalidOperation, NodeExists, NodeNotFound
from .hooks import WriteHook
from .integrations import PathHook
from .managers import HookManager, StorageManager
from .nodes import VirtualDirectory, VirtualFile, VirtualNode
from .path_utils import PathResolverMixin
from .policies import NodePolicy, VisibilityView
from .providers import ContentProvider, DirectoryProvider


@dataclass
class DirEntry:
    name: str
    path: PurePosixPath
    is_dir: bool
    metadata: dict[str, object]
    policy: NodePolicy


@dataclass
class NodeSnapshot:
    is_dir: bool
    metadata: dict[str, object]
    policy: NodePolicy
    version: int
    created_at: float
    modified_at: float
    content: str | None = None


@dataclass
class VFSSnapshot:
    nodes: dict[str, NodeSnapshot]
    cwd: PurePosixPath
    storage_mounts: dict[str, StorageAdapter]


class VirtualFileSystem(PathResolverMixin):
    """In-memory filesystem that supports dynamic nodes."""

    def __init__(self) -> None:
        self.root = VirtualDirectory(name="")
        self.cwd = self.root
        self.hooks = HookManager(self)
        self.storage = StorageManager(self)

    def _ensure_read_allowed(self, node: VirtualNode) -> None:
        if not node.policy.readable:
            raise InvalidOperation(f"{node.path()} is not readable")

    def _ensure_write_allowed(self, node: VirtualNode, *, append: bool = False) -> None:
        if not node.policy.writable:
            raise InvalidOperation(f"{node.path()} is read-only")
        if node.policy.append_only and not append:
            raise InvalidOperation(f"{node.path()} is append-only")

    def _check_version(self, node: VirtualNode, expected_version: int | None) -> None:
        if expected_version is None:
            return
        if node.version != expected_version:
            raise InvalidOperation(
                (
                    f"Version mismatch for {node.path()}: "
                    f"expected {expected_version}, current {node.version}"
                )
            )

    def _clone_policy(self, policy: NodePolicy) -> NodePolicy:
        return NodePolicy(
            readable=policy.readable,
            writable=policy.writable,
            append_only=policy.append_only,
            classification=policy.classification,
            principals=set(policy.principals),
        )

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
        view: VisibilityView | None = None,
    ) -> list[DirEntry]:
        directory = self._resolve_dir(path or self.cwd.path())
        self._ensure_read_allowed(directory)
        directory.ensure_loaded(self)
        entries: list[DirEntry] = []
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
        expected_version: int | None = None,
    ) -> VirtualFile:
        node = self._ensure_file(path, create=True)
        self._ensure_write_allowed(node, append=append)
        self._check_version(node, expected_version)
        previous_version = node.version
        node.write(data, append=append)
        node.version += 1
        node.modified_at = time.time()
        self.storage.persist(node, previous_version)
        event_type = "create" if previous_version == 0 else "update"
        self.hooks.emit_write_event(node, append=append, event_type=event_type)
        return node

    def append_file(
        self,
        path: str | PurePosixPath,
        data: str,
        *,
        expected_version: int | None = None,
    ) -> VirtualFile:
        return self.write_file(path, data, append=True, expected_version=expected_version)

    def read_file(self, path: str | PurePosixPath) -> str:
        node = self._ensure_file(path, create=False)
        self._ensure_read_allowed(node)
        return node.read(self)

    def touch(self, path: str | PurePosixPath) -> VirtualFile:
        node = self._ensure_file(path, create=True)
        self._ensure_write_allowed(node, append=True)
        node.modified_at = time.time()
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
            self.storage.delete_entry(node)
            self.hooks.emit_path_event(node.path(), "delete", None)

    def move(self, source: str | PurePosixPath, target: str | PurePosixPath) -> None:
        src_path = self._normalize(source)
        if src_path == PurePosixPath("/"):
            raise InvalidOperation("Cannot move root directory")
        node = self._resolve_node(src_path)
        parent = node.parent
        if parent is None:
            raise InvalidOperation("Cannot move node without parent")
        self._ensure_write_allowed(node)
        self._ensure_write_allowed(parent)

        dest_path = self._normalize(target)
        dest_parent: VirtualDirectory
        dest_name: str

        try:
            dest_node = self._resolve_node(dest_path)
        except (NodeNotFound, InvalidOperation):
            dest_parent = self._resolve_dir(dest_path.parent or PurePosixPath("/"))
            self._ensure_write_allowed(dest_parent)
            dest_name = dest_path.name
            if not dest_name:
                raise InvalidOperation("Destination path missing file name") from None
        else:
            if isinstance(dest_node, VirtualDirectory):
                self._ensure_write_allowed(dest_node)
                dest_parent = dest_node
                dest_name = node.name
            else:
                raise InvalidOperation(f"Destination {dest_path} already exists")

        dest_parent_path = dest_parent.path()
        node_path = node.path()
        if isinstance(node, VirtualDirectory):
            try:
                dest_parent_path.relative_to(node_path)
                raise InvalidOperation("Cannot move a directory inside itself")
            except ValueError:
                pass
            if dest_parent_path == node_path:
                raise InvalidOperation("Destination directory matches source directory")

        if dest_parent is parent and dest_name == node.name:
            return

        if dest_parent is node:
            raise InvalidOperation("Cannot move a node into itself")

        parent.remove_child(node.name)
        node.name = dest_name
        dest_parent.add_child(node)

    def copy(
        self,
        source: str | PurePosixPath,
        target: str | PurePosixPath,
        *,
        recursive: bool = False,
    ) -> None:
        src_path = self._normalize(source)
        node = self._resolve_node(src_path)
        if isinstance(node, VirtualDirectory) and not recursive:
            raise InvalidOperation("Source is a directory; pass recursive=True")
        self._ensure_read_allowed(node)

        dest_path = self._normalize(target)
        dest_parent: VirtualDirectory
        dest_name: str

        try:
            dest_node = self._resolve_node(dest_path)
        except (NodeNotFound, InvalidOperation):
            dest_parent = self._resolve_dir(dest_path.parent or PurePosixPath("/"))
            self._ensure_write_allowed(dest_parent)
            dest_name = dest_path.name
            if not dest_name:
                raise InvalidOperation("Destination path missing file name") from None
        else:
            if isinstance(dest_node, VirtualDirectory):
                self._ensure_write_allowed(dest_node)
                dest_parent = dest_node
                dest_name = node.name
            else:
                parent_node = dest_node.parent
                if parent_node is None:
                    raise InvalidOperation("Cannot overwrite root directory")
                self._ensure_write_allowed(parent_node)
                self._ensure_write_allowed(dest_node)
                parent_node.remove_child(dest_node.name)
                dest_name = dest_node.name
                dest_parent = parent_node

        if isinstance(node, VirtualDirectory):
            dest_parent_path = dest_parent.path()
            target_path = dest_parent_path.joinpath(dest_name)
            try:
                target_path.relative_to(node.path())
                raise InvalidOperation("Cannot copy a directory inside itself")
            except ValueError:
                pass

        clone = self._clone_node(node, recursive=recursive)
        clone.name = dest_name
        dest_parent.add_child(clone)

    def _clone_node(self, node: VirtualNode, *, recursive: bool) -> VirtualNode:
        if isinstance(node, VirtualFile):
            file_clone = VirtualFile(name=node.name, metadata=dict(node.metadata))
            file_clone.policy = self._clone_policy(node.policy)
            file_clone.write(node.read(self))
            # Timestamps are left as-is since the new node is initialized with current time.
            return file_clone
        if isinstance(node, VirtualDirectory):
            node.ensure_loaded(self)
            directory_clone = VirtualDirectory(name=node.name, metadata=dict(node.metadata))
            directory_clone.policy = self._clone_policy(node.policy)
            for child in node.iter_children(self):
                child_clone = self._clone_node(child, recursive=recursive)
                directory_clone.add_child(child_clone)
            return directory_clone
        raise InvalidOperation("Unsupported node type for copy")

    def walk(
        self,
        path: str | PurePosixPath | None = None,
    ) -> Iterator[tuple[PurePosixPath, VirtualNode]]:
        start_node = self._resolve_node(path or self.cwd.path())

        def _walk(node: VirtualNode) -> Iterator[tuple[PurePosixPath, VirtualNode]]:
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
    ) -> Iterator[tuple[PurePosixPath, VirtualFile]]:
        start_node = self._resolve_node(path or self.cwd.path())
        self._ensure_read_allowed(start_node)
        if isinstance(start_node, VirtualFile):
            yield (start_node.path(), start_node)
            return

        directory = self._resolve_dir(start_node.path())
        directory.ensure_loaded(self)

        def _walk_dir(dir_node: VirtualDirectory) -> Iterator[tuple[PurePosixPath, VirtualFile]]:
            for child in dir_node.iter_children(self):
                if isinstance(child, VirtualFile):
                    yield (child.path(), child)
                elif isinstance(child, VirtualDirectory) and recursive:
                    child.ensure_loaded(self)
                    yield from _walk_dir(child)

        yield from _walk_dir(directory)

    def snapshot(self) -> VFSSnapshot:
        nodes: dict[str, NodeSnapshot] = {}
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
                created_at=node.created_at,
                modified_at=node.modified_at,
                content=content,
            )
        storage_mounts = {str(path): adapter for path, adapter in self.storage.mounts.items()}
        return VFSSnapshot(nodes=nodes, cwd=self.cwd.path(), storage_mounts=storage_mounts)

    def restore(self, snapshot: VFSSnapshot) -> None:
        self.root = VirtualDirectory(name="")
        self.cwd = self.root
        self.storage.replace_mounts(
            {PurePosixPath(path): adapter for path, adapter in snapshot.storage_mounts.items()}
        )
        ordered = sorted(snapshot.nodes.items(), key=lambda item: len(PurePosixPath(item[0]).parts))
        for path_str, node_state in ordered:
            path = PurePosixPath(path_str)
            if path == PurePosixPath("/"):
                target = self.root
                target.metadata = dict(node_state.metadata)
                target.policy = self._clone_policy(node_state.policy)
                target.version = node_state.version
                target.created_at = node_state.created_at
                target.modified_at = node_state.modified_at
                continue
            if node_state.is_dir:
                directory = self.mkdir(path, parents=True, exist_ok=True)
                directory.metadata = dict(node_state.metadata)
                directory.policy = self._clone_policy(node_state.policy)
                directory.version = node_state.version
                directory.created_at = node_state.created_at
                directory.modified_at = node_state.modified_at
            else:
                file_node = self._ensure_file(path, create=True)
                file_node.metadata = dict(node_state.metadata)
                file_node.policy = self._clone_policy(node_state.policy)
                file_node.write(node_state.content or "")
                file_node.version = node_state.version
                file_node.created_at = node_state.created_at
                file_node.modified_at = node_state.modified_at
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
    def materialize(
        self,
        path: str | PurePosixPath | None = None,
    ) -> Iterator[Path]:
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
            elif isinstance(child, VirtualFile):
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(child.read(self))
            else:
                raise InvalidOperation(f"Unsupported node type during export: {type(child)!r}")

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
        metadata: dict[str, object] | None = None,
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
        metadata: dict[str, object] | None = None,
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
        policy: NodePolicy | None = None,
    ) -> VirtualDirectory:
        return self.storage.mount(path, adapter, policy=policy)

    def sync_storage(self, path: str | PurePosixPath) -> None:
        self.storage.sync(path)

    def register_write_hook(self, prefix: str | PurePosixPath, hook: WriteHook) -> None:
        self.hooks.register_write_hook(prefix, hook)

    def register_path_hook(self, prefix: str | PurePosixPath, hook: PathHook) -> None:
        self.hooks.register_path_hook(prefix, hook)

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
        view: VisibilityView | None = None,
    ) -> str:
        root_dir = self._resolve_dir(path or self.cwd.path())
        self._ensure_read_allowed(root_dir)
        lines: list[str] = []

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
                if isinstance(node, VirtualDirectory):
                    label = f"{prefix}{connector} {node.name}/"
                else:
                    label = f"{prefix}{connector} {node.name}"
                lines.append(label)
                if isinstance(node, VirtualDirectory):
                    extension = "    " if idx == len(entries) - 1 else "│   "
                    render(node, prefix + extension)

        render(root_dir)
        header = str(root_dir.path())
        return "\n".join([header] + lines)


__all__ = ["VirtualFileSystem", "DirEntry"]
