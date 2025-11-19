"""Text processing commands."""

from __future__ import annotations

from typing import TYPE_CHECKING

from ..common import CommandResult
from ..registry import COMMAND_REGISTRY
from ...exceptions import InvalidOperation, NodeNotFound

if TYPE_CHECKING:  # pragma: no cover - typing helper
    from ..core import SandboxShell


@COMMAND_REGISTRY.command("write", description="Write text to file")
def write(shell: "SandboxShell", args: list[str]) -> CommandResult:
    if not args:
        return CommandResult(stderr="write expects a target path", exit_code=2)
    path = args[0]
    shell._ensure_visible_path(path)
    text_parts: list[str] = []
    append_flag = False
    idx = 1
    while idx < len(args):
        token = args[idx]
        if token == "--append":
            append_flag = True
            idx += 1
            continue
        if token == "--text" and idx + 1 < len(args):
            text_parts.append(args[idx + 1])
            idx += 2
            continue
        text_parts.append(token)
        idx += 1
    payload = " ".join(text_parts)
    if append_flag:
        shell.vfs.append_file(path, payload)
    else:
        shell.vfs.write_file(path, payload)
    return CommandResult()


@COMMAND_REGISTRY.command("append", description="Append text to file")
def append(shell: "SandboxShell", args: list[str]) -> CommandResult:
    if len(args) < 2:
        return CommandResult(stderr="append expects a path and text", exit_code=2)
    path = args[0]
    shell._ensure_visible_path(path)
    text = " ".join(args[1:])
    shell.vfs.append_file(path, text)
    return CommandResult()


def _slice_content(content: str, *, count: int, mode: str, tail: bool) -> str:
    if mode == "lines":
        lines = content.splitlines(keepends=True)
        return "".join(lines[-count:] if tail else lines[:count])
    return content[-count:] if tail else content[:count]


def _read_range_command(
    shell: "SandboxShell", args: list[str], *, tail: bool
) -> CommandResult:
    count = 10
    mode = "lines"
    paths: list[str] = []

    idx = 0
    while idx < len(args):
        arg = args[idx]
        if arg in ("-n", "-c"):
            if idx + 1 >= len(args):
                name = "n" if arg == "-n" else "c"
                return CommandResult(
                    stderr=f"{'tail' if tail else 'head'}: option requires an argument -- '{name}'",
                    exit_code=1,
                )
            arg_value = args[idx + 1]
            try:
                count = int(arg_value)
            except ValueError:
                return CommandResult(
                    stderr=f"{'tail' if tail else 'head'}: invalid number: '{arg_value}'",
                    exit_code=1,
                )
            mode = "lines" if arg == "-n" else "bytes"
            idx += 2
            continue
        paths.append(arg)
        idx += 1

    if not paths:
        return CommandResult(
            stderr=f"{'tail' if tail else 'head'}: missing file operand",
            exit_code=1,
        )

    output: list[str] = []
    for i, path in enumerate(paths):
        shell._ensure_visible_path(path)
        try:
            content = shell.vfs.read_file(path)
        except (NodeNotFound, InvalidOperation) as exc:
            return CommandResult(stderr=str(exc), exit_code=1)

        if len(paths) > 1:
            output.append(f"==> {path} <==")

        output.append(_slice_content(content, count=count, mode=mode, tail=tail))

        if i < len(paths) - 1:
            output.append("")

    return CommandResult(stdout="\n".join(output))


@COMMAND_REGISTRY.command("head", description="Output the first part of files")
def head(shell: "SandboxShell", args: list[str]) -> CommandResult:
    return _read_range_command(shell, args, tail=False)


@COMMAND_REGISTRY.command("tail", description="Output the last part of files")
def tail(shell: "SandboxShell", args: list[str]) -> CommandResult:
    return _read_range_command(shell, args, tail=True)
