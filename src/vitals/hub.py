"""Where readings come from: one live sampler and one static inventory,
shared by every request and stream.

Live samples are taken on demand, or by a sampler thread that runs only
while a stream is subscribed, at the fastest interval any subscriber asked
for. Either way every sample goes through one ``Rates``, so each rate is over
the interval since the sample before it, and the sample says how long that
was (``interval_s``).

The static inventory (what the machine is and has) is collected at most once
a minute. Its ``generation`` is a hash of the contents, so it changes only
when something does: a card swapped, a link renegotiated, a disk added.
"""

from __future__ import annotations

import hashlib
import importlib.metadata
import json
import logging
import os
import platform
import pwd
import socket
import sys
import threading
import time
from types import SimpleNamespace
from typing import Any, Callable

import psutil

from . import __version__
from .collect import cpu, gpu, host, memory, network, pci, processes, sensors, storage
from .rates import Rates

logger = logging.getLogger("vitals")

LIVE_SECTIONS = ("host", "cpu", "memory", "pressure", "gpus", "sensors", "storage", "network", "rdma")
STATIC_SECTIONS = ("host", "exporter", "cpu", "memory", "gpus", "pci", "storage", "network", "sensors")
ENVELOPE = ("hostname", "seq", "time", "interval_s", "duration_s", "static_generation")
# Readings that move on their own and are not a change of hardware: a GPU's
# PCIe link downtrains when idle. Left out of the static generation.
VOLATILE_STATIC = frozenset({"current_speed", "current_width", "collected_at", "collection_seconds"})
STATIC_TTL_S = 60.0


def _stable(obj: Any) -> Any:
    if isinstance(obj, dict):
        return {k: _stable(v) for k, v in obj.items() if k not in VOLATILE_STATIC}
    if isinstance(obj, list):
        return [_stable(v) for v in obj]
    return obj


def generation_of(doc: dict[str, Any]) -> str:
    blob = json.dumps(_stable(doc), sort_keys=True, default=str).encode()
    return hashlib.sha1(blob).hexdigest()[:12]


def pick(doc: dict[str, Any], sections: tuple[str, ...] | None, envelope: tuple[str, ...]) -> dict[str, Any]:
    if sections is None:
        return doc
    return {k: doc[k] for k in (*envelope, *sections) if k in doc}


def package_version(name: str, module: str | None = None) -> str | None:
    try:
        return importlib.metadata.version(name)
    except importlib.metadata.PackageNotFoundError:
        mod = sys.modules.get(module or name)
        return getattr(mod, "__version__", None)


