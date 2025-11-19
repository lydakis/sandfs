"""Python execution commands."""

from __future__ import annotations

from typing import TYPE_CHECKING

from ..common import CommandResult
from ..registry import COMMAND_REGISTRY

if TYPE_CHECKING:  # pragma: no cover - typing helper
    from ..core import SandboxShell


def _python(shell: "SandboxShell", args: list[str]) -> CommandResult:
    if not args:
        return CommandResult(stderr="python expects code", exit_code=2)
    if args[0] == "-c" and len(args) >= 2:
        code = " ".join(args[1:])
    else:
        code = " ".join(args)
    result = shell.py_exec.run(code)
    return CommandResult(stdout=result.stdout)


@COMMAND_REGISTRY.command("python", description="Execute Python snippet")
def python(shell: "SandboxShell", args: list[str]) -> CommandResult:  # noqa: A001
    return _python(shell, args)


@COMMAND_REGISTRY.command("python3", description="Execute Python snippet")
def python3(shell: "SandboxShell", args: list[str]) -> CommandResult:
    return _python(shell, args)
