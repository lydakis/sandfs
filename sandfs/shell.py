"""Pure-Python shell facade around the virtual filesystem."""

from __future__ import annotations

import fnmatch
import re
import shlex
import subprocess
from collections.abc import Callable, Iterable
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path, PurePosixPath

from .exceptions import InvalidOperation, NodeNotFound, SandboxError
from .nodes import VirtualDirectory, VirtualFile, VirtualNode
from .policies import VisibilityView
from .pyexec import PythonExecutor
from .vfs import DirEntry, VirtualFileSystem


@dataclass
class CommandResult:
    stdout: str = ""
    stderr: str = ""
    exit_code: int = 0


CommandHandler = Callable[[list[str]], CommandResult | str | None]


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

    def _run_host_process(self, command_tokens: list[str], path: str | None) -> CommandResult:
        if not command_tokens:
            return CommandResult(stderr="Missing host command", exit_code=2)
        target = str(self.vfs._normalize(path or self.vfs.pwd()))
        self._ensure_visible_path(target)
        sandbox_cwd = PurePosixPath(target)
        try:
            with self.vfs.materialize("/") as fs_root:
                host_cwd = self._sandbox_to_host_path(fs_root, sandbox_cwd)
                mapped = self._map_command_tokens(command_tokens, fs_root)
                completed = subprocess.run(
                    mapped,
                    cwd=str(host_cwd),
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self._sync_from_host(fs_root)
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

    def _sandbox_to_host_path(self, fs_root: Path, sandbox_path: PurePosixPath) -> Path:
        if not sandbox_path.is_absolute():
            sandbox_path = PurePosixPath(self.vfs._normalize(sandbox_path))
        if sandbox_path == PurePosixPath("/"):
            return fs_root
        rel = sandbox_path.relative_to("/")
        return fs_root.joinpath(*rel.parts)

    def _map_command_tokens(self, tokens: list[str], fs_root: Path) -> list[str]:
        return [self._translate_token(token, fs_root) for token in tokens]

    def _translate_token(self, token: str, fs_root: Path) -> str:
        def replacer(match: re.Match[str]) -> str:
            candidate = match.group(0)
            if match.start() >= 3 and token[match.start() - 3 : match.start()] == "://":
                return candidate
            sandbox_path = self._eligible_sandbox_path(candidate)
            if sandbox_path is None:
                return candidate
            host_path = self._sandbox_to_host_path(fs_root, sandbox_path)
            rendered = str(host_path)
            if candidate.endswith("/") and not rendered.endswith("/"):
                rendered = f"{rendered}/"
            return rendered

        return re.sub(r"/[A-Za-z0-9._/\-]+", replacer, token)

    def _eligible_sandbox_path(self, path_str: str) -> PurePosixPath | None:
        try:
            normalized = PurePosixPath(self.vfs._normalize(path_str))
        except InvalidOperation:
            return None
        if path_str == "/":
            return normalized
        if self.vfs.exists(path_str):
            return normalized
        parent = normalized.parent if normalized.parent != normalized else None
        if parent and parent != PurePosixPath("/") and self.vfs.is_dir(str(parent)):
            return normalized
        return None

    def _sync_from_host(self, fs_root: Path) -> None:
        host_dirs: set[PurePosixPath] = set()
        host_files: set[PurePosixPath] = set()
        for path in sorted(fs_root.rglob("*")):
            sandbox_path = PurePosixPath("/").joinpath(*path.relative_to(fs_root).parts)
            if path.is_dir():
                host_dirs.add(sandbox_path)
                if sandbox_path != PurePosixPath("/"):
                    self.vfs.mkdir(sandbox_path, parents=True, exist_ok=True)
                continue
            host_files.add(sandbox_path)
            try:
                text = path.read_text()
            except UnicodeDecodeError:
                text = path.read_bytes().decode(errors="ignore")
            self.vfs.mkdir(sandbox_path.parent, parents=True, exist_ok=True)
            should_write = True
            if self.vfs.is_file(sandbox_path):
                try:
                    existing = self.vfs.read_file(sandbox_path)
                except InvalidOperation:
                    existing = None
                else:
                    if existing == text:
                        should_write = False
            if should_write:
                self.vfs.write_file(sandbox_path, text)
        self._remove_missing(host_dirs, host_files)

    def _remove_missing(
        self,
        host_dirs: set[PurePosixPath],
        host_files: set[PurePosixPath],
    ) -> None:
        existing_dirs: list[PurePosixPath] = []
        existing_files: list[PurePosixPath] = []
        for path, node in self.vfs.walk("/"):
            sandbox_path = PurePosixPath(path)
            if isinstance(node, VirtualDirectory):
                existing_dirs.append(sandbox_path)
            elif isinstance(node, VirtualFile):
                existing_files.append(sandbox_path)
        for file_path in existing_files:
            if file_path not in host_files:
                self.vfs.remove(str(file_path))
        for dir_path in sorted(existing_dirs, key=lambda p: len(p.parts), reverse=True):
            if dir_path == PurePosixPath("/"):
                continue
            if dir_path not in host_dirs and str(dir_path) != "/":
                self.vfs.remove(str(dir_path), recursive=True)

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
        except Exception as exc:
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
        builtin_commands = [
            ("pwd", self._cmd_pwd, "Print working directory"),
            ("cd", self._cmd_cd, "Change directory"),
            ("ls", self._cmd_ls, "List directory contents"),
            ("cat", self._cmd_cat, "Print file contents"),
            ("touch", self._cmd_touch, "Create empty file"),
            ("mkdir", self._cmd_mkdir, "Create directories"),
            ("rm", self._cmd_rm, "Remove files or directories"),
            ("cp", self._cmd_cp, "Copy files and directories"),
            ("mv", self._cmd_mv, "Move or rename files and directories"),
            ("tree", self._cmd_tree, "Render tree view"),
            ("write", self._cmd_write, "Write text to file"),
            ("append", self._cmd_append, "Append text to file"),
            ("grep", self._cmd_grep, "Search files (non-recursive)"),
            ("rg", self._cmd_rg, "Search files recursively"),
            ("python", self._cmd_python, "Execute Python snippet"),
            ("python3", self._cmd_python, "Execute Python snippet"),
            ("host", self._cmd_host, "Run host command in materialized tree"),
            ("bash", self._cmd_shell_host, "Run bash via host"),
            ("sh", self._cmd_shell_host, "Run sh via host"),
            ("help", self._cmd_help, "Show available commands"),
            ("stat", self._cmd_stat, "Display file status"),
            ("head", self._cmd_head, "Output the first part of files"),
            ("tail", self._cmd_tail, "Output the last part of files"),
            ("find", self._cmd_find, "Search for files in a directory hierarchy"),
        ]
        for name, handler, description in builtin_commands:
            self.register_command(name, handler, description=description)

    def _cmd_pwd(self, _: list[str]) -> CommandResult:
        return CommandResult(stdout=self.vfs.pwd())

    def _cmd_cd(self, args: list[str]) -> CommandResult:
        if len(args) != 1:
            return CommandResult(stderr="cd expects exactly one path", exit_code=2)
        self._ensure_visible_path(args[0])
        new_path = self.vfs.cd(args[0])
        return CommandResult(stdout=new_path)

    def _cmd_ls(self, args: list[str]) -> CommandResult:
        long = False
        targets: list[str] = []
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
        blocks: list[str] = []
        for idx, target in enumerate(targets):
            self._ensure_visible_path(target)
            entries = self.vfs.ls(target, view=self.view)
            if len(targets) > 1:
                blocks.append(f"{target}:")
            blocks.append(self._format_ls(entries, long_format=long))
            if idx < len(targets) - 1:
                blocks.append("")
        return CommandResult(stdout="\n".join(filter(None, blocks)))

    def _format_ls(self, entries: list[DirEntry], *, long_format: bool) -> str:
        if not entries:
            return ""
        if long_format:
            return "\n".join(f"{'d' if entry.is_dir else '-'} {entry.path}" for entry in entries)
        return "  ".join(f"{entry.name}/" if entry.is_dir else entry.name for entry in entries)

    def _cmd_cat(self, args: list[str]) -> CommandResult:
        if not args:
            return CommandResult(stderr="cat expects at least one file", exit_code=2)
        blobs = []
        for path in args:
            self._ensure_visible_path(path)
            blobs.append(self.vfs.read_file(path))
        return CommandResult(stdout="".join(blobs))

    def _cmd_append(self, args: list[str]) -> CommandResult:
        if len(args) < 2:
            return CommandResult(stderr="append expects a path and text", exit_code=2)
        path = args[0]
        self._ensure_visible_path(path)
        text = " ".join(args[1:])
        self.vfs.append_file(path, text)
        return CommandResult()

    def _cmd_touch(self, args: list[str]) -> CommandResult:
        if not args:
            return CommandResult(stderr="touch expects at least one file", exit_code=2)
        for path in args:
            self._ensure_visible_path(path)
            self.vfs.touch(path)
        return CommandResult()

    def _cmd_mkdir(self, args: list[str]) -> CommandResult:
        parents = False
        paths: list[str] = []
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

    def _cmd_rm(self, args: list[str]) -> CommandResult:
        recursive = False
        targets: list[str] = []
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

    def _cmd_cp(self, args: list[str]) -> CommandResult:
        recursive = False
        operands: list[str] = []
        for arg in args:
            if arg in ("-r", "-R", "--recursive"):
                recursive = True
            else:
                operands.append(arg)
        if len(operands) != 2:
            return CommandResult(stderr="cp expects a source and destination", exit_code=2)
        source, dest = operands
        self._ensure_visible_path(source)
        self._ensure_visible_path(dest)
        try:
            self.vfs.copy(source, dest, recursive=recursive)
        except (InvalidOperation, NodeNotFound) as exc:
            return CommandResult(stderr=str(exc), exit_code=1)
        return CommandResult()

    def _cmd_mv(self, args: list[str]) -> CommandResult:
        if len(args) != 2:
            return CommandResult(stderr="mv expects a source and destination", exit_code=2)
        source, dest = args
        self._ensure_visible_path(source)
        self._ensure_visible_path(dest)
        try:
            self.vfs.move(source, dest)
        except (InvalidOperation, NodeNotFound) as exc:
            return CommandResult(stderr=str(exc), exit_code=1)
        return CommandResult()

    def _cmd_tree(self, args: list[str]) -> CommandResult:
        target = args[0] if args else None
        if target:
            self._ensure_visible_path(target)
        return CommandResult(stdout=self.vfs.tree(target, view=self.view))

    def _cmd_write(self, args: list[str]) -> CommandResult:
        if not args:
            return CommandResult(stderr="write expects a target path", exit_code=2)
        path = args[0]
        self._ensure_visible_path(path)
        text_parts: list[str] = []
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

    def _cmd_grep(self, args: list[str]) -> CommandResult:
        if not args:
            return CommandResult(stderr="grep expects a pattern", exit_code=2)
        recursive = False
        regex = False
        ignore_case = False
        show_numbers = False
        paths: list[str] = []
        pattern: str | None = None
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
        output = self._search(
            pattern,
            paths,
            recursive=recursive,
            regex=regex,
            ignore_case=ignore_case,
            show_numbers=show_numbers,
        )
        return CommandResult(stdout="\n".join(output))

    def _cmd_rg(self, args: list[str]) -> CommandResult:
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
    ) -> list[str]:
        results: list[str] = []
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

    def _cmd_python(self, args: list[str]) -> CommandResult:
        if not args:
            return CommandResult(stderr="python expects code", exit_code=2)
        if args[0] == "-c" and len(args) >= 2:
            code = " ".join(args[1:])
        else:
            code = " ".join(args)
        result = self.py_exec.run(code)
        return CommandResult(stdout=result.stdout)

    def _cmd_shell_host(self, args: list[str]) -> CommandResult:
        # First token is bash/sh command itself; delegate to host with same args
        command = self.last_command_name
        if command is None:
            return CommandResult(stderr="host shell command unavailable", exit_code=1)
        return self._run_host_process([command, *args], None)

    def _cmd_host(self, args: list[str]) -> CommandResult:
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
        return self._run_host_process(command_tokens, path)

    def _cmd_help(self, _: list[str]) -> CommandResult:
        lines = ["Available commands:"]
        for name in self.available_commands():
            desc = self.command_docs.get(name, "")
            if desc:
                lines.append(f"  {name} - {desc}")
            else:
                lines.append(f"  {name}")
        lines.append("Use host <cmd> (or run unknown commands directly) for full GNU tools.")
        return CommandResult(stdout="\n".join(lines))

    def _cmd_stat(self, args: list[str]) -> CommandResult:
        if not args:
            return CommandResult(stderr="stat expects a file path", exit_code=2)
        path = args[0]
        self._ensure_visible_path(path)
        try:
            node = self.vfs.get_node(path)
        except (NodeNotFound, InvalidOperation) as exc:
            return CommandResult(stderr=str(exc), exit_code=1)

        lines = [f"  File: {node.path()}"]
        if isinstance(node, VirtualFile):
            size = len(node.read(self.vfs))
            kind = "regular file"
        else:
            size = 0  # Directories do not have a serialized size in this VFS
            kind = "directory"

        lines.append(f"  Size: {size:<10} Type: {kind}")
        lines.append(f"  Vers: {node.version:<10} Policy: {node.policy}")
        created = datetime.fromtimestamp(node.created_at).strftime("%Y-%m-%d %H:%M:%S")
        modified = datetime.fromtimestamp(node.modified_at).strftime("%Y-%m-%d %H:%M:%S")
        lines.append(f" Birth: {created}")
        lines.append(f"Modify: {modified}")
        return CommandResult(stdout="\n".join(lines))

    def _cmd_head(self, args: list[str]) -> CommandResult:
        count = 10
        mode = "lines"  # or "bytes"
        paths: list[str] = []

        idx = 0
        while idx < len(args):
            arg = args[idx]
            if arg == "-n":
                if idx + 1 >= len(args):
                    return CommandResult(
                        stderr="head: option requires an argument -- 'n'",
                        exit_code=1,
                    )
                arg_value = args[idx + 1]
                try:
                    count = int(arg_value)
                    mode = "lines"
                    idx += 2
                    continue
                except ValueError:
                    return CommandResult(
                        stderr=f"head: invalid number: '{arg_value}'",
                        exit_code=1,
                    )
            elif arg == "-c":
                if idx + 1 >= len(args):
                    return CommandResult(
                        stderr="head: option requires an argument -- 'c'",
                        exit_code=1,
                    )
                arg_value = args[idx + 1]
                try:
                    count = int(arg_value)
                    mode = "bytes"
                    idx += 2
                    continue
                except ValueError:
                    return CommandResult(
                        stderr=f"head: invalid number: '{arg_value}'",
                        exit_code=1,
                    )
            else:
                paths.append(arg)
                idx += 1

        if not paths:
            return CommandResult(stderr="head: missing file operand", exit_code=1)

        output: list[str] = []
        for i, path in enumerate(paths):
            self._ensure_visible_path(path)
            try:
                content = self.vfs.read_file(path)
            except (NodeNotFound, InvalidOperation) as exc:
                return CommandResult(stderr=str(exc), exit_code=1)

            if len(paths) > 1:
                output.append(f"==> {path} <==")

            if mode == "lines":
                lines = content.splitlines(keepends=True)
                output.append("".join(lines[:count]))
            else:
                output.append(content[:count])

            if i < len(paths) - 1:
                output.append("")

        return CommandResult(stdout="\n".join(output))

    def _cmd_tail(self, args: list[str]) -> CommandResult:
        count = 10
        mode = "lines"
        paths: list[str] = []

        idx = 0
        while idx < len(args):
            arg = args[idx]
            if arg == "-n":
                if idx + 1 >= len(args):
                    return CommandResult(
                        stderr="tail: option requires an argument -- 'n'",
                        exit_code=1,
                    )
                arg_value = args[idx + 1]
                try:
                    count = int(arg_value)
                    mode = "lines"
                    idx += 2
                    continue
                except ValueError:
                    return CommandResult(
                        stderr=f"tail: invalid number: '{arg_value}'",
                        exit_code=1,
                    )
            elif arg == "-c":
                if idx + 1 >= len(args):
                    return CommandResult(
                        stderr="tail: option requires an argument -- 'c'",
                        exit_code=1,
                    )
                arg_value = args[idx + 1]
                try:
                    count = int(arg_value)
                    mode = "bytes"
                    idx += 2
                    continue
                except ValueError:
                    return CommandResult(
                        stderr=f"tail: invalid number: '{arg_value}'",
                        exit_code=1,
                    )
            else:
                paths.append(arg)
                idx += 1

        if not paths:
            return CommandResult(stderr="tail: missing file operand", exit_code=1)

        output: list[str] = []
        for i, path in enumerate(paths):
            self._ensure_visible_path(path)
            try:
                content = self.vfs.read_file(path)
            except (NodeNotFound, InvalidOperation) as exc:
                return CommandResult(stderr=str(exc), exit_code=1)

            if len(paths) > 1:
                output.append(f"==> {path} <==")

            if mode == "lines":
                lines = content.splitlines(keepends=True)
                output.append("".join(lines[-count:]))
            else:
                output.append(content[-count:])

            if i < len(paths) - 1:
                output.append("")

        return CommandResult(stdout="\n".join(output))

    def _cmd_find(self, args: list[str]) -> CommandResult:
        paths: list[str] = []
        name_pattern: str | None = None
        type_filter: str | None = None

        idx = 0
        while idx < len(args):
            arg = args[idx]
            if arg == "-name":
                if idx + 1 >= len(args):
                    return CommandResult(
                        stderr="find: missing argument to `-name'",
                        exit_code=1,
                    )
                name_pattern = args[idx + 1]
                idx += 2
            elif arg == "-type":
                if idx + 1 >= len(args):
                    return CommandResult(
                        stderr="find: missing argument to `-type'",
                        exit_code=1,
                    )
                candidate = args[idx + 1]
                if candidate not in ("f", "d"):
                    return CommandResult(
                        stderr=f"find: unknown argument to -type: {candidate}",
                        exit_code=1,
                    )
                type_filter = candidate
                idx += 2
            elif arg.startswith("-"):
                return CommandResult(stderr=f"find: unknown predicate `{arg}'", exit_code=1)
            else:
                paths.append(arg)
                idx += 1

        if not paths:
            paths = [self.vfs.pwd()]

        def walk_visible(node: VirtualNode) -> Iterable[tuple[PurePosixPath, VirtualNode]]:
            if self.view and not self.view.allows(node.policy):
                return
            yield (node.path(), node)
            if isinstance(node, VirtualDirectory):
                node.ensure_loaded(self.vfs)
                for child in node.iter_children(self.vfs):
                    yield from walk_visible(child)

        results: list[str] = []
        for start_path in paths:
            self._ensure_visible_path(start_path)
            try:
                start_node = self.vfs.get_node(start_path)
            except (NodeNotFound, InvalidOperation):
                return CommandResult(
                    stderr=f"find: `{start_path}': No such file or directory",
                    exit_code=1,
                )

            for path, node in walk_visible(start_node):
                if type_filter:
                    is_dir = isinstance(node, VirtualDirectory)
                    if type_filter == "f" and is_dir:
                        continue
                    if type_filter == "d" and not is_dir:
                        continue

                if name_pattern and not fnmatch.fnmatch(node.name, name_pattern):
                    continue

                results.append(str(path))

        return CommandResult(stdout="\n".join(results))


__all__ = ["SandboxShell", "CommandResult"]
