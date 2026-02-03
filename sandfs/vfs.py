"""Virtual filesystem implementation."""

from __future__ import annotations

import contextlib
import fnmatch
import re
import tempfile
import time
from collections.abc import Iterable, Iterator, Mapping
from dataclasses import dataclass
from pathlib import Path, PurePosixPath

from .adapters import StorageAdapter
from .exceptions import InvalidOperation, NodeExists, NodeNotFound
from .hooks import WriteEvent, WriteHook
from .integrations import PathEvent, PathHook
from .nodes import VirtualDirectory, VirtualFile, VirtualNode
from .policies import NodePolicy, VisibilityView
from .providers import ContentProvider, DirectoryProvider, NodeContext, ProvidedNode
from .search import FullTextIndex, SearchQuery, SearchResult


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


@dataclass(frozen=True)
class SearchViewContext:
    query: SearchQuery
    view: VisibilityView | None


class VirtualFileSystem:
    """In-memory filesystem that supports dynamic nodes."""

    def __init__(self) -> None:
        self.root = VirtualDirectory(name="")
        self.cwd = self.root
        self._write_hooks: list[tuple[PurePosixPath, WriteHook]] = []
        self._storage_mounts: dict[PurePosixPath, StorageAdapter] = {}
        self._path_hooks: list[tuple[PurePosixPath, PathHook]] = []
        self._full_text_index: FullTextIndex | None = None
        self._search_view_prefix: PurePosixPath | None = None
        self._search_view_context: SearchViewContext | None = None

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

    def _emit_write_event(self, node: VirtualFile, *, append: bool, event_type: str) -> None:
        if self._write_hooks:
            path = node.path()
            content = node.read(self)
            event = WriteEvent(path=str(path), content=content, version=node.version, append=append)
            for prefix, hook in self._write_hooks:
                if self._path_matches_prefix(path, prefix):
                    hook(event)

        self._emit_path_event(node.path(), event_type, node.read(self))

    def _emit_path_event(self, path: PurePosixPath, event_type: str, content: str | None) -> None:
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
    ) -> tuple[PurePosixPath, StorageAdapter] | None:
        matches: list[tuple[PurePosixPath, StorageAdapter]] = []
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

    def _rebuild_index(self) -> None:
        if self._full_text_index is None:
            return
        entries = []
        skip_prefixes = [self._search_view_prefix] if self._search_view_prefix else None
        for path, file_node in self.iter_files("/", recursive=True, skip_prefixes=skip_prefixes):
            try:
                content = file_node.read(self)
            except InvalidOperation:
                continue
            entries.append((path, content))
        self._full_text_index.build(entries)

    def _index_file(self, node: VirtualFile) -> None:
        if self._full_text_index is None:
            return
        try:
            self._full_text_index.index_file(node.path(), node.read(self))
        except InvalidOperation:
            return

    def _remove_index_entry(self, path: PurePosixPath) -> None:
        if self._full_text_index is None:
            return
        self._full_text_index.remove_file(path)

    def enable_full_text_index(self, index: FullTextIndex | None = None) -> FullTextIndex:
        self._full_text_index = index or FullTextIndex()
        self._rebuild_index()
        return self._full_text_index

    def enable_search_view(self, prefix: str | PurePosixPath = "/@search") -> None:
        normalized = self._normalize(prefix)
        self._search_view_prefix = normalized

        def provider(_: NodeContext) -> Mapping[str, ProvidedNode]:
            return self._search_view_provider()

        self.mount_directory(normalized, provider)

    @contextlib.contextmanager
    def search_view_context(
        self,
        query: SearchQuery,
        *,
        view: VisibilityView | None = None,
    ) -> Iterator[None]:
        previous = self._search_view_context
        self._search_view_context = SearchViewContext(query=query, view=view)
        try:
            yield
        finally:
            self._search_view_context = previous

    def _reset_directory(self, path: str | PurePosixPath) -> None:
        directory = self._resolve_dir(path)
        directory.children.clear()
        directory._loaded = False

    def _search_view_provider(self) -> Mapping[str, ProvidedNode]:
        if self._search_view_context is None:
            return {}
        query = self._search_view_context.query
        view = self._search_view_context.view
        results = self.search(query, view=view)
        return self._build_search_tree(results)

    def _build_search_tree(self, results: Iterable[SearchResult]) -> Mapping[str, ProvidedNode]:
        files: dict[PurePosixPath, list[SearchResult]] = {}
        for result in results:
            files.setdefault(result.path, []).append(result)
        root: dict[str, ProvidedNode] = {}

        for path, matches in files.items():
            parts = [part for part in path.parts if part not in ("/", "")]
            cursor = root
            for part in parts[:-1]:
                node = cursor.get(part)
                if node is None:
                    node = ProvidedNode.directory(children={})
                    cursor[part] = node
                if node.children is None:
                    node.children = {}
                cursor = node.children
            content_lines = [
                f"{path}:{match.line_no}:{match.line_text}" for match in matches
            ]
            cursor[parts[-1]] = ProvidedNode.file(content="\n".join(content_lines))
        return root

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
            if view and not view.allows_node(child):
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

    def search(
        self,
        query: SearchQuery,
        *,
        view: VisibilityView | None = None,
    ) -> list[SearchResult]:
        if self._full_text_index is not None:
            results = self._full_text_index.search(query)
            if view is None:
                if self._search_view_prefix is None:
                    return results
                return [
                    result
                    for result in results
                    if not result.path.is_relative_to(self._search_view_prefix)
                ]
            filtered: list[SearchResult] = []
            for result in results:
                if self._search_view_prefix and result.path.is_relative_to(
                    self._search_view_prefix
                ):
                    continue
                try:
                    node = self.get_node(result.path)
                except (NodeNotFound, InvalidOperation):
                    continue
                if view.allows_node(node):
                    filtered.append(result)
            return filtered

        results: list[SearchResult] = []
        try:
            files = self.iter_files(
                query.path_prefix or "/",
                recursive=True,
                skip_prefixes=[self._search_view_prefix] if self._search_view_prefix else None,
            )
        except (NodeNotFound, InvalidOperation):
            return results
        flags = re.MULTILINE | (re.IGNORECASE if query.ignore_case else 0)
        compiled = re.compile(query.query, flags) if query.regex else None
        lowered = query.query.lower() if query.ignore_case and not query.regex else None
        for path, node in files:
            if self._search_view_prefix and path.is_relative_to(self._search_view_prefix):
                continue
            if view and not view.allows_node(node):
                continue
            text = node.read(self)
            for idx, line in enumerate(text.splitlines(), start=1):
                matched = False
                if query.regex:
                    if compiled and compiled.search(line):
                        matched = True
                elif query.ignore_case:
                    if lowered and lowered in line.lower():
                        matched = True
                else:
                    if query.query in line:
                        matched = True
                if matched:
                    results.append(SearchResult(path=path, line_no=idx, line_text=line))
        return results

    def glob(
        self,
        pattern: str,
        *,
        cwd: str | PurePosixPath | None = None,
        view: VisibilityView | None = None,
    ) -> list[str]:
        if not pattern:
            return []
        cwd_path = self._normalize(cwd) if cwd is not None else self.cwd.path()
        if "/" not in pattern and not pattern.startswith("/"):
            entries = self.ls(cwd_path, view=view)
            return [
                str(entry.path)
                for entry in entries
                if fnmatch.fnmatchcase(entry.name, pattern)
            ]

        if pattern.startswith("/"):
            pattern_path = PurePosixPath(pattern)
        else:
            pattern_path = cwd_path.joinpath(PurePosixPath(pattern))

        matches: list[str] = []
        for path, node in self.walk("/"):
            if view and not view.allows_node(node):
                continue
            if fnmatch.fnmatchcase(str(path), str(pattern_path)):
                matches.append(str(path))
        return sorted(matches)

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
        self._persist_storage(node, previous_version)
        self._index_file(node)
        event_type = "create" if previous_version == 0 else "update"
        self._emit_write_event(node, append=append, event_type=event_type)
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
            self._delete_storage_entry(node)
            self._remove_index_entry(node.path())
            self._emit_path_event(node.path(), "delete", None)

    def move(self, source: str | PurePosixPath, target: str | PurePosixPath) -> None:
        src_path = self._normalize(source)
        if src_path == PurePosixPath("/"):
            raise InvalidOperation("Cannot move root directory")
        node = self._resolve_node(src_path)
        original_path = node.path()
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
        if isinstance(node, VirtualFile):
            self._remove_index_entry(original_path)
            self._index_file(node)
        else:
            self._rebuild_index()

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
        if isinstance(clone, VirtualFile):
            self._index_file(clone)
        else:
            self._rebuild_index()

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
        skip_prefixes: Iterable[PurePosixPath] | None = None,
    ) -> Iterator[tuple[PurePosixPath, VirtualFile]]:
        start_node = self._resolve_node(path or self.cwd.path())
        self._ensure_read_allowed(start_node)
        prefixes = list(skip_prefixes or [])

        def should_skip(target: PurePosixPath) -> bool:
            return any(target.is_relative_to(prefix) for prefix in prefixes)

        if isinstance(start_node, VirtualFile):
            if not should_skip(start_node.path()):
                yield (start_node.path(), start_node)
            return

        directory = self._resolve_dir(start_node.path())
        if should_skip(directory.path()):
            return
        directory.ensure_loaded(self)

        def _walk_dir(dir_node: VirtualDirectory) -> Iterator[tuple[PurePosixPath, VirtualFile]]:
            if should_skip(dir_node.path()):
                return
            for child in dir_node.iter_children(self):
                if isinstance(child, VirtualFile):
                    if not should_skip(child.path()):
                        yield (child.path(), child)
                elif isinstance(child, VirtualDirectory) and recursive:
                    if should_skip(child.path()):
                        continue
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
        storage_mounts = {str(path): adapter for path, adapter in self._storage_mounts.items()}
        return VFSSnapshot(nodes=nodes, cwd=self.cwd.path(), storage_mounts=storage_mounts)

    def restore(self, snapshot: VFSSnapshot) -> None:
        self.root = VirtualDirectory(name="")
        self.cwd = self.root
        self._storage_mounts = {
            PurePosixPath(path): adapter for path, adapter in snapshot.storage_mounts.items()
        }
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
        self._rebuild_index()

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
        normalized = self._normalize(path)
        directory = self.mkdir(normalized, parents=True, exist_ok=True)
        if policy is not None:
            directory.policy = policy
        self._storage_mounts[normalized] = adapter
        self._load_storage_mount(normalized, adapter)
        self._rebuild_index()
        return directory

    def sync_storage(self, path: str | PurePosixPath) -> None:
        normalized = self._normalize(path)
        adapter = self._storage_mounts.get(normalized)
        if adapter is None:
            raise InvalidOperation(f"No storage mount at {normalized}")
        self._load_storage_mount(normalized, adapter)
        self._rebuild_index()

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
                    if not view or view.allows_node(child)
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
