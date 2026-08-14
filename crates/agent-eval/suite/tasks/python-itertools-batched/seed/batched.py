"""Seed: dumps the whole iterable into one batch. CPython recipe is the fix."""


def batched(iterable, n):
    if n < 1:
        raise ValueError("n must be at least one")
    return [list(iterable)]
