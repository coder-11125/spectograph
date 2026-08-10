from .core import do_work, unused_internal
from .helpers import useful, dead_helper


def run():
    return do_work()


def other_entry():
    # Not referenced as a script; only reachable if called.
    return useful()
