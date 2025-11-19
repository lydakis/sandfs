"""Import command modules for their registration side-effects."""

# Re-exporting modules isn't required; importing ensures registration happens.
from . import file_ops as _file_ops  # noqa: F401
from . import host as _host  # noqa: F401
from . import meta as _meta  # noqa: F401
from . import navigation as _navigation  # noqa: F401
from . import python_cmds as _python_cmds  # noqa: F401
from . import search as _search  # noqa: F401
from . import text as _text  # noqa: F401

__all__ = []