class Hub:
    def __init__(self) -> None:
        self.nv = gpu.Nvml()
        self.clock_cap = gpu.ClockCap()
        self.gpu_procs = gpu.GpuProcesses()
        self.systemd = host.Systemd()
        self.cpu_throttle = cpu.ThrottleCounts()
        self.spbm = sensors.Spbm()
        self.statvfs = storage.Statvfs()
        self.processes = processes.Processes()
        self.rates = Rates()
        self.started_at = time.time()
        self.listen: str | None = None
        self._failed: set[str] = set()

        self._sample_lock = threading.Lock()
        self.latest: dict[str, Any] | None = None
        self.latest_at = 0.0
        self.seq = 0

        self._static_lock = threading.Lock()
        self._static: dict[str, Any] | None = None
        self._static_at = 0.0
        self.static_generation: str | None = None
        self._static_refreshing = False

        # Streams: subscriber id -> interval. The condition is notified on
        # every publish, every static change, and on shutdown.
        self.cond = threading.Condition()
        self.subscribers: dict[int, float] = {}
        self._next_id = 0
        self._sampler: threading.Thread | None = None
        self.stopping = False
        self._encoded: dict[tuple, bytes] = {}

    # -- sections -----------------------------------------------------------

    def _safe(self, name: str, fn: Callable[[], Any]) -> Any:
        """One section failing is that section's error, not a failed sample."""
        try:
            value = fn()
        except Exception as exc:  # noqa: BLE001 - a collector bug must not take the rest down
            if name not in self._failed:
                self._failed.add(name)
                logger.exception("collecting %s failed", name)
            return {"error": f"{type(exc).__name__}: {exc}"}
        self._failed.discard(name)
        return value

    def take(self) -> dict[str, Any]:
        """A fresh live sample of every section."""
        with self._sample_lock:
            started = time.perf_counter()
            rates = self.rates
            rates.begin(time.monotonic())
            hw = self._safe("sensors.hwmon", sensors.hwmon_live)
            hw = hw if isinstance(hw, list) else []
            mem = self._safe("memory", lambda: memory.live(rates))
            vm = SimpleNamespace(total=mem.get("total_bytes") or 0, available=mem.get("available_bytes") or 0)
            doc: dict[str, Any] = {
                "hostname": socket.gethostname(),
                "seq": 0,
                "time": time.time(),
                "interval_s": None if rates.dt is None else round(rates.dt, 4),
                "static_generation": self.static_generation,
                "host": self._safe("host", lambda: host.live(rates, self.systemd)),
                "cpu": self._safe("cpu", lambda: cpu.live(rates, hw, self.cpu_throttle)),
                "memory": mem,
                "pressure": self._safe("pressure", lambda: memory.pressure(rates)),
                "gpus": self._safe("gpus", lambda: gpu.live(self.nv, rates, vm, self.clock_cap, self.gpu_procs)),
                "sensors": self._safe("sensors", lambda: sensors.live(hw, self.spbm)),
                "storage": self._safe("storage", lambda: storage.live(rates, self.statvfs)),
                "network": self._safe("network", lambda: network.live(rates)),
                "rdma": self._safe("rdma", lambda: network.rdma_live(rates)),
            }
            rates.end()
            if self.static_generation is None:
                self._refresh_static_in_background()
            doc["duration_s"] = round(time.perf_counter() - started, 4)
            with self.cond:
                self.seq += 1
                doc["seq"] = self.seq
                self.latest, self.latest_at = doc, time.monotonic()
                self._encoded = {}
                self.cond.notify_all()
            return doc

    def live(self, max_age_s: float = 0.25) -> dict[str, Any]:
        """The latest sample if it is fresh enough, else a new one.

        While a stream runs, its samples are fresh for one of its intervals,
        so polling alongside a stream costs nothing and does not shorten the
        stream's rate intervals.
        """
        with self.cond:
            if self.subscribers:
                max_age_s = max(max_age_s, min(self.subscribers.values()) * 1.5)
            if self.latest is not None and time.monotonic() - self.latest_at <= max_age_s:
                return self.latest
        return self.take()

    def encoded(self, doc: dict[str, Any], sections: tuple[str, ...] | None) -> bytes:
        """A sample's JSON, encoded once per section set however many streams want it."""
        key = (doc["seq"], sections)
        with self.cond:
            cached = self._encoded.get(key) if doc is self.latest else None
        if cached is not None:
            return cached
        body = json.dumps(pick(doc, sections, ENVELOPE), separators=(",", ":")).encode()
        with self.cond:
            if doc is self.latest:
                self._encoded[key] = body
        return body

    # -- static -------------------------------------------------------------

    def exporter_static(self) -> dict[str, Any]:
        try:
            user = pwd.getpwuid(os.getuid()).pw_name
        except KeyError:
            user = str(os.getuid())
        archive = sys.argv[0] if sys.argv and sys.argv[0].endswith(".pyz") else None
        return {
            "name": "vitals",
            "version": __version__,
            "python": platform.python_version(),
            "pid": os.getpid(),
            "user": user,
            "started_at": self.started_at,
            "listen": self.listen,
            "archive": archive,
            "psutil": psutil.__version__,
            "nvidia_ml_py": package_version("nvidia-ml-py", "pynvml") if self.nv.ready else None,
            "nvml_ready": self.nv.ready,
            "nvml_error": self.nv.error,
            "live_sections": list(LIVE_SECTIONS),
            "static_sections": list(STATIC_SECTIONS),
        }

    def _collect_static(self) -> dict[str, Any]:
        started = time.perf_counter()
        mem = self._safe("static.memory", memory.static)
        gpus = self._safe("static.gpus", lambda: gpu.static(self.nv, mem.get("total_bytes")))
        sections = {
            "host": self._safe("static.host", host.static),
            "exporter": self.exporter_static(),  # after gpus: it reports how NVML fared
            "cpu": self._safe("static.cpu", cpu.static),
            "memory": mem,
            "gpus": gpus,
            "pci": self._safe("static.pci", pci.static),
            "storage": self._safe("static.storage", storage.static),
            "network": self._safe("static.network", network.static),
            "sensors": self._safe("static.sensors", sensors.static),
        }
        gen = generation_of(sections)
        return {
            "hostname": socket.gethostname(),
            "generation": gen,
            "collected_at": time.time(),
            "collection_seconds": round(time.perf_counter() - started, 4),
            **sections,
        }

    def static(self, refresh: bool = False, max_age_s: float = STATIC_TTL_S) -> dict[str, Any]:
        with self._static_lock:
            now = time.monotonic()
            if refresh or self._static is None or now - self._static_at > max_age_s:
                doc = self._collect_static()
                self._static, self._static_at = doc, time.monotonic()
                if doc["generation"] != self.static_generation:
                    with self.cond:
                        changed = self.static_generation is not None
                        self.static_generation = doc["generation"]
                        self.cond.notify_all()
                    if changed:
                        logger.info("static inventory changed: generation %s", doc["generation"])
            return self._static

    def _refresh_static_in_background(self) -> None:
        # Not under _static_lock: a collection in progress holds it, and the
        # sampler must not wait a whole collection to find that out.
        with self.cond:
            if self._static_refreshing or (self._static is not None
                                           and time.monotonic() - self._static_at <= STATIC_TTL_S):
                return
            self._static_refreshing = True

        def work() -> None:
            try:
                self.static()
            finally:
                self._static_refreshing = False

        threading.Thread(target=work, name="vitals-static", daemon=True).start()

    # -- processes ----------------------------------------------------------

    def process_table(self, top: int, sort: str) -> dict[str, Any]:
        table = self.processes.table(top, sort, gpu.process_memory(self.nv))
        return {"hostname": socket.gethostname(), "time": time.time(), **table}

    # -- streams ------------------------------------------------------------

    def subscribe(self, interval: float) -> int:
        with self.cond:
            self._next_id += 1
            sid = self._next_id
            self.subscribers[sid] = interval
            if self._sampler is None:
                self._sampler = threading.Thread(target=self._run, name="vitals-sampler", daemon=True)
                self._sampler.start()
            self.cond.notify_all()
            return sid

    def unsubscribe(self, sid: int) -> None:
        with self.cond:
            self.subscribers.pop(sid, None)
            self.cond.notify_all()

    def stop(self) -> None:
        with self.cond:
            self.stopping = True
            self.cond.notify_all()

    def _run(self) -> None:
        """Sample at the fastest subscribed interval while anyone listens."""
        while True:
            with self.cond:
                if not self.subscribers or self.stopping:
                    self._sampler = None
                    return
            tick = time.monotonic()
            try:
                self.take()
            except Exception:  # noqa: BLE001 - keep sampling; take() already isolates sections
                logger.exception("sample failed")
            self._refresh_static_in_background()
            with self.cond:
                while self.subscribers and not self.stopping:
                    interval = min(self.subscribers.values())
                    remaining = tick + interval - time.monotonic()
                    if remaining <= 0:
                        break
                    self.cond.wait(remaining)
