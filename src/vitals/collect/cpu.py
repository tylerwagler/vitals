"""CPUs: what they are (models, clusters, caches, flags, NUMA) and what they
are doing (per-core utilization by kind, frequency, temperature)."""

from __future__ import annotations

import glob
import json
import os
import threading
import time
from collections import defaultdict
from typing import Any

import psutil

from ..rates import Rates
from ..sysfs import cpu_list, natural_key, read_int, read_text, run

CPU_DIR = "/sys/devices/system/cpu"
# Chips whose hottest sensor is "the CPU temperature", most specific first.
CPU_TEMP_CHIPS = ("coretemp", "k10temp", "zenpower", "cpu_thermal", "acpitz")
# Time that is not work. guest/guest_nice are already inside user/nice.
IDLE_FIELDS = ("idle", "iowait")
NOT_SUMMED = ("guest", "guest_nice")


def _num(value: Any) -> float | None:
    try:
        return round(float(value), 1)
    except (TypeError, ValueError):
        return None


def _lscpu_cpus() -> list[dict[str, Any]]:
    """Per-CPU rows from lscpu, which knows every ARM part name."""
    for columns in ("CPU,CORE,SOCKET,NODE,CLUSTER,MAXMHZ,MINMHZ,MODELNAME",
                    "CPU,CORE,SOCKET,NODE,MAXMHZ,MINMHZ,MODELNAME",
                    "CPU,CORE,SOCKET,NODE,MAXMHZ,MINMHZ"):
        out = run(["lscpu", "-J", f"--extended={columns}"])
        if out:
            try:
                return json.loads(out).get("cpus", [])
            except ValueError:
                return []
    return []


def _caches() -> list[dict[str, Any]]:
    out = run(["lscpu", "-J", "-C"])
    try:
        rows = json.loads(out).get("caches", []) if out else []
    except ValueError:
        return []
    return [{"name": r.get("name"), "level": r.get("level"), "type": r.get("type"),
             "one_size": r.get("one-size"), "all_size": r.get("all-size"), "ways": r.get("ways")}
            for r in rows]


def _cpuinfo() -> tuple[str | None, list[str]]:
    """(model name, feature flags) of the first processor in /proc/cpuinfo."""
    model, flags = None, []
    for line in (read_text("/proc/cpuinfo") or "").splitlines():
        key, _, value = line.partition(":")
        key = key.strip()
        if key == "model name" and model is None:
            model = value.strip()
        elif key in ("flags", "Features") and not flags:
            flags = value.split()
    return model, flags


def physical_cores(online: list[int]) -> int | None:
    """Distinct core sibling sets among the CPUs this host (or container) has.

    Not psutil.cpu_count(logical=False): that counts every core of the
    machine, so in a container given 24 of 96 threads it says 48.
    """
    sets = {read_text(f"{CPU_DIR}/cpu{n}/topology/core_cpus_list")
            or read_text(f"{CPU_DIR}/cpu{n}/topology/thread_siblings_list") for n in online}
    sets.discard(None)
    return len(sets) or None


def static() -> dict[str, Any]:
    online = cpu_list(read_text(f"{CPU_DIR}/online"))
    rows = {int(r["cpu"]): r for r in _lscpu_cpus() if "cpu" in r}
    model_name, flags = _cpuinfo()

    clusters: dict[tuple, list[int]] = defaultdict(list)
    for n in online:
        row = rows.get(n, {})
        model = row.get("modelname") or model_name
        top = _num(row.get("maxmhz")) or _num((read_int(f"{CPU_DIR}/cpu{n}/cpufreq/cpuinfo_max_freq") or 0) / 1000) or None
        low = _num(row.get("minmhz")) or _num((read_int(f"{CPU_DIR}/cpu{n}/cpufreq/cpuinfo_min_freq") or 0) / 1000) or None
        clusters[(model, top, low)].append(n)

    packages = {read_int(f"{CPU_DIR}/cpu{n}/topology/physical_package_id") for n in online}
    nodes = []
    for node in sorted(glob.glob("/sys/devices/system/node/node[0-9]*"), key=natural_key):
        total = None
        for line in (read_text(f"{node}/meminfo") or "").splitlines():
            if "MemTotal:" in line:
                total = int(line.split()[-2]) * 1024
        nodes.append({"node": int(os.path.basename(node)[4:]),
                      "cpus": cpu_list(read_text(f"{node}/cpulist")), "memory_total_bytes": total})

    vulns = {os.path.basename(p): read_text(p)
             for p in sorted(glob.glob(f"{CPU_DIR}/vulnerabilities/*"))}
    return {
        "model": model_name or (next(iter(clusters))[0] if clusters else None),
        "logical_cpus": psutil.cpu_count(),
        "physical_cores": physical_cores(online),
        "sockets": len(packages - {None}) or None,
        "online": online,
        "isolated": cpu_list(read_text(f"{CPU_DIR}/isolated")),
        "smt": {"active": read_int(f"{CPU_DIR}/smt/active"), "control": read_text(f"{CPU_DIR}/smt/control")},
        "clusters": [{"model": m, "max_mhz": top, "min_mhz": low, "count": len(cpus), "cpus": cpus}
                     for (m, top, low), cpus in clusters.items()],
        "caches": _caches(),
        "numa_nodes": nodes,
        "frequency_driver": read_text(f"{CPU_DIR}/cpu0/cpufreq/scaling_driver"),
        "governor": read_text(f"{CPU_DIR}/cpu0/cpufreq/scaling_governor"),
        "energy_performance_preference": read_text(f"{CPU_DIR}/cpu0/cpufreq/energy_performance_preference"),
        "boost": read_int(f"{CPU_DIR}/cpufreq/boost"),
        "flags": flags,
        "vulnerabilities": vulns,
    }


