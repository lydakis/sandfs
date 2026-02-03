"""Search helpers for sandfs."""

from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import PurePosixPath
from typing import Iterable


@dataclass(frozen=True)
class SearchQuery:
    query: str
    regex: bool = False
    ignore_case: bool = False
    path_prefix: PurePosixPath | None = None


@dataclass(frozen=True)
class SearchResult:
    path: PurePosixPath
    line_no: int
    line_text: str


class FullTextIndex:
    def __init__(self) -> None:
        self._files: dict[PurePosixPath, str] = {}

    def clear(self) -> None:
        self._files.clear()

    def build(self, entries: Iterable[tuple[PurePosixPath, str]]) -> None:
        self._files.clear()
        for path, content in entries:
            self._files[path] = content

    def index_file(self, path: PurePosixPath, content: str) -> None:
        self._files[path] = content

    def remove_file(self, path: PurePosixPath) -> None:
        self._files.pop(path, None)

    def search(self, query: SearchQuery) -> list[SearchResult]:
        results: list[SearchResult] = []
        flags = re.MULTILINE
        if query.ignore_case:
            flags |= re.IGNORECASE
        compiled = re.compile(query.query, flags) if query.regex else None
        lowered = query.query.lower() if query.ignore_case and not query.regex else None
        for path, content in self._files.items():
            if query.path_prefix and not path.is_relative_to(query.path_prefix):
                continue
            for line_no, line in enumerate(content.splitlines(), start=1):
                matched = False
                if query.regex:
                    if compiled and compiled.search(line):
                        matched = True
                elif query.ignore_case:
                    if lowered and lowered in line.lower():
                        matched = True
                else:
                    if query.query in line:
                        matched = True
                if matched:
                    results.append(SearchResult(path=path, line_no=line_no, line_text=line))
        return results


__all__ = ["SearchQuery", "SearchResult", "FullTextIndex"]
