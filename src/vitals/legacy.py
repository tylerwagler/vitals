"""``GET /metrics.json``: the snapshot pulsar-gui's cluster view reads.

Its shape is the reference sparkview-exporter 0.6.0's, as pulsar-gui's
``tests/fixtures/sparkview-*.json`` capture it, and it does not change: new
data goes in the /v1 API. Readings are taken when a request arrives and
shared by requests within 250 ms.

A reading that cannot be taken is left out or null, never zero: pulsar-gui
treats a zero as a reading.
"""

from __future__ import annotations

import glob
import json
import os
import platform
import re
import threading
import time
from datetime import datetime
from typing import Any

import psutil

from . import __version__
from .collect.gpu import CLOCK_THRESHOLD_MHZ, EVENT_FIELDS, PROBLEM_BITS, ClockCap, Nvml, clock_status, decode_reasons, gpu_memory
from .collect.memory import psi_level
from .collect.network import rdma, rdma_netlink_counters
from .collect.sensors import Spbm
from .sysfs import read_int, read_text, scaled


class Gpus:
    """The reference exporter's per-GPU fields."""

    def __init__(self, nv: Nvml) -> None:
        self.nv = nv
        self._procs: dict[int, psutil.Process] = {}

    def driver(self) -> dict[str, str]:
        if not self.nv.init():
            return {"driver": "", "cuda": ""}
        versions = self.nv.versions()
        return {"driver": versions["driver"] or "", "cuda": versions["cuda"] or ""}

    def collect(self, vm: Any, clock_cap: dict[str, Any] | None) -> tuple[list[dict], list[dict]]:
        gpus, processes = [], []
        for index, handle in self.nv.handles():
            gpu, procs = self._one(index, handle, vm, clock_cap)
            gpus.append(gpu)
            processes.extend(procs)
        return gpus, processes

    def _one(self, index: int, h: Any, vm: Any, clock_cap: dict[str, Any] | None) -> tuple[dict, list[dict]]:
        call, nv = self.nv.call, self.nv
        name = call("nvmlDeviceGetName", h)
        name = name.decode() if isinstance(name, bytes) else name
        util = call("nvmlDeviceGetUtilizationRates", h)
        util_pct = util.gpu if util is not None else None
        power_mw = call("nvmlDeviceGetPowerUsage", h)

        uma, mem_total, mem_used = gpu_memory(call("nvmlDeviceGetMemoryInfo", h), vm)

        gpu: dict[str, Any] = {
            "index": index,
            "name": name,
            "utilization_percent": util_pct,
            "temperature_celsius": call("nvmlDeviceGetTemperature", h, nv.const("NVML_TEMPERATURE_GPU")),
            "power_watts": None if power_mw is None else round(power_mw / 1000, 3),
            "is_uma": uma,
            "memory_total_bytes": mem_total,
            "memory_used_bytes": mem_used,
            "throttle": self._throttle(h, util_pct),
            "identity": self._identity(h),
            "clock_event_seconds": self._events(h),
            "clock_cap": clock_cap,
        }
        return gpu, self._processes(h)

    def _throttle(self, h: Any, util_pct: int | None) -> dict[str, Any]:
        call, nv = self.nv.call, self.nv
        clk = call("nvmlDeviceGetClockInfo", h, nv.const("NVML_CLOCK_GRAPHICS"))
        clk_max = call("nvmlDeviceGetMaxClockInfo", h, nv.const("NVML_CLOCK_GRAPHICS"))
        pstate_no = call("nvmlDeviceGetPerformanceState", h)
        raw = call("nvmlDeviceGetCurrentClocksEventReasons", h)
        if clk is None or clk_max is None or pstate_no is None or raw is None:
            return {"available": False}
        pstate = f"P{pstate_no}"
        problem = bool(raw & PROBLEM_BITS)
        status, locked = clock_status(float(clk), float(clk_max), util_pct or 0, pstate, problem)
        return {
            "available": True,
            "status": status,
            "clk_mhz": float(clk),
            "clk_max_mhz": float(clk_max),
            "clk_threshold_mhz": CLOCK_THRESHOLD_MHZ,
            "pstate": pstate,
            "util_pct": util_pct,
            "throttle_raw": raw,
            "throttle_reasons": decode_reasons(raw),
            "problem_throttle": problem,
            "locked": locked,
        }

    def _identity(self, h: Any) -> dict[str, Any]:
        def text(value: Any) -> Any:
            return value.decode() if isinstance(value, bytes) else value

        pci = self.nv.call("nvmlDeviceGetPciInfo", h)
        return {
            "uuid": text(self.nv.call("nvmlDeviceGetUUID", h)),
            "pci_bus_id": text(pci.busId) if pci is not None else None,
            "vbios": text(self.nv.call("nvmlDeviceGetVbiosVersion", h)),
        }

    def _events(self, h: Any) -> dict[str, float]:
        nv = self.nv
        wanted = {key: nv.const(const) for key, const in EVENT_FIELDS.items() if nv.const(const) is not None}
        values = nv.call("nvmlDeviceGetFieldValues", h, list(wanted.values()))
        if values is None:
            return {}
        return {
            key: round(v.value.ullVal / 1e9, 6)
            for key, v in zip(wanted, values)
            if v.nvmlReturn == nv.lib.NVML_SUCCESS
        }

    def _processes(self, h: Any) -> list[dict[str, Any]]:
        running = []
        for fn in ("nvmlDeviceGetComputeRunningProcesses", "nvmlDeviceGetGraphicsRunningProcesses"):
            running.extend(self.nv.call(fn, h) or [])
        out, seen = [], set()
        for p in running:
            if p.pid in seen:
                continue
            seen.add(p.pid)
            gpu_mem = getattr(p, "usedGpuMemory", None)
            row: dict[str, Any] = {"pid": p.pid, "user": None, "cmd": None,
                                   "gpu_mem": gpu_mem if isinstance(gpu_mem, int) else None,
                                   "cpu_pct": None}
            try:
                # Keep the Process so cpu_percent measures since the last scrape.
                proc = self._procs.get(p.pid)
                if proc is None or not proc.is_running():
                    proc = self._procs[p.pid] = psutil.Process(p.pid)
                    proc.cpu_percent(None)
                row["user"] = proc.username()
                row["cmd"] = proc.name()
                row["cpu_pct"] = proc.cpu_percent(None)
            except (psutil.Error, KeyError):
                pass
            out.append(row)
        for pid in set(self._procs) - seen:
            del self._procs[pid]
        return sorted(out, key=lambda r: r["gpu_mem"] or 0, reverse=True)


