"""File manipulation commands."""

from __future__ import annotations

from typing import TYPE_CHECKING

from ..common import CommandResult
from ..registry import COMMAND_REGISTRY
from ...exceptions import InvalidOperation, NodeNotFound

if TYPE_CHECKING:  # pragma: no cover - typing helper
    from ..core import SandboxShell


@COMMAND_REGISTRY.command("cat", description="Print file contents")
def cat(shell: "SandboxShell", args: list[str]) -> CommandResult:
    if not args:
        return CommandResult(stderr="cat expects at least one file", exit_code=2)
    blobs = []
    for path in args:
        shell._ensure_visible_path(path)
        blobs.append(shell.vfs.read_file(path))
    return CommandResult(stdout="".join(blobs))


@COMMAND_REGISTRY.command("touch", description="Create empty file")
def touch(shell: "SandboxShell", args: list[str]) -> CommandResult:
    if not args:
        return CommandResult(stderr="touch expects at least one file", exit_code=2)
    for path in args:
        shell._ensure_visible_path(path)
        shell.vfs.touch(path)
    return CommandResult()


@COMMAND_REGISTRY.command("mkdir", description="Create directories")
def mkdir(shell: "SandboxShell", args: list[str]) -> CommandResult:
    parents = False
    paths: list[str] = []
    for arg in args:
        if arg in ("-p", "--parents"):
            parents = True
        else:
            paths.append(arg)
    if not paths:
        return CommandResult(stderr="mkdir expects a path", exit_code=2)
    for path in paths:
        shell._ensure_visible_path(path)
        shell.vfs.mkdir(path, parents=parents, exist_ok=parents)
    return CommandResult()


@COMMAND_REGISTRY.command("rm", description="Remove files or directories")
def rm(shell: "SandboxShell", args: list[str]) -> CommandResult:
    recursive = False
    targets: list[str] = []
    for arg in args:
        if arg in ("-r", "-rf", "-R", "--recursive"):
            recursive = True
        else:
            targets.append(arg)
    if not targets:
        return CommandResult(stderr="rm expects a target", exit_code=2)
    for target in targets:
        shell._ensure_visible_path(target)
        shell.vfs.remove(target, recursive=recursive)
    return CommandResult()


@COMMAND_REGISTRY.command("cp", description="Copy files and directories")
def cp(shell: "SandboxShell", args: list[str]) -> CommandResult:
    recursive = False
    operands: list[str] = []
    for arg in args:
        if arg in ("-r", "-R", "--recursive"):
            recursive = True
        else:
            operands.append(arg)
    if len(operands) != 2:
        return CommandResult(stderr="cp expects a source and destination", exit_code=2)
    source, dest = operands
    shell._ensure_visible_path(source)
    shell._ensure_visible_path(dest)
    try:
        shell.vfs.copy(source, dest, recursive=recursive)
    except (InvalidOperation, NodeNotFound) as exc:
        return CommandResult(stderr=str(exc), exit_code=1)
    return CommandResult()


@COMMAND_REGISTRY.command("mv", description="Move or rename files and directories")
def mv(shell: "SandboxShell", args: list[str]) -> CommandResult:
    if len(args) != 2:
        return CommandResult(stderr="mv expects a source and destination", exit_code=2)
    source, dest = args
    shell._ensure_visible_path(source)
    shell._ensure_visible_path(dest)
    try:
        shell.vfs.move(source, dest)
    except (InvalidOperation, NodeNotFound) as exc:
        return CommandResult(stderr=str(exc), exit_code=1)
    return CommandResult()