def _breakdown(rates: Rates, key: str, times: Any) -> dict[str, float] | None:
    """Percent of the interval in each kind of time, from cumulative seconds."""
    deltas = {f: rates.delta(f"{key}/{f}", v) for f, v in times._asdict().items()}
    if any(v is None for v in deltas.values()):
        return None
    total = sum(v for f, v in deltas.items() if f not in NOT_SUMMED)
    if total <= 0:
        return None
    out = {f: round(100 * v / total, 2) for f, v in deltas.items()}
    out["utilization"] = round(100 - sum(out[f] for f in IDLE_FIELDS if f in out), 2)
    return out


def temperature(hwmon: list[dict[str, Any]]) -> float | None:
    """The hottest sensor of the first CPU chip present."""
    for chip in CPU_TEMP_CHIPS:
        readings = [s["value"] for s in hwmon
                    if s["chip"] == chip and s["kind"] == "temp" and s["value"] is not None]
        if readings:
            return max(readings)
    return None


class ThrottleCounts:
    """x86 thermal-throttle event counters, at most every 10 s.

    Each of those sysfs files is an MSR read on its CPU: 50+ ms for all of a
    24-core host, more than the rest of a sample. The rate is over the
    counters' own interval, not the sample's.
    """

    TTL_S = 10.0

    def __init__(self) -> None:
        self._at = -self.TTL_S
        self._prev: tuple[float, dict[str, int]] | None = None
        self._value: dict[str, Any] | None = None
        self._lock = threading.Lock()

    def get(self) -> dict[str, Any] | None:
        with self._lock:
            now = time.monotonic()
            if now - self._at < self.TTL_S:
                return self._value
            self._at = now
            counts: dict[str, int] = {}
            for kind in ("core", "package"):
                values = [read_int(p) for p in glob.glob(f"{CPU_DIR}/cpu*/thermal_throttle/{kind}_throttle_count")]
                values = [v for v in values if v is not None]
                if values:
                    counts[kind] = sum(values)
            if not counts:
                self._value = None
                return None
            out: dict[str, Any] = {}
            for kind, total in counts.items():
                out[f"{kind}_throttle_events"] = total
                rate = None
                if self._prev is not None and kind in self._prev[1] and total >= self._prev[1][kind]:
                    rate = round((total - self._prev[1][kind]) / (now - self._prev[0]), 3)
                out[f"{kind}_throttle_events_per_s"] = rate
            self._prev, self._value = (now, counts), out
            return out


def live(rates: Rates, hwmon: list[dict[str, Any]], throttle: ThrottleCounts | None = None) -> dict[str, Any]:
    total = _breakdown(rates, "cpu/all", psutil.cpu_times())
    cores = []
    for n, times in enumerate(psutil.cpu_times(percpu=True)):
        b = _breakdown(rates, f"cpu/{n}", times)
        khz = read_int(f"{CPU_DIR}/cpu{n}/cpufreq/scaling_cur_freq")
        cores.append({
            "cpu": n,
            "utilization_percent": b["utilization"] if b else None,
            "user_percent": b["user"] if b else None,
            "system_percent": b["system"] if b else None,
            "iowait_percent": b.get("iowait") if b else None,
            "steal_percent": b.get("steal") if b else None,
            "frequency_mhz": None if khz is None else round(khz / 1000, 1),
        })
    freqs = [c["frequency_mhz"] for c in cores if c["frequency_mhz"] is not None]
    return {
        "utilization_percent": total["utilization"] if total else None,
        "times_percent": total,
        "frequency_mhz": {"min": min(freqs), "max": max(freqs),
                          "mean": round(sum(freqs) / len(freqs), 1)} if freqs else None,
        "temperature_celsius": temperature(hwmon),
        "thermal_throttle": throttle.get() if throttle is not None else None,
        "cores": cores,
    }
