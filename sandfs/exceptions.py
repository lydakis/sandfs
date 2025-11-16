"""Custom exceptions for sandfs."""

from __future__ import annotations


class SandboxError(RuntimeError):
    """Base error for the sandbox."""


class NodeNotFound(SandboxError):
    """Raised when a node cannot be resolved."""


class NodeExists(SandboxError):
    """Raised when attempting to create a node that already exists."""


class InvalidOperation(SandboxError):
    """Raised when an operation cannot be completed."""


class ProviderError(SandboxError):
    """Raised when a provider callable fails."""


__all__ = [
    "SandboxError",
    "NodeNotFound",
    "NodeExists",
    "InvalidOperation",
    "ProviderError",
]
