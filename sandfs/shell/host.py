"""Helpers for materializing the sandbox on the host."""

from __future__ import annotations

import re
import subprocess
from pathlib import Path, PurePosixPath
from typing import TYPE_CHECKING

from ..exceptions import InvalidOperation, SandboxError
from ..nodes import VirtualDirectory, VirtualFile
from .common import CommandResult

if TYPE_CHECKING:
    from .core import SandboxShell


def run_host_process(
    shell: "SandboxShell", command_tokens: list[str], path: str | None
) -> CommandResult:
    """Materialize the VFS and run the host command inside it."""

    if not command_tokens:
        return CommandResult(stderr="Missing host command", exit_code=2)
    target = str(shell.vfs._normalize(path or shell.vfs.pwd()))
    shell._ensure_visible_path(target)
    sandbox_cwd = PurePosixPath(target)
    try:
        with shell.vfs.materialize("/") as fs_root:
            host_cwd = sandbox_to_host_path(shell, fs_root, sandbox_cwd)
            mapped = map_command_tokens(shell, command_tokens, fs_root)
            completed = subprocess.run(
                mapped,
                cwd=str(host_cwd),
                capture_output=True,
                text=True,
                check=False,
            )
            sync_from_host(shell, fs_root)
    except SandboxError as exc:
        return CommandResult(stderr=str(exc), exit_code=1)
    except FileNotFoundError as exc:
        return CommandResult(stderr=str(exc), exit_code=127)
    except OSError as exc:
        return CommandResult(stderr=str(exc), exit_code=getattr(exc, "errno", 1))
    return CommandResult(
        stdout=completed.stdout,
        stderr=completed.stderr,
        exit_code=completed.returncode,
    )


def sandbox_to_host_path(shell: "SandboxShell", fs_root: Path, sandbox_path: PurePosixPath) -> Path:
    if not sandbox_path.is_absolute():
        sandbox_path = PurePosixPath(shell.vfs._normalize(sandbox_path))
    if sandbox_path == PurePosixPath("/"):
        return fs_root
    rel = sandbox_path.relative_to("/")
    return fs_root.joinpath(*rel.parts)


def map_command_tokens(shell: "SandboxShell", tokens: list[str], fs_root: Path) -> list[str]:
    return [translate_token(shell, token, fs_root) for token in tokens]


def translate_token(shell: "SandboxShell", token: str, fs_root: Path) -> str:
    def replacer(match: re.Match[str]) -> str:
        candidate = match.group(0)
        if match.start() >= 3 and token[match.start() - 3 : match.start()] == "://":
            return candidate
        sandbox_path = eligible_sandbox_path(shell, candidate)
        if sandbox_path is None:
            return candidate
        host_path = sandbox_to_host_path(shell, fs_root, sandbox_path)
        rendered = str(host_path)
        if candidate.endswith("/") and not rendered.endswith("/"):
            rendered = f"{rendered}/"
        return rendered

    return re.sub(r"/[A-Za-z0-9._/\-]+", replacer, token)


def eligible_sandbox_path(shell: "SandboxShell", path_str: str) -> PurePosixPath | None:
    try:
        normalized = PurePosixPath(shell.vfs._normalize(path_str))
    except InvalidOperation:
        return None
    if path_str == "/":
        return normalized
    if shell.vfs.exists(path_str):
        return normalized
    parent = normalized.parent if normalized.parent != normalized else None
    if parent and parent != PurePosixPath("/") and shell.vfs.is_dir(str(parent)):
        return normalized
    return None


def sync_from_host(shell: "SandboxShell", fs_root: Path) -> None:
    host_dirs: set[PurePosixPath] = set()
    host_files: set[PurePosixPath] = set()
    for path in sorted(fs_root.rglob("*")):
        sandbox_path = PurePosixPath("/").joinpath(*path.relative_to(fs_root).parts)
        if path.is_dir():
            host_dirs.add(sandbox_path)
            if sandbox_path != PurePosixPath("/"):
                shell.vfs.mkdir(sandbox_path, parents=True, exist_ok=True)
            continue
        host_files.add(sandbox_path)
        try:
            text = path.read_text()
        except UnicodeDecodeError:
            text = path.read_bytes().decode(errors="ignore")
        shell.vfs.mkdir(sandbox_path.parent, parents=True, exist_ok=True)
        should_write = True
        if shell.vfs.is_file(sandbox_path):
            try:
                existing = shell.vfs.read_file(sandbox_path)
            except InvalidOperation:
                existing = None
            else:
                if existing == text:
                    should_write = False
        if should_write:
            shell.vfs.write_file(sandbox_path, text)
    remove_missing(shell, host_dirs, host_files)


def remove_missing(
    shell: "SandboxShell",
    host_dirs: set[PurePosixPath],
    host_files: set[PurePosixPath],
) -> None:
    existing_dirs: list[PurePosixPath] = []
    existing_files: list[PurePosixPath] = []
    for path, node in shell.vfs.walk("/"):
        sandbox_path = PurePosixPath(path)
        if isinstance(node, VirtualDirectory):
            existing_dirs.append(sandbox_path)
        elif isinstance(node, VirtualFile):
            existing_files.append(sandbox_path)
    for file_path in existing_files:
        if file_path not in host_files:
            shell.vfs.remove(str(file_path))
    for dir_path in sorted(existing_dirs, key=lambda p: len(p.parts), reverse=True):
        if dir_path == PurePosixPath("/"):
            continue
        if dir_path not in host_dirs and str(dir_path) != "/":
            shell.vfs.remove(str(dir_path), recursive=True)


__all__ = ["run_host_process"]
