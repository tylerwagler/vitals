"""The process table, on request: the top N by CPU, memory or GPU memory.

Names and owners only, never command lines: those can carry secrets (an
--api-key, a token in a URL), and this is served to the whole LAN.
"""

from __future__ import annotations

import threading
import time
from collections import Counter
from typing import Any

import psutil

ATTRS = ["pid", "ppid", "name", "username", "status", "num_threads", "nice",
         "create_time", "memory_info", "memory_percent"]
SORTS = ("cpu", "memory", "gpu")
# A first table has no CPU interval to measure over; it waits this long for one.
PRIME_S = 0.25
# Past this the cached Process objects are stale, so the table re-primes.
STALE_S = 60.0


class Processes:
    def __init__(self) -> None:
        self._procs: dict[tuple[int, float], psutil.Process] = {}
        self._at = 0.0
        self._lock = threading.Lock()

    def _measure(self) -> dict[tuple[int, float], float]:
        cpu, live = {}, {}
        for proc in psutil.process_iter():
            try:
                key = (proc.pid, proc.create_time())
                proc = self._procs.get(key, proc)
                cpu[key] = proc.cpu_percent(None)
                live[key] = proc
            except psutil.Error:
                continue
        self._procs = live
        return cpu

    def table(self, top: int, sort: str, gpu_mem: dict[int, list[dict[str, Any]]]) -> dict[str, Any]:
        with self._lock:
            now = time.monotonic()
            if not self._procs or now - self._at > STALE_S:
                self._measure()
                time.sleep(PRIME_S)
                now = time.monotonic()
            interval = now - self._at if self._at and now - self._at <= STALE_S else PRIME_S
            cpu = self._measure()
            self._at = now
            procs = dict(self._procs)

        rows, states, threads = [], Counter(), 0
        for key, proc in procs.items():
            try:
                info = proc.as_dict(ATTRS)
            except psutil.Error:
                continue
            states[info["status"]] += 1
            threads += info["num_threads"] or 0
            mem = info["memory_info"]
            gpus = gpu_mem.get(info["pid"], [])
            rows.append({
                "pid": info["pid"], "ppid": info["ppid"], "name": info["name"],
                "user": info["username"], "state": info["status"], "nice": info["nice"],
                "threads": info["num_threads"], "started_at": info["create_time"],
                "cpu_percent": cpu.get(key),
                "memory_rss_bytes": mem.rss if mem else None,
                "memory_percent": round(info["memory_percent"], 3) if info["memory_percent"] is not None else None,
                "gpu_memory_bytes": sum(g["bytes"] or 0 for g in gpus) if gpus else None,
                "gpus": [g["gpu"] for g in gpus],
            })
        field = {"cpu": "cpu_percent", "memory": "memory_rss_bytes", "gpu": "gpu_memory_bytes"}[sort]
        rows.sort(key=lambda r: r[field] or 0, reverse=True)
        return {
            "interval_s": round(interval, 3),
            "sort": sort,
            "totals": {"processes": len(rows), "threads": threads, "by_state": dict(states)},
            "processes": rows[:top],
        }
