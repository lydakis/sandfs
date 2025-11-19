"""Host integration commands."""

from __future__ import annotations

from typing import TYPE_CHECKING

from ..common import CommandResult
from ..host import run_host_process
from ..registry import COMMAND_REGISTRY

if TYPE_CHECKING:
    from ..core import SandboxShell


def _shell_host(shell: "SandboxShell", args: list[str]) -> CommandResult:
    command = shell.last_command_name
    if command is None:
        return CommandResult(stderr="host shell command unavailable", exit_code=1)
    return run_host_process(shell, [command, *args], None)


@COMMAND_REGISTRY.command("host", description="Run host command in materialized tree")
def host(shell: "SandboxShell", args: list[str]) -> CommandResult:
    path: str | None = None
    idx = 0
    while idx < len(args):
        token = args[idx]
        if token in ("-p", "--path", "-C"):
            idx += 1
            if idx >= len(args):
                return CommandResult(stderr="host expects a path after -p/--path", exit_code=2)
            path = args[idx]
            idx += 1
            continue
        if token == "--":
            idx += 1
            break
        break
    command_tokens = args[idx:]
    if not command_tokens:
        return CommandResult(stderr="host expects a command to run", exit_code=2)
    return run_host_process(shell, command_tokens, path)


@COMMAND_REGISTRY.command("bash", description="Run bash via host")
def bash(shell: "SandboxShell", args: list[str]) -> CommandResult:  # noqa: A001
    return _shell_host(shell, args)


@COMMAND_REGISTRY.command("sh", description="Run sh via host")
def sh(shell: "SandboxShell", args: list[str]) -> CommandResult:  # noqa: A001
    return _shell_host(shell, args)
