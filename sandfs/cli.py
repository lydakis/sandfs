"""Command-line interface for sandfs."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from .adapters import FileSystemAdapter
from .policies import NodePolicy
from .shell import SandboxShell
from .vfs import VirtualFileSystem


def _add_common_flags(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--mount",
        action="append",
        default=[],
        help="Mount a host directory as /path (format: /host/path:/sandbox/path).",
    )
    parser.add_argument(
        "--enable-search",
        action="store_true",
        help="Enable full-text search index and /@search view.",
    )
    parser.add_argument(
        "--host",
        action="store_true",
        help="Enable host fallback execution for unknown commands.",
    )


def _build_vfs(mounts: list[str], enable_search: bool) -> VirtualFileSystem:
    vfs = VirtualFileSystem()
    for mount in mounts:
        if ":" not in mount:
            raise ValueError(f"Invalid mount '{mount}'. Expected /host:/sandbox")
        src, dst = mount.split(":", 1)
        adapter = FileSystemAdapter(Path(src))
        vfs.mount_storage(dst, adapter, policy=NodePolicy(writable=True))
    if enable_search:
        vfs.enable_full_text_index()
        vfs.enable_search_view()
    return vfs


def _run_exec(args: argparse.Namespace) -> int:
    vfs = _build_vfs(args.mount, args.enable_search)
    shell = SandboxShell(vfs, host_fallback=args.host)
    result = shell.exec(args.command)
    if result.stdout:
        sys.stdout.write(result.stdout)
    if result.stderr:
        sys.stderr.write(result.stderr)
    return result.exit_code


def _run_shell(args: argparse.Namespace) -> int:
    vfs = _build_vfs(args.mount, args.enable_search)
    shell = SandboxShell(vfs, host_fallback=args.host)
    try:
        while True:
            prompt = f"{vfs.pwd()}$ "
            line = input(prompt)
            if line.strip() in {":q", "exit", "quit"}:
                return 0
            result = shell.exec(line)
            if result.stdout:
                sys.stdout.write(result.stdout)
            if result.stderr:
                sys.stderr.write(result.stderr)
    except (EOFError, KeyboardInterrupt):
        return 0


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(prog="sandfs")
    subparsers = parser.add_subparsers(dest="command_name", required=True)

    exec_parser = subparsers.add_parser("exec", help="Run a single command")
    _add_common_flags(exec_parser)
    exec_parser.add_argument("command", help="Command string to execute")
    exec_parser.set_defaults(func=_run_exec)

    shell_parser = subparsers.add_parser("shell", help="Start an interactive shell")
    _add_common_flags(shell_parser)
    shell_parser.set_defaults(func=_run_shell)

    args = parser.parse_args(argv)
    exit_code = args.func(args)
    raise SystemExit(exit_code)


__all__ = ["main"]
