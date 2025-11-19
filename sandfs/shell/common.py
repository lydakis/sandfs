"""Shared shell types."""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .core import SandboxShell


@dataclass(slots=True)
class CommandResult:
    stdout: str = ""
    stderr: str = ""
    exit_code: int = 0


CommandHandler = Callable[[list[str]], CommandResult | str | None]
ShellCommand = Callable[["SandboxShell", list[str]], CommandResult | str | None]


__all__ = ["CommandResult", "CommandHandler", "ShellCommand"]
