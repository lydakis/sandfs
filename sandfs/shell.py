"""Pure-Python shell facade around the virtual filesystem."""

from __future__ import annotations

import re
import shlex
import subprocess
from dataclasses import dataclass
from typing import Callable, Dict, Iterable, List, Optional, Set

from .exceptions import InvalidOperation, SandboxError, NodeNotFound
from .policies import VisibilityView
from .pyexec import PythonExecutor
from .vfs import DirEntry, VirtualFileSystem


@dataclass
class CommandResult:
    stdout: str = ""
    stderr: str = ""
    exit_code: int = 0


CommandHandler = Callable[[List[str]], CommandResult | str | None]


class SandboxShell:
    """Executes a curated subset of shell commands against the VFS."""

    def __init__(
        self,
        vfs: VirtualFileSystem,
        *,
        python_executor: Optional[PythonExecutor] = None,
        env: Optional[Dict[str, str]] = None,
        view: Optional[VisibilityView] = None,
        allowed_commands: Optional[Iterable[str]] = None,
        max_output_bytes: Optional[int] = None,
        host_fallback: bool = True,
    ) -> None:
        self.vfs = vfs
        self.env: Dict[str, str] = dict(env or {})
        self.commands: Dict[str, CommandHandler] = {}
        self.command_docs: Dict[str, str] = {}
        self.last_command_name: Optional[str] = None
        self.command_docs: Dict[str, str] = {}
        self.py_exec = python_executor or PythonExecutor(vfs)
        self.view = view or VisibilityView()
        self.allowed_commands: Optional[Set[str]] = set(allowed_commands) if allowed_commands else None
        self.max_output_bytes = max_output_bytes
        self.host_fallback = host_fallback
        self._register_builtin_commands()

    # ------------------------------------------------------------------
    # Command registration
    # ------------------------------------------------------------------
    def register_command(self, name: str, handler: CommandHandler, *, description: str = "") -> None:
        self.commands[name] = handler
        if description:
            self.command_docs[name] = description

    def available_commands(self) -> List[str]:
        return sorted(self.commands)

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

    def _run_host_process(self, command_tokens: List[str], path: Optional[str]) -> CommandResult:
        if not command_tokens:
            return CommandResult(stderr="Missing host command", exit_code=2)
        target = path or self.vfs.pwd()
        self._ensure_visible_path(target)
        try:
            with self.vfs.materialize(target) as fs_root:
                completed = subprocess.run(
                    command_tokens,
                    cwd=str(fs_root),
                    capture_output=True,
                    text=True,
                    check=False,
                )
        except SandboxError as exc:
            return CommandResult(stderr=str(exc), exit_code=1)
        except FileNotFoundError as exc:
            return CommandResult(stderr=str(exc), exit_code=127)
        return CommandResult(
            stdout=completed.stdout,
            stderr=completed.stderr,
            exit_code=completed.returncode,
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
                return self._run_host_process(tokens, None)
            return CommandResult(stderr=f"Unknown command: {name}", exit_code=127)
        if self.allowed_commands is not None and name not in self.allowed_commands:
            return CommandResult(stderr=f"Command '{name}' is disabled in this shell", exit_code=1)
        try:
            result = handler(args)
        except SandboxError as exc:
            return CommandResult(stderr=str(exc), exit_code=1)
        except Exception as exc:  # pragma: no cover - unexpected failure path
            return CommandResult(stderr=f"{name} failed: {exc}", exit_code=1)
        if isinstance(result, CommandResult):
            return self._enforce_output_limit(result)
        if result is None:
            return self._enforce_output_limit(CommandResult())
        return self._enforce_output_limit(CommandResult(stdout=str(result)))

    # ------------------------------------------------------------------
    # Builtin commands
    # ------------------------------------------------------------------
    def _register_builtin_commands(self) -> None:
        self.register_command("pwd", self._cmd_pwd, description="Print working directory")
        self.register_command("cd", self._cmd_cd, description="Change directory")
        self.register_command("ls", self._cmd_ls, description="List directory contents")
        self.register_command("cat", self._cmd_cat, description="Print file contents")
        self.register_command("touch", self._cmd_touch, description="Create empty file")
        self.register_command("mkdir", self._cmd_mkdir, description="Create directories")
        self.register_command("rm", self._cmd_rm, description="Remove files or directories")
        self.register_command("tree", self._cmd_tree, description="Render tree view")
        self.register_command("write", self._cmd_write, description="Write text to file")
        self.register_command("append", self._cmd_append, description="Append text to file")
        self.register_command("grep", self._cmd_grep, description="Search files (non-recursive)")
        self.register_command("rg", self._cmd_rg, description="Search files recursively")
        self.register_command("python", self._cmd_python, description="Execute Python snippet")
        self.register_command("python3", self._cmd_python, description="Execute Python snippet")
        self.register_command("host", self._cmd_host, description="Run host command in materialized tree")
        self.register_command("bash", self._cmd_shell_host, description="Run bash via host")
        self.register_command("sh", self._cmd_shell_host, description="Run sh via host")
        self.register_command("help", self._cmd_help, description="Show available commands")

    def _cmd_pwd(self, _: List[str]) -> CommandResult:
        return CommandResult(stdout=self.vfs.pwd())

    def _cmd_cd(self, args: List[str]) -> CommandResult:
        if len(args) != 1:
            return CommandResult(stderr="cd expects exactly one path", exit_code=2)
        self._ensure_visible_path(args[0])
        new_path = self.vfs.cd(args[0])
        return CommandResult(stdout=new_path)

    def _cmd_ls(self, args: List[str]) -> CommandResult:
        long = False
        targets: List[str] = []
        for arg in args:
            if arg in ("-l", "--long"):
                long = True
            elif arg.startswith("-"):
                # fallback for other flags via host
                return self._run_host_process(["ls", *args], None)
            else:
                targets.append(arg)
        if not targets:
            targets = [self.vfs.pwd()]
        blocks: List[str] = []
        for idx, target in enumerate(targets):
            self._ensure_visible_path(target)
            entries = self.vfs.ls(target, view=self.view)
            if len(targets) > 1:
                blocks.append(f"{target}:")
            blocks.append(self._format_ls(entries, long_format=long))
            if idx < len(targets) - 1:
                blocks.append("")
        return CommandResult(stdout="\n".join(filter(None, blocks)))

    def _format_ls(self, entries: List[DirEntry], *, long_format: bool) -> str:
        if not entries:
            return ""
        if long_format:
            return "\n".join(
                f"{'d' if entry.is_dir else '-'} {entry.path}"
                for entry in entries
            )
        return "  ".join(
            f"{entry.name}/" if entry.is_dir else entry.name
            for entry in entries
        )

    def _cmd_cat(self, args: List[str]) -> CommandResult:
        if not args:
            return CommandResult(stderr="cat expects at least one file", exit_code=2)
        blobs = []
        for path in args:
            self._ensure_visible_path(path)
            blobs.append(self.vfs.read_file(path))
        return CommandResult(stdout="".join(blobs))

    def _cmd_append(self, args: List[str]) -> CommandResult:
        if len(args) < 2:
            return CommandResult(stderr="append expects a path and text", exit_code=2)
        path = args[0]
        self._ensure_visible_path(path)
        text = " ".join(args[1:])
        self.vfs.append_file(path, text)
        return CommandResult()

    def _cmd_touch(self, args: List[str]) -> CommandResult:
        if not args:
            return CommandResult(stderr="touch expects at least one file", exit_code=2)
        for path in args:
            self._ensure_visible_path(path)
            self.vfs.touch(path)
        return CommandResult()

    def _cmd_mkdir(self, args: List[str]) -> CommandResult:
        parents = False
        paths: List[str] = []
        for arg in args:
            if arg in ("-p", "--parents"):
                parents = True
            else:
                paths.append(arg)
        if not paths:
            return CommandResult(stderr="mkdir expects a path", exit_code=2)
        for path in paths:
            self._ensure_visible_path(path)
            self.vfs.mkdir(path, parents=parents, exist_ok=parents)
        return CommandResult()

    def _cmd_rm(self, args: List[str]) -> CommandResult:
        recursive = False
        targets: List[str] = []
        for arg in args:
            if arg in ("-r", "-rf", "-R", "--recursive"):
                recursive = True
            else:
                targets.append(arg)
        if not targets:
            return CommandResult(stderr="rm expects a target", exit_code=2)
        for target in targets:
            self._ensure_visible_path(target)
            self.vfs.remove(target, recursive=recursive)
        return CommandResult()

    def _cmd_tree(self, args: List[str]) -> CommandResult:
        target = args[0] if args else None
        if target:
            self._ensure_visible_path(target)
        return CommandResult(stdout=self.vfs.tree(target, view=self.view))

    def _cmd_write(self, args: List[str]) -> CommandResult:
        if not args:
            return CommandResult(stderr="write expects a target path", exit_code=2)
        path = args[0]
        self._ensure_visible_path(path)
        text_parts: List[str] = []
        append = False
        idx = 1
        while idx < len(args):
            token = args[idx]
            if token == "--append":
                append = True
                idx += 1
                continue
            if token == "--text" and idx + 1 < len(args):
                text_parts.append(args[idx + 1])
                idx += 2
                continue
            text_parts.append(token)
            idx += 1
        payload = " ".join(text_parts)
        if append:
            self.vfs.append_file(path, payload)
        else:
            self.vfs.write_file(path, payload)
        return CommandResult()

    def _cmd_grep(self, args: List[str]) -> CommandResult:
        if not args:
            return CommandResult(stderr="grep expects a pattern", exit_code=2)
        recursive = False
        regex = False
        ignore_case = False
        show_numbers = False
        paths: List[str] = []
        pattern: Optional[str] = None
        idx = 0
        while idx < len(args):
            token = args[idx]
            if token in ("-r", "-R", "--recursive"):
                recursive = True
                idx += 1
                continue
            if token in ("-i", "--ignore-case"):
                ignore_case = True
                idx += 1
                continue
            if token in ("-n", "--line-number"):
                show_numbers = True
                idx += 1
                continue
            if token in ("-e", "--regex"):
                regex = True
                idx += 1
                continue
            if pattern is None:
                pattern = token
            else:
                paths.append(token)
            idx += 1
        if pattern is None:
            return CommandResult(stderr="Missing pattern", exit_code=2)
        if not paths:
            paths = [self.vfs.pwd()]
        for target in paths:
            self._ensure_visible_path(target)
        output = self._search(pattern, paths, recursive=recursive, regex=regex, ignore_case=ignore_case, show_numbers=show_numbers)
        return CommandResult(stdout="\n".join(output))

    def _cmd_rg(self, args: List[str]) -> CommandResult:
        # ripgrep defaults to recursive search
        return self._cmd_grep(["-r"] + args)

    def _search(
        self,
        pattern: str,
        paths: Iterable[str],
        *,
        recursive: bool,
        regex: bool,
        ignore_case: bool,
        show_numbers: bool,
    ) -> List[str]:
        results: List[str] = []
        flags = re.MULTILINE
        if ignore_case:
            flags |= re.IGNORECASE
        compiled = re.compile(pattern, flags) if regex else None
        lowered = pattern.lower() if ignore_case and not regex else None
        for target in paths:
            for file_path, file_node in self.vfs.iter_files(target, recursive=recursive):
                if self.view and not self.view.allows(file_node.policy):
                    continue
                text = file_node.read(self.vfs)
                lines = text.splitlines()
                for idx, line in enumerate(lines, start=1):
                    matched = False
                    if regex:
                        if compiled and compiled.search(line):
                            matched = True
                    elif ignore_case:
                        if lowered and lowered in line.lower():
                            matched = True
                    else:
                        if pattern in line:
                            matched = True
                    if matched:
                        prefix = f"{file_path}:{idx}:" if show_numbers else f"{file_path}:"
                        results.append(f"{prefix}{line}")
        return results

    def _cmd_python(self, args: List[str]) -> CommandResult:
        if not args:
            return CommandResult(stderr="python expects code", exit_code=2)
        if args[0] == "-c" and len(args) >= 2:
            code = " ".join(args[1:])
        else:
            code = " ".join(args)
        result = self.py_exec.run(code)
        return CommandResult(stdout=result.stdout)

    def _cmd_shell_host(self, args: List[str]) -> CommandResult:
        # First token is bash/sh command itself; delegate to host with same args
        return self._run_host_process([self.last_command_name] + args, None)

    def _cmd_host(self, args: List[str]) -> CommandResult:
        path: Optional[str] = None
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
        return self._run_host_process(command_tokens, path)

    def _cmd_help(self, _: List[str]) -> CommandResult:
        lines = ["Available commands:"]
        for name in self.available_commands():
            desc = self.command_docs.get(name, "")
            if desc:
                lines.append(f"  {name} - {desc}")
            else:
                lines.append(f"  {name}")
        lines.append("Use host <cmd> (or run unknown commands directly) for full GNU tools.")
        return CommandResult(stdout="\n".join(lines))


__all__ = ["SandboxShell", "CommandResult"]