def pressure(path: str = "/proc/pressure/memory") -> dict[str, Any]:
    text = read_text(path)
    if text is None:
        return {"available": False}
    avg: dict[str, float] = {}
    for line in text.splitlines():
        kind, *fields = line.split()
        for field_ in fields:
            key, _, value = field_.partition("=")
            if key == "avg10":
                avg[kind] = float(value)
    some, full = avg.get("some", 0.0), avg.get("full", 0.0)
    return {"available": True, "some_avg10": some, "full_avg10": full, "level": psi_level(some, full)}


def memory(vm: Any) -> dict[str, Any]:
    sw = psutil.swap_memory()
    return {
        # total - available, not psutil's "used": on a UMA host the page cache
        # the GPU can reclaim is not memory in use.
        "total_bytes": vm.total,
        "used_bytes": vm.total - vm.available,
        "available_bytes": vm.available,
        "percent": vm.percent,
        "swap_total_bytes": sw.total,
        "swap_used_bytes": sw.used,
        "swap_percent": sw.percent,
    }


CPU_TEMP_CHIPS = ("coretemp", "k10temp", "cpu_thermal", "acpitz")


def cpu(hwmon_temps: list[dict[str, Any]]) -> dict[str, Any]:
    # Non-blocking: utilization since the previous call (the previous scrape).
    per_core = psutil.cpu_percent(None, percpu=True)
    # The hottest sensor of the first CPU chip present, as sparkview does, but
    # from the hwmon readings already taken: psutil.sensors_temperatures()
    # re-reads every chip, and the NIC and Wi-Fi ones answer slowly.
    temp = None
    for chip in CPU_TEMP_CHIPS:
        readings = [t["celsius"] for t in hwmon_temps
                    if t["chip"] == chip and t["celsius"] is not None]
        if readings:
            temp = max(readings)
            break
    return {
        "utilization_percent": sum(per_core) / len(per_core) if per_core else None,
        "cores_total": psutil.cpu_count(),
        "cores_active": sum(1 for c in per_core if c > 0),
        "temperature_celsius": temp,
    }


def thermal_zones(root: str = "/sys/class/thermal") -> list[dict[str, Any]]:
    zones = sorted(glob.glob(os.path.join(root, "thermal_zone*")),
                   key=lambda p: int(re.sub(r"\D", "", os.path.basename(p)) or 0))
    return [
        {"zone": os.path.basename(z), "type": read_text(f"{z}/type"),
         "celsius": scaled(f"{z}/temp", 1e-3, 2)}
        for z in zones
    ]


