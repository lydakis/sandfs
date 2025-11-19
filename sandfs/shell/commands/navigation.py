"""Navigation-oriented commands."""

from __future__ import annotations

from typing import TYPE_CHECKING

from ..common import CommandResult
from ..registry import COMMAND_REGISTRY
from ...vfs import DirEntry

if TYPE_CHECKING:  # pragma: no cover - typing helper
    from ..core import SandboxShell


def _format_ls(entries: list[DirEntry], *, long_format: bool) -> str:
    if not entries:
        return ""
    if long_format:
        return "\n".join(f"{'d' if entry.is_dir else '-'} {entry.path}" for entry in entries)
    return "  ".join(f"{entry.name}/" if entry.is_dir else entry.name for entry in entries)


@COMMAND_REGISTRY.command("pwd", description="Print working directory")
def pwd(shell: "SandboxShell", _: list[str]) -> CommandResult:
    return CommandResult(stdout=shell.vfs.pwd())


@COMMAND_REGISTRY.command("cd", description="Change directory")
def cd(shell: "SandboxShell", args: list[str]) -> CommandResult:
    if len(args) != 1:
        return CommandResult(stderr="cd expects exactly one path", exit_code=2)
    shell._ensure_visible_path(args[0])
    new_path = shell.vfs.cd(args[0])
    return CommandResult(stdout=new_path)


@COMMAND_REGISTRY.command("ls", description="List directory contents")
def ls(shell: "SandboxShell", args: list[str]) -> CommandResult:
    long = False
    targets: list[str] = []
    for arg in args:
        if arg in ("-l", "--long"):
            long = True
        elif arg.startswith("-"):
            from ..host import run_host_process

            return run_host_process(shell, ["ls", *args], None)
        else:
            targets.append(arg)
    if not targets:
        targets = [shell.vfs.pwd()]
    blocks: list[str] = []
    for idx, target in enumerate(targets):
        shell._ensure_visible_path(target)
        entries = shell.vfs.ls(target, view=shell.view)
        if len(targets) > 1:
            blocks.append(f"{target}:")
        blocks.append(_format_ls(entries, long_format=long))
        if idx < len(targets) - 1:
            blocks.append("")
    return CommandResult(stdout="\n".join(filter(None, blocks)))


@COMMAND_REGISTRY.command("tree", description="Render tree view")
def tree(shell: "SandboxShell", args: list[str]) -> CommandResult:
    target = args[0] if args else None
    if target:
        shell._ensure_visible_path(target)
    return CommandResult(stdout=shell.vfs.tree(target, view=shell.view))
