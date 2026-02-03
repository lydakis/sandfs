"""Minimal shell parser for pipelines and redirections."""

from __future__ import annotations

import re
import shlex
from dataclasses import dataclass, field


@dataclass
class CommandSpec:
    name: str | None
    args: list[str] = field(default_factory=list)
    assignments: dict[str, str] = field(default_factory=dict)
    stdin: str | None = None
    stdout: str | None = None
    append: bool = False


@dataclass
class Pipeline:
    commands: list[CommandSpec]


_ASSIGNMENT_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")


def _tokenize(command_line: str) -> list[str]:
    lexer = shlex.shlex(command_line, posix=True, punctuation_chars="|<>")
    lexer.whitespace_split = True
    lexer.commenters = ""
    return list(lexer)


def parse_pipeline(command_line: str) -> Pipeline:
    tokens = _tokenize(command_line)
    if not tokens:
        return Pipeline(commands=[])

    commands: list[CommandSpec] = []
    current = CommandSpec(name=None)
    idx = 0
    while idx < len(tokens):
        token = tokens[idx]
        if token == "|":
            _finalize_command(commands, current)
            current = CommandSpec(name=None)
            idx += 1
            continue
        if token in ("<", ">", ">>"):
            if idx + 1 >= len(tokens):
                raise ValueError(f"Missing redirection target after {token}")
            target = tokens[idx + 1]
            if token == "<":
                current.stdin = target
            else:
                current.stdout = target
                current.append = token == ">>"
            idx += 2
            continue
        if current.name is None and _ASSIGNMENT_RE.match(token):
            key, value = token.split("=", 1)
            current.assignments[key] = value
        elif current.name is None:
            current.name = token
        else:
            current.args.append(token)
        idx += 1

    _finalize_command(commands, current)
    return Pipeline(commands=commands)


def _finalize_command(commands: list[CommandSpec], command: CommandSpec) -> None:
    if command.name is None and not command.assignments:
        raise ValueError("Missing command before pipe or end of line")
    if command.name is None and (command.stdin or command.stdout):
        raise ValueError("Redirection without command is not supported")
    commands.append(command)


__all__ = ["CommandSpec", "Pipeline", "parse_pipeline"]
