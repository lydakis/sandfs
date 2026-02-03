"""Pure-Python shell facade around the virtual filesystem."""

from __future__ import annotations

import contextlib
import fnmatch
import inspect
import os
import re
import subprocess
from collections.abc import Callable, Iterable, Iterator
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path, PurePosixPath

from .exceptions import InvalidOperation, NodeNotFound, SandboxError
from .nodes import VirtualDirectory, VirtualFile, VirtualNode
from .policies import VisibilityView
from .pyexec import PythonExecutor
from .search import SearchQuery
from .shell_parser import parse_pipeline
from .vfs import DirEntry, VirtualFileSystem


@dataclass
class CommandResult:
    stdout: str = ""
    stderr: str = ""
    exit_code: int = 0


@dataclass
class CommandContext:
    stdin: str
    env: dict[str, str]
    cwd: str
    vfs: VirtualFileSystem
    view: VisibilityView | None


@dataclass(frozen=True)
class ParsedSearchPath:
    base: PurePosixPath
    query: SearchQuery
    nav: PurePosixPath | None


CommandHandler = Callable[..., CommandResult | str | None]


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
        host_fallback: bool = False,
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
        self._handler_accepts_ctx: dict[str, bool] = {}
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
        params = list(inspect.signature(handler).parameters.values())
        accepts_ctx = False
        if params:
            if len(params) >= 2:
                accepts_ctx = True
            elif params[0].kind == inspect.Parameter.VAR_POSITIONAL:
                accepts_ctx = True
        self._handler_accepts_ctx[name] = accepts_ctx
        if description:
            self.command_docs[name] = description

    def available_commands(self) -> list[str]:
        return sorted(self.commands)

    def _ensure_visible_path(self, path: str) -> None:
        if self.view is None:
            return
        if self.view.path_prefixes is not None:
            normalized = self.vfs._normalize(path)
            if not any(
                normalized.is_relative_to(prefix) or prefix.is_relative_to(normalized)
                for prefix in self.view.path_prefixes
            ):
                raise InvalidOperation(f"Path {path} is hidden for this view")
        try:
            node = self.vfs.get_node(path)
        except NodeNotFound:
            return
        if not self.view.allows(node.policy):
            raise InvalidOperation(f"Path {path} is hidden for this view")
        if self.view.metadata_filters and isinstance(node, VirtualDirectory):
            return
        if not self.view.allows_node(node):
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

    def _run_host_process(
        self,
        command_tokens: list[str],
        path: str | None,
        *,
        stdin: str | None = None,
        env: dict[str, str] | None = None,
    ) -> CommandResult:
        if not command_tokens:
            return CommandResult(stderr="Missing host command", exit_code=2)
        target = str(self.vfs._normalize(path or self.vfs.pwd()))
        self._ensure_visible_path(target)
        sandbox_cwd = PurePosixPath(target)
        merged_env = os.environ.copy()
        if env:
            merged_env.update(env)
        try:
            with self.vfs.materialize("/") as fs_root:
                host_cwd = self._sandbox_to_host_path(fs_root, sandbox_cwd)
                mapped = self._map_command_tokens(command_tokens, fs_root)
                completed = subprocess.run(
                    mapped,
                    cwd=str(host_cwd),
                    input=stdin,
                    capture_output=True,
                    text=True,
                    check=False,
                    env=merged_env,
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
            last_result = self._exec_pipeline(segment)
            if last_result.exit_code != 0:
                return last_result
        return last_result

    def _exec_pipeline(self, command: str) -> CommandResult:
        if not command.strip():
            return CommandResult()
        try:
            pipeline = parse_pipeline(command)
        except ValueError as exc:
            return CommandResult(stderr=str(exc), exit_code=2)
        if not pipeline.commands:
            return CommandResult()

        if len(pipeline.commands) == 1 and pipeline.commands[0].name is None:
            assignments = pipeline.commands[0].assignments
            for key, value in assignments.items():
                self.env[key] = self._expand_vars(value, self.env)
            return CommandResult()

        stdout_pipe = ""
        stderr_chunks: list[str] = []
        for spec in pipeline.commands:
            if spec.name is None:
                return CommandResult(stderr="Missing command in pipeline", exit_code=2)

            env = dict(self.env)
            expanded_assignments = {
                key: self._expand_vars(value, env) for key, value in spec.assignments.items()
            }
            env.update(expanded_assignments)
            name = self._expand_vars(spec.name, env)
            args = self._expand_args(spec.args, env, command_name=name)

            stdin_data = stdout_pipe
            if spec.stdin is not None:
                stdin_path = self._expand_vars(spec.stdin, env)
                stdin_data = self._read_from_path(stdin_path)

            ctx = CommandContext(
                stdin=stdin_data,
                env=env,
                cwd=self.vfs.pwd(),
                vfs=self.vfs,
                view=self.view,
            )
            result = self._run_command(name, args, ctx)
            stderr_chunks.append(result.stderr)
            if result.exit_code != 0:
                return self._enforce_output_limit(
                    CommandResult(
                        stdout=result.stdout,
                        stderr="".join(filter(None, stderr_chunks)),
                        exit_code=result.exit_code,
                    )
                )

            output = result.stdout
            if spec.stdout is not None:
                out_path = self._expand_vars(spec.stdout, env)
                self._write_to_path(out_path, output, append=spec.append)
                output = ""
            stdout_pipe = output

        return self._enforce_output_limit(
            CommandResult(stdout=stdout_pipe, stderr="".join(filter(None, stderr_chunks)))
        )

    def _run_command(self, name: str, args: list[str], ctx: CommandContext) -> CommandResult:
        self.last_command_name = name
        handler = self.commands.get(name)
        if handler is None:
            if self.host_fallback:
                return self._run_host_process([name, *args], None, stdin=ctx.stdin, env=ctx.env)
            return CommandResult(stderr=f"Unknown command: {name}", exit_code=127)
        if self.allowed_commands is not None and name not in self.allowed_commands:
            return CommandResult(stderr=f"Command '{name}' is disabled in this shell", exit_code=1)
        try:
            if self._handler_accepts_ctx.get(name):
                result = handler(args, ctx)
            else:
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

    def _expand_vars(self, token: str, env: dict[str, str]) -> str:
        pattern = re.compile(r"\$(\w+)|\${([^}]+)}")

        def replacer(match: re.Match[str]) -> str:
            name = match.group(1) or match.group(2)
            if not name:
                return ""
            return env.get(name, "")

        return pattern.sub(replacer, token)

    def _expand_args(self, args: list[str], env: dict[str, str], *, command_name: str) -> list[str]:
        expanded: list[str] = []
        skip_next = False
        for arg in args:
            if skip_next:
                expanded.append(arg)
                skip_next = False
                continue
            if command_name in {"bash", "sh"} and arg in {"-c", "-lc"}:
                expanded.append(arg)
                skip_next = True
                continue
            expanded.append(self._expand_vars(arg, env))
        return self._expand_globs(expanded)

    def _expand_globs(self, args: list[str]) -> list[str]:
        expanded: list[str] = []
        for arg in args:
            if any(ch in arg for ch in "*?[]"):
                matches = self.vfs.glob(arg, cwd=self.vfs.pwd(), view=self.view)
                if matches:
                    expanded.extend(matches)
                else:
                    expanded.append(arg)
            else:
                expanded.append(arg)
        return expanded

    def _read_from_path(self, path: str) -> str:
        parsed = self._parse_search_path(path)
        if parsed is None:
            self._ensure_visible_path(path)
            return self.vfs.read_file(path)
        resolved = self._resolve_search_nav(parsed)
        with self.vfs.search_view_context(parsed.query, view=self.view):
            self.vfs._reset_directory(parsed.base)
            return self.vfs.read_file(resolved)

    def _write_to_path(self, path: str, content: str, *, append: bool) -> None:
        parsed = self._parse_search_path(path)
        if parsed is not None:
            raise InvalidOperation("Cannot write to search view paths")
        self._ensure_visible_path(path)
        if append:
            self.vfs.append_file(path, content)
        else:
            self.vfs.write_file(path, content)

    def _resolve_search_nav(self, parsed: "ParsedSearchPath") -> str:
        if parsed.nav is None:
            return str(parsed.base)
        return str(parsed.base.joinpath(parsed.nav))

    def _parse_search_path(self, path: str) -> "ParsedSearchPath | None":
        prefix = self.vfs._search_view_prefix
        if prefix is None or "?" not in path:
            return None
        raw = path
        if not raw.startswith("/"):
            raw = str(PurePosixPath(self.vfs.pwd()).joinpath(raw))
        base_str, tail = raw.split("?", 1)
        base = PurePosixPath(base_str.rstrip("/") or "/")
        if base != prefix:
            return None

        query_part = tail
        nav_part: str | None = None
        if "/" in tail:
            candidate, remainder = tail.split("/", 1)
            if "path=" in candidate and "=" not in remainder:
                query_part = f"{candidate}{remainder}"
                nav_part = None
            else:
                query_part = candidate
                nav_part = remainder

        from urllib.parse import parse_qs, unquote

        params = parse_qs(query_part, keep_blank_values=True)
        query_text = params.get("q", [""])[0]
        regex = self._parse_bool(params.get("regex", ["0"])[0])
        ignore_case = self._parse_bool(params.get("ignore_case", ["0"])[0])
        path_prefix_raw = params.get("path", [None])[0]
        path_prefix = None
        if path_prefix_raw:
            path_prefix = PurePosixPath(unquote(path_prefix_raw))
            if not path_prefix.is_absolute():
                path_prefix = PurePosixPath("/").joinpath(path_prefix)

        query = SearchQuery(
            query=query_text,
            regex=regex,
            ignore_case=ignore_case,
            path_prefix=path_prefix,
        )
        nav = PurePosixPath(nav_part) if nav_part else None
        return ParsedSearchPath(base=base, query=query, nav=nav)

    def _parse_bool(self, value: str) -> bool:
        return value.lower() in {"1", "true", "yes", "on"}

    @contextlib.contextmanager
    def _maybe_search_context(self, path: str) -> Iterator[str]:
        parsed = self._parse_search_path(path)
        if parsed is None:
            yield path
            return
        resolved = self._resolve_search_nav(parsed)
        with self.vfs.search_view_context(parsed.query, view=self.view):
            self.vfs._reset_directory(parsed.base)
            yield resolved

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
            ("echo", self._cmd_echo, "Echo arguments"),
            ("printf", self._cmd_printf, "Format and print text"),
            ("wc", self._cmd_wc, "Count lines or bytes"),
            ("grep", self._cmd_grep, "Search files (non-recursive)"),
            ("rg", self._cmd_rg, "Search files recursively"),
            ("search", self._cmd_search, "Search files (rg-like)"),
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

    def _cmd_pwd(self, _: list[str], ctx: CommandContext | None = None) -> CommandResult:
        return CommandResult(stdout=self.vfs.pwd())

    def _cmd_cd(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
        if len(args) != 1:
            return CommandResult(stderr="cd expects exactly one path", exit_code=2)
        with self._maybe_search_context(args[0]) as resolved:
            self._ensure_visible_path(resolved)
            new_path = self.vfs.cd(resolved)
        return CommandResult(stdout=new_path)

    def _cmd_ls(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
        long = False
        targets: list[str] = []
        for arg in args:
            if arg in ("-l", "--long"):
                long = True
            elif arg.startswith("-"):
                if not self.host_fallback:
                    return CommandResult(stderr=f"ls: unsupported flag {arg}", exit_code=2)
                # fallback for other flags via host
                return self._run_host_process(
                    ["ls", *args],
                    None,
                    stdin=ctx.stdin if ctx else None,
                )
            else:
                targets.append(arg)
        if not targets:
            targets = [self.vfs.pwd()]
        blocks: list[str] = []
        for idx, target in enumerate(targets):
            with self._maybe_search_context(target) as resolved:
                self._ensure_visible_path(resolved)
                try:
                    entries = self.vfs.ls(resolved, view=self.view)
                except InvalidOperation:
                    node = self.vfs.get_node(resolved)
                    if isinstance(node, VirtualDirectory):
                        raise
                    if not node.policy.readable:
                        raise InvalidOperation(f"{node.path()} is not readable") from None
                    if self.view and not self.view.allows_node(node):
                        raise InvalidOperation(f"Path {resolved} is hidden for this view") from None
                    entries = [
                        DirEntry(
                            name=node.name,
                            path=node.path(),
                            is_dir=False,
                            metadata=node.metadata,
                            policy=node.policy,
                        )
                    ]
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

    def _cmd_cat(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
        if not args or args == ["-"]:
            return CommandResult(stdout=ctx.stdin if ctx else "")
        blobs: list[str] = []
        for path in args:
            if path == "-":
                blobs.append(ctx.stdin if ctx else "")
                continue
            with self._maybe_search_context(path) as resolved:
                self._ensure_visible_path(resolved)
                blobs.append(self.vfs.read_file(resolved))
        return CommandResult(stdout="".join(blobs))

    def _cmd_append(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
        if len(args) < 2:
            return CommandResult(stderr="append expects a path and text", exit_code=2)
        path = args[0]
        text = " ".join(args[1:])
        try:
            self._write_to_path(path, text, append=True)
        except InvalidOperation as exc:
            return CommandResult(stderr=str(exc), exit_code=1)
        return CommandResult()

    def _cmd_touch(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
        if not args:
            return CommandResult(stderr="touch expects at least one file", exit_code=2)
        for path in args:
            if self._parse_search_path(path) is not None:
                return CommandResult(stderr="touch: cannot touch search view paths", exit_code=1)
            self._ensure_visible_path(path)
            self.vfs.touch(path)
        return CommandResult()

    def _cmd_mkdir(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
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
            if self._parse_search_path(path) is not None:
                return CommandResult(stderr="mkdir: cannot create search view paths", exit_code=1)
            self._ensure_visible_path(path)
            self.vfs.mkdir(path, parents=parents, exist_ok=parents)
        return CommandResult()

    def _cmd_rm(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
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
            if self._parse_search_path(target) is not None:
                return CommandResult(stderr="rm: cannot remove search view paths", exit_code=1)
            self._ensure_visible_path(target)
            self.vfs.remove(target, recursive=recursive)
        return CommandResult()

    def _cmd_cp(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
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
        if self._parse_search_path(source) is not None or self._parse_search_path(dest) is not None:
            return CommandResult(stderr="cp: cannot use search view paths", exit_code=1)
        self._ensure_visible_path(source)
        self._ensure_visible_path(dest)
        try:
            self.vfs.copy(source, dest, recursive=recursive)
        except (InvalidOperation, NodeNotFound) as exc:
            return CommandResult(stderr=str(exc), exit_code=1)
        return CommandResult()

    def _cmd_mv(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
        if len(args) != 2:
            return CommandResult(stderr="mv expects a source and destination", exit_code=2)
        source, dest = args
        if self._parse_search_path(source) is not None or self._parse_search_path(dest) is not None:
            return CommandResult(stderr="mv: cannot use search view paths", exit_code=1)
        self._ensure_visible_path(source)
        self._ensure_visible_path(dest)
        try:
            self.vfs.move(source, dest)
        except (InvalidOperation, NodeNotFound) as exc:
            return CommandResult(stderr=str(exc), exit_code=1)
        return CommandResult()

    def _cmd_tree(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
        target = args[0] if args else None
        if target:
            with self._maybe_search_context(target) as resolved:
                self._ensure_visible_path(resolved)
                return CommandResult(stdout=self.vfs.tree(resolved, view=self.view))
        return CommandResult(stdout=self.vfs.tree(target, view=self.view))

    def _cmd_write(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
        if not args:
            return CommandResult(stderr="write expects a target path", exit_code=2)
        path = args[0]
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
        try:
            self._write_to_path(path, payload, append=append)
        except InvalidOperation as exc:
            return CommandResult(stderr=str(exc), exit_code=1)
        return CommandResult()

    def _cmd_echo(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
        newline = True
        if args and args[0] == "-n":
            newline = False
            args = args[1:]
        output = " ".join(args)
        if newline:
            output += "\n"
        return CommandResult(stdout=output)

    def _cmd_printf(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
        if not args:
            return CommandResult(stdout="")
        fmt = args[0]
        values = tuple(args[1:])
        try:
            rendered = fmt % values if "%s" in fmt else fmt
        except TypeError:
            rendered = fmt + (" " + " ".join(values) if values else "")
        rendered = (
            rendered.replace("\\n", "\n")
            .replace("\\t", "\t")
            .replace("\\r", "\r")
            .replace("\\\\", "\\")
        )
        return CommandResult(stdout=rendered)

    def _cmd_wc(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
        count_lines = False
        count_bytes = False
        paths: list[str] = []
        for token in args:
            if token == "-l":
                count_lines = True
            elif token == "-c":
                count_bytes = True
            else:
                paths.append(token)
        if not count_lines and not count_bytes:
            count_lines = True
            count_bytes = True

        def format_counts(lines: int, bytes_count: int, label: str | None = None) -> str:
            parts: list[str] = []
            if count_lines:
                parts.append(str(lines))
            if count_bytes:
                parts.append(str(bytes_count))
            if label:
                parts.append(label)
            return " ".join(parts)

        outputs: list[str] = []
        if not paths:
            content = ctx.stdin if ctx else ""
            outputs.append(
                format_counts(len(content.splitlines()), len(content.encode("utf-8")))
            )
            return CommandResult(stdout="\n".join(outputs))

        for path in paths:
            if path == "-":
                content = ctx.stdin if ctx else ""
                outputs.append(
                    format_counts(len(content.splitlines()), len(content.encode("utf-8")), "-")
                )
                continue
            with self._maybe_search_context(path) as resolved:
                try:
                    self._ensure_visible_path(resolved)
                    content = self.vfs.read_file(resolved)
                except (NodeNotFound, InvalidOperation) as exc:
                    return CommandResult(stderr=str(exc), exit_code=1)
            outputs.append(
                format_counts(
                    len(content.splitlines()),
                    len(content.encode("utf-8")),
                    path,
                )
            )
        return CommandResult(stdout="\n".join(outputs))

    def _cmd_grep(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
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
        output: list[str] = []
        if not paths:
            if ctx is not None:
                output = self._search_text(
                    pattern,
                    ctx.stdin,
                    regex=regex,
                    ignore_case=ignore_case,
                    show_numbers=show_numbers,
                )
                return CommandResult(stdout="\n".join(output))
            paths = [self.vfs.pwd()]
        for target in paths:
            if target == "-":
                output.extend(
                    self._search_text(
                        pattern,
                        ctx.stdin if ctx else "",
                        regex=regex,
                        ignore_case=ignore_case,
                        show_numbers=show_numbers,
                    )
                )
                continue
            with self._maybe_search_context(target) as resolved:
                self._ensure_visible_path(resolved)
                output.extend(
                    self._search(
                        pattern,
                        [resolved],
                        recursive=recursive,
                        regex=regex,
                        ignore_case=ignore_case,
                        show_numbers=show_numbers,
                    )
                )
        return CommandResult(stdout="\n".join(output))

    def _cmd_rg(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
        # ripgrep defaults to recursive search
        return self._cmd_grep(["-r"] + args, ctx=ctx)

    def _cmd_search(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
        if not args:
            return CommandResult(stderr="search expects a pattern", exit_code=2)
        regex = False
        ignore_case = False
        show_numbers = True
        paths: list[str] = []
        pattern: str | None = None
        idx = 0
        while idx < len(args):
            token = args[idx]
            if token in ("-i", "--ignore-case"):
                ignore_case = True
            elif token in ("-e", "--regex"):
                regex = True
            elif token in ("-n", "--line-number"):
                show_numbers = True
            elif token == "--no-line-number":
                show_numbers = False
            elif pattern is None:
                pattern = token
            else:
                paths.append(token)
            idx += 1
        if pattern is None:
            return CommandResult(stderr="Missing pattern", exit_code=2)
        if not paths:
            paths = [self.vfs.pwd()]

        results: list[str] = []
        for path in paths:
            normalized = self.vfs._normalize(path)
            query = SearchQuery(
                query=pattern,
                regex=regex,
                ignore_case=ignore_case,
                path_prefix=normalized,
            )
            for match in self.vfs.search(query, view=self.view):
                if show_numbers:
                    results.append(f"{match.path}:{match.line_no}:{match.line_text}")
                else:
                    results.append(f"{match.path}:{match.line_text}")
        return CommandResult(stdout="\\n".join(results))

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
                if self.view and not self.view.allows_node(file_node):
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

    def _search_text(
        self,
        pattern: str,
        text: str,
        *,
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
        for idx, line in enumerate(text.splitlines(), start=1):
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
                prefix = f"{idx}:" if show_numbers else ""
                results.append(f"{prefix}{line}" if prefix else line)
        return results

    def _cmd_python(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
        if not args:
            return CommandResult(stderr="python expects code", exit_code=2)
        if args[0] == "-c" and len(args) >= 2:
            code = " ".join(args[1:])
        else:
            code = " ".join(args)
        result = self.py_exec.run(code)
        return CommandResult(stdout=result.stdout)

    def _cmd_shell_host(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
        # First token is bash/sh command itself; delegate to host with same args
        command = self.last_command_name
        if command is None:
            return CommandResult(stderr="host shell command unavailable", exit_code=1)
        return self._run_host_process(
            [command, *args],
            None,
            stdin=ctx.stdin if ctx else None,
            env=ctx.env if ctx else None,
        )

    def _cmd_host(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
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
        return self._run_host_process(
            command_tokens,
            path,
            stdin=ctx.stdin if ctx else None,
            env=ctx.env if ctx else None,
        )

    def _cmd_help(self, _: list[str], ctx: CommandContext | None = None) -> CommandResult:
        lines = ["Available commands:"]
        for name in self.available_commands():
            desc = self.command_docs.get(name, "")
            if desc:
                lines.append(f"  {name} - {desc}")
            else:
                lines.append(f"  {name}")
        lines.append("Use host <cmd> for full GNU tools.")
        return CommandResult(stdout="\n".join(lines))

    def _cmd_stat(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
        if not args:
            return CommandResult(stderr="stat expects a file path", exit_code=2)
        path = args[0]
        try:
            with self._maybe_search_context(path) as resolved:
                self._ensure_visible_path(resolved)
                node = self.vfs.get_node(resolved)
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

    def _cmd_head(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
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
            if ctx is None:
                return CommandResult(stderr="head: missing file operand", exit_code=1)
            paths = ["-"]

        output: list[str] = []
        for i, path in enumerate(paths):
            if path == "-":
                content = ctx.stdin if ctx else ""
            else:
                with self._maybe_search_context(path) as resolved:
                    self._ensure_visible_path(resolved)
                    try:
                        content = self.vfs.read_file(resolved)
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

    def _cmd_tail(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
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
            if ctx is None:
                return CommandResult(stderr="tail: missing file operand", exit_code=1)
            paths = ["-"]

        output: list[str] = []
        for i, path in enumerate(paths):
            if path == "-":
                content = ctx.stdin if ctx else ""
            else:
                with self._maybe_search_context(path) as resolved:
                    self._ensure_visible_path(resolved)
                    try:
                        content = self.vfs.read_file(resolved)
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

    def _cmd_find(self, args: list[str], ctx: CommandContext | None = None) -> CommandResult:
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
            if self.view and not self.view.allows_node(node):
                return
            yield (node.path(), node)
            if isinstance(node, VirtualDirectory):
                node.ensure_loaded(self.vfs)
                for child in node.iter_children(self.vfs):
                    yield from walk_visible(child)

        results: list[str] = []
        for start_path in paths:
            try:
                with self._maybe_search_context(start_path) as resolved:
                    self._ensure_visible_path(resolved)
                    start_node = self.vfs.get_node(resolved)
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
