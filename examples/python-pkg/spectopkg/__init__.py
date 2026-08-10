# package-level dead code: re-exports in __init__.py, __all__, unused members.
from .maths import add, double, unused_helper
from .strings import to_upper, to_lower

__all__ = ["add", "double", "to_upper", "unused_helper"]
