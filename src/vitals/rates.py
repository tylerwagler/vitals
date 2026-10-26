"""Per-second rates from the kernel's cumulative counters.

Every live sample goes through one ``Rates``, so a rate is always over the
interval since the previous sample, whoever asked for it. A counter seen for
the first time, or one that went backwards (a reset, a re-created
interface), has no rate: None, never 0.
"""

from __future__ import annotations


class Rates:
    def __init__(self) -> None:
        self._prev: dict[str, float] = {}
        self._next: dict[str, float] = {}
        self._prev_t: float | None = None
        self.now = 0.0
        self.dt: float | None = None

    def begin(self, now: float) -> None:
        self.now = now
        self.dt = now - self._prev_t if self._prev_t is not None and now > self._prev_t else None
        self._next = {}

    def end(self) -> None:
        self._prev, self._prev_t = self._next, self.now

    def delta(self, key: str, value: float | None) -> float | None:
        if value is None:
            return None
        self._next[key] = value
        prev = self._prev.get(key)
        if prev is None or value < prev or self.dt is None:
            return None
        return value - prev

    def rate(self, key: str, value: float | None, digits: int = 3) -> float | None:
        d = self.delta(key, value)
        return None if d is None or not self.dt else round(d / self.dt, digits)
