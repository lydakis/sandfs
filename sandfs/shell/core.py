"""Core SandboxShell implementation."""

from __future__ import annotations

import shlex
from collections.abc import Iterable

from ..exceptions import InvalidOperation, NodeNotFound, SandboxError
from ..policies import VisibilityView
from ..pyexec import PythonExecutor
from ..vfs import VirtualFileSystem
from .common import CommandHandler, CommandResult, ShellCommand
from .host import run_host_process
from .registry import COMMAND_REGISTRY


class SandboxShell:
    """Executes a curated subset of shell commands against the VFS."""

    def __init__(
        self,
        vfs: VirtualFileSystem,
        *,
        python_executor: PythonExecutor | None = None,
        env: dict[str, str] | None = None,
        view: VisibilityView | None = None,
        allowed_commands: Iterable[str] | None = None,
        max_output_bytes: int | None = None,
        host_fallback: bool = True,
    ) -> None:
        self.vfs = vfs
        self.env: dict[str, str] = dict(env or {})
        self.commands: dict[str, CommandHandler] = {}
        self.command_docs: dict[str, str] = {}
        self.last_command_name: str | None = None
        self.py_exec = python_executor or PythonExecutor(vfs)
        self.view = view or VisibilityView()
        self.allowed_commands: set[str] | None = set(allowed_commands) if allowed_commands else None
        self.max_output_bytes = max_output_bytes
        self.host_fallback = host_fallback
        self._register_builtin_commands()

    # ------------------------------------------------------------------
    # Command registration
    # ------------------------------------------------------------------
    def register_command(
        self,
        name: str,
        handler: CommandHandler,
        *,
        description: str = "",
    ) -> None:
        self.commands[name] = handler
        if description:
            self.command_docs[name] = description

    def available_commands(self) -> list[str]:
        return sorted(self.commands)

    def _bind_registered_handler(self, func: ShellCommand) -> CommandHandler:
        def bound(args: list[str]) -> CommandResult | str | None:
            return func(self, args)

        return bound

    def _register_builtin_commands(self) -> None:
        # Import command modules for their side effects (registration)
        from . import commands  # noqa: F401

        for spec in COMMAND_REGISTRY.iter_commands():
            self.register_command(
                spec.name,
                self._bind_registered_handler(spec.handler),
                description=spec.description,
            )

    def _ensure_visible_path(self, path: str) -> None:
        if self.view is None:
            return
        try:
            policy = self.vfs.get_policy(path)
        except NodeNotFound:
            return
        if not self.view.allows(policy):
            raise InvalidOperation(f"Path {path} is hidden for this view")

    def _enforce_output_limit(self, result: CommandResult) -> CommandResult:
        if self.max_output_bytes is None:
            return result
        total = len(result.stdout) + len(result.stderr)
        if total <= self.max_output_bytes:
            return result
        return CommandResult(
            stdout="",
            stderr=f"Output limit ({self.max_output_bytes} bytes) exceeded",
            exit_code=1,
        )

    # ------------------------------------------------------------------
    # Dispatcher
    # ------------------------------------------------------------------
    def exec(self, command: str) -> CommandResult:
        last_result = CommandResult()
        for segment in filter(None, (line.strip() for line in command.splitlines())):
            last_result = self._exec_one(segment)
            if last_result.exit_code != 0:
                return last_result
        return last_result

    def _exec_one(self, command: str) -> CommandResult:
        if not command.strip():
            return CommandResult()
        try:
            tokens = shlex.split(command)
        except ValueError as exc:
            return CommandResult(stderr=str(exc), exit_code=2)
        if not tokens:
            return CommandResult()
        name, *args = tokens
        self.last_command_name = name
        handler = self.commands.get(name)
        if handler is None:
            if self.host_fallback:
                return run_host_process(self, tokens, None)
            return CommandResult(stderr=f"Unknown command: {name}", exit_code=127)
        if self.allowed_commands is not None and name not in self.allowed_commands:
            return CommandResult(stderr=f"Command '{name}' is disabled in this shell", exit_code=1)
        try:
            result = handler(args)
        except SandboxError as exc:
            return CommandResult(stderr=str(exc), exit_code=1)
        except Exception as exc:  # unexpected failure path
            return CommandResult(stderr=f"{name} failed: {exc}", exit_code=1)
        if isinstance(result, CommandResult):
            return self._enforce_output_limit(result)
        if result is None:
            return self._enforce_output_limit(CommandResult())
        return self._enforce_output_limit(CommandResult(stdout=str(result)))


__all__ = ["SandboxShell"]
