def helper(n):
    return n * 2


def dead_function():
    """Never called anywhere."""
    return "dead"


class UnusedClass:
    def __init__(self):
        self.value = 1


class UsedClass:
    def __init__(self):
        self.value = 2