def hwmon_temperatures(root: str = "/sys/class/hwmon") -> list[dict[str, Any]]:
    out = []
    for hw in sorted(glob.glob(os.path.join(root, "hwmon*")),
                     key=lambda p: int(re.sub(r"\D", "", os.path.basename(p)) or 0)):
        chip = read_text(f"{hw}/name")
        for inp in sorted(glob.glob(f"{hw}/temp*_input")):
            sensor = inp[: -len("_input")]
            out.append({
                "chip": chip,
                "sensor": read_text(f"{sensor}_label") or os.path.basename(sensor),
                "celsius": scaled(inp, 1e-3, 2),
            })
    return out


def network(root: str = "/sys/class/net") -> list[dict[str, Any]]:
    """Physical interfaces only: bridges, veths and docker0 have no device."""
    out = []
    for path in sorted(glob.glob(os.path.join(root, "*"))):
        if not os.path.exists(f"{path}/device"):
            continue
        stats = f"{path}/statistics"
        speed = read_int(f"{path}/speed")
        driver = os.path.basename(os.path.realpath(f"{path}/device/driver"))
        out.append({
            "iface": os.path.basename(path),
            "state": (read_text(f"{path}/operstate") or "unknown").upper(),
            "speed_gbps": speed / 1000 if speed is not None and speed > 0 else None,
            "rx_bytes": read_int(f"{stats}/rx_bytes"),
            "tx_bytes": read_int(f"{stats}/tx_bytes"),
            "rx_packets": read_int(f"{stats}/rx_packets"),
            "tx_packets": read_int(f"{stats}/tx_packets"),
            "rx_errors": read_int(f"{stats}/rx_errors"),
            "tx_errors": read_int(f"{stats}/tx_errors"),
            "rx_dropped": read_int(f"{stats}/rx_dropped"),
            "cx7": driver == "mlx5_core",
        })
    return out


def uptime_seconds() -> float | None:
    text = read_text("/proc/uptime")
    return float(text.split()[0]) if text else None


def format_uptime(seconds: float | None) -> str:
    if seconds is None:
        return ""
    minutes = int(seconds // 60)
    days, rem = divmod(minutes, 24 * 60)
    hours, mins = divmod(rem, 60)
    return f"{days}d {hours}h {mins}m" if days else f"{hours}h {mins}m"


class Exporter:
    def __init__(self, nv: Nvml, clock_cap: ClockCap) -> None:
        self.gpus = Gpus(nv)
        self.spbm = Spbm()
        self.clock_cap = clock_cap
        self._lock = threading.Lock()
        self._cached: tuple[float, bytes] | None = None
        psutil.cpu_percent(None, percpu=True)  # prime the first interval

    def snapshot(self) -> dict[str, Any]:
        started = time.perf_counter()
        vm = psutil.virtual_memory()
        gpus, processes = self.gpus.collect(vm, self.clock_cap.get())
        up = uptime_seconds()
        hwmon_temps = hwmon_temperatures()
        return {
            "hostname": platform.node(),
            "exporter_version": __version__,
            "timestamp": time.time(),
            "uptime_seconds": up,
            "info": {
                "time": datetime.now().strftime("%I:%M:%S %p"),
                "uptime": format_uptime(up),
                "kernel": platform.release(),
                **self.gpus.driver(),
                "gpu_name": gpus[0]["name"] if gpus else "",
            },
            "gpus": gpus,
            "memory": memory(vm),
            "pressure": pressure(),
            "cpu": cpu(hwmon_temps),
            "spbm": self.spbm.collect(),
            "thermal_zones": thermal_zones(),
            "hwmon_temperatures": hwmon_temps,
            "network": network(),
            "rdma": rdma(hw_counters=rdma_netlink_counters()),
            "processes": processes,
            "scrape_duration_seconds": round(time.perf_counter() - started, 4),
        }

    def body(self, max_age_s: float = 0.25) -> bytes:
        """The JSON body, shared by requests that land within ``max_age_s``.

        Several dashboards polling at once then cost one scrape, and the
        rate-style readings (CPU, process CPU) keep a sensible interval.
        """
        with self._lock:
            now = time.monotonic()
            if self._cached is None or now - self._cached[0] > max_age_s:
                self._cached = (now, json.dumps(self.snapshot()).encode())
            return self._cached[1]
