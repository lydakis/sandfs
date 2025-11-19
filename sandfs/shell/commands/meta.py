"""Meta commands for shell introspection."""

from __future__ import annotations

from datetime import datetime
from typing import TYPE_CHECKING

from ..common import CommandResult
from ..registry import COMMAND_REGISTRY
from ...exceptions import InvalidOperation, NodeNotFound
from ...nodes import VirtualFile

if TYPE_CHECKING:  # pragma: no cover - typing helper
    from ..core import SandboxShell


@COMMAND_REGISTRY.command("help", description="Show available commands")
def help(shell: "SandboxShell", _: list[str]) -> CommandResult:  # noqa: A001
    lines = ["Available commands:"]
    for name in shell.available_commands():
        desc = shell.command_docs.get(name, "")
        if desc:
            lines.append(f"  {name} - {desc}")
        else:
            lines.append(f"  {name}")
    lines.append("Use host <cmd> (or run unknown commands directly) for full GNU tools.")
    return CommandResult(stdout="\n".join(lines))


@COMMAND_REGISTRY.command("stat", description="Display file status")
def stat(shell: "SandboxShell", args: list[str]) -> CommandResult:
    if not args:
        return CommandResult(stderr="stat expects a file path", exit_code=2)
    path = args[0]
    shell._ensure_visible_path(path)
    try:
        node = shell.vfs.get_node(path)
    except (NodeNotFound, InvalidOperation) as exc:
        return CommandResult(stderr=str(exc), exit_code=1)

    lines = [f"  File: {node.path()}"]
    if isinstance(node, VirtualFile):
        size = len(node.read(shell.vfs))
        kind = "regular file"
    else:
        size = 0
        kind = "directory"

    lines.append(f"  Size: {size:<10} Type: {kind}")
    lines.append(f"  Vers: {node.version:<10} Policy: {node.policy}")
    created = datetime.fromtimestamp(node.created_at).strftime("%Y-%m-%d %H:%M:%S")
    modified = datetime.fromtimestamp(node.modified_at).strftime("%Y-%m-%d %H:%M:%S")
    lines.append(f" Birth: {created}")
    lines.append(f"Modify: {modified}")
    return CommandResult(stdout="\n".join(lines))
