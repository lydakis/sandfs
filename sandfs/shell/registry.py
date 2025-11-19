"""Registry for shell commands."""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from typing import Iterable

from .common import ShellCommand


@dataclass(slots=True)
class CommandSpec:
    name: str
    handler: ShellCommand
    description: str = ""


class CommandRegistry:
    """Simple container that stores command factories."""

    def __init__(self) -> None:
        self._commands: list[CommandSpec] = []

    def register(
        self,
        name: str,
        handler: ShellCommand,
        *,
        description: str = "",
    ) -> ShellCommand:
        self._commands.append(CommandSpec(name, handler, description))
        return handler

    def command(
        self,
        name: str,
        *,
        description: str = "",
    ) -> Callable[[ShellCommand], ShellCommand]:
        """Decorator variant for registering shell commands."""

        def decorator(func: ShellCommand) -> ShellCommand:
            return self.register(name, func, description=description)

        return decorator

    def iter_commands(self) -> Iterable[CommandSpec]:
        return tuple(self._commands)


COMMAND_REGISTRY = CommandRegistry()


__all__ = ["COMMAND_REGISTRY", "CommandRegistry", "CommandSpec"]
