"""Search-oriented commands."""

from __future__ import annotations

import fnmatch
import re
from typing import Iterable, TYPE_CHECKING

from ..common import CommandResult
from ..registry import COMMAND_REGISTRY
from ...exceptions import InvalidOperation, NodeNotFound
from ...nodes import VirtualDirectory, VirtualNode

if TYPE_CHECKING:  # pragma: no cover - typing helper
    from ..core import SandboxShell


@COMMAND_REGISTRY.command("grep", description="Search files (non-recursive)")
def grep(shell: "SandboxShell", args: list[str]) -> CommandResult:
    if not args:
        return CommandResult(stderr="grep expects a pattern", exit_code=2)
    recursive = False
    regex = False
    ignore_case = False
    show_numbers = False
    paths: list[str] = []
    pattern: str | None = None
    idx = 0
    while idx < len(args):
        token = args[idx]
        if token in ("-r", "-R", "--recursive"):
            recursive = True
            idx += 1
            continue
        if token in ("-i", "--ignore-case"):
            ignore_case = True
            idx += 1
            continue
        if token in ("-n", "--line-number"):
            show_numbers = True
            idx += 1
            continue
        if token in ("-e", "--regex"):
            regex = True
            idx += 1
            continue
        if pattern is None:
            pattern = token
        else:
            paths.append(token)
        idx += 1
    if pattern is None:
        return CommandResult(stderr="Missing pattern", exit_code=2)
    if not paths:
        paths = [shell.vfs.pwd()]
    for target in paths:
        shell._ensure_visible_path(target)
    output = _search(
        shell,
        pattern,
        paths,
        recursive=recursive,
        regex=regex,
        ignore_case=ignore_case,
        show_numbers=show_numbers,
    )
    return CommandResult(stdout="\n".join(output))


@COMMAND_REGISTRY.command("rg", description="Search files recursively")
def rg(shell: "SandboxShell", args: list[str]) -> CommandResult:
    return grep(shell, ["-r", *args])


def _search(
    shell: "SandboxShell",
    pattern: str,
    paths: Iterable[str],
    *,
    recursive: bool,
    regex: bool,
    ignore_case: bool,
    show_numbers: bool,
) -> list[str]:
    results: list[str] = []
    flags = re.MULTILINE
    if ignore_case:
        flags |= re.IGNORECASE
    compiled = re.compile(pattern, flags) if regex else None
    lowered = pattern.lower() if ignore_case and not regex else None
    for target in paths:
        for file_path, file_node in shell.vfs.iter_files(target, recursive=recursive):
            if shell.view and not shell.view.allows(file_node.policy):
                continue
            text = file_node.read(shell.vfs)
            lines = text.splitlines()
            for idx, line in enumerate(lines, start=1):
                matched = False
                if regex:
                    if compiled and compiled.search(line):
                        matched = True
                elif ignore_case:
                    if lowered and lowered in line.lower():
                        matched = True
                else:
                    if pattern in line:
                        matched = True
                if matched:
                    prefix = f"{file_path}:{idx}:" if show_numbers else f"{file_path}:"
                    results.append(f"{prefix}{line}")
    return results


@COMMAND_REGISTRY.command(
    "find", description="Search for files in a directory hierarchy"
)
def find(shell: "SandboxShell", args: list[str]) -> CommandResult:
    paths: list[str] = []
    name_pattern: str | None = None
    type_filter: str | None = None

    idx = 0
    while idx < len(args):
        arg = args[idx]
        if arg == "-name":
            if idx + 1 >= len(args):
                return CommandResult(
                    stderr="find: missing argument to `-name'",
                    exit_code=1,
                )
            name_pattern = args[idx + 1]
            idx += 2
        elif arg == "-type":
            if idx + 1 >= len(args):
                return CommandResult(
                    stderr="find: missing argument to `-type'",
                    exit_code=1,
                )
            candidate = args[idx + 1]
            if candidate not in ("f", "d"):
                return CommandResult(
                    stderr=f"find: unknown argument to -type: {candidate}",
                    exit_code=1,
                )
            type_filter = candidate
            idx += 2
        elif arg.startswith("-"):
            return CommandResult(stderr=f"find: unknown predicate `{arg}'", exit_code=1)
        else:
            paths.append(arg)
            idx += 1

    if not paths:
        paths = [shell.vfs.pwd()]

    def walk_visible(node: VirtualNode):
        if shell.view and not shell.view.allows(node.policy):
            return
        yield (node.path(), node)
        if isinstance(node, VirtualDirectory):
            node.ensure_loaded(shell.vfs)
            for child in node.iter_children(shell.vfs):
                yield from walk_visible(child)

    results: list[str] = []
    for start_path in paths:
        shell._ensure_visible_path(start_path)
        try:
            start_node = shell.vfs.get_node(start_path)
        except (NodeNotFound, InvalidOperation):
            return CommandResult(
                stderr=f"find: `{start_path}': No such file or directory",
                exit_code=1,
            )

        for path, node in walk_visible(start_node):
            if type_filter:
                is_dir = isinstance(node, VirtualDirectory)
                if type_filter == "f" and is_dir:
                    continue
                if type_filter == "d" and not is_dir:
                    continue

            if name_pattern and not fnmatch.fnmatch(node.name, name_pattern):
                continue

            results.append(str(path))

    return CommandResult(stdout="\n".join(results))
