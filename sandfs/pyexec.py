"""Helper to execute Python snippets inside the sandbox."""

from __future__ import annotations

import contextlib
import io
from dataclasses import dataclass
from typing import Any

from .vfs import VirtualFileSystem


_ALLOWED_BUILTINS = {
    "abs": abs,
    "all": all,
    "any": any,
    "bool": bool,
    "dict": dict,
    "enumerate": enumerate,
    "float": float,
    "int": int,
    "len": len,
    "list": list,
    "map": map,
    "max": max,
    "min": min,
    "print": print,
    "range": range,
    "repr": repr,
    "reversed": reversed,
    "round": round,
    "set": set,
    "sorted": sorted,
    "str": str,
    "sum": sum,
    "tuple": tuple,
    "zip": zip,
}


@dataclass
class PythonExecutionResult:
    stdout: str
    globals: dict[str, Any]


class PythonExecutor:
    def __init__(
        self,
        vfs: VirtualFileSystem,
        *,
        builtins: dict[str, Any] | None = None,
        globals_template: dict[str, Any] | None = None,
    ) -> None:
        self.vfs = vfs
        self._builtins = dict(_ALLOWED_BUILTINS)
        if builtins:
            self._builtins.update(builtins)
        base = {"vfs": vfs, "fs": vfs}
        if globals_template:
            base.update(globals_template)
        self._globals_template = base

    def run(
        self,
        code: str,
        *,
        filename: str = "<sandbox>",
        extra_globals: dict[str, Any] | None = None,
    ) -> PythonExecutionResult:
        env: dict[str, Any] = dict(self._globals_template)
        if extra_globals:
            env.update(extra_globals)
        env["__builtins__"] = dict(self._builtins)
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            exec(compile(code, filename, "exec"), env, env)
        env.pop("__builtins__", None)
        return PythonExecutionResult(stdout=stdout.getvalue(), globals=env)


__all__ = ["PythonExecutor", "PythonExecutionResult"]
