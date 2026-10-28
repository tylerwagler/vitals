"""RAM, swap and the kernel's memory policy; and pressure stall information
for every resource the kernel reports it for."""

from __future__ import annotations

import glob
import os
import re
from typing import Any

from ..rates import Rates
from ..sysfs import bracketed, read_int, read_text

VMSTAT_RATES = ("pgfault", "pgmajfault", "pswpin", "pswpout", "pgpgin", "pgpgout",
                "pgsteal_kswapd", "pgsteal_direct", "workingset_refault_file")
PSI_RESOURCES = ("cpu", "memory", "io", "irq")


def snake(key: str) -> str:
    """'SReclaimable' -> 'sreclaimable', 'Active(anon)' -> 'active_anon',
    'HugePages_Total' -> 'huge_pages_total'."""
    key = key.replace("(", "_").replace(")", "")
    key = re.sub(r"(?<=[a-z0-9])(?=[A-Z])", "_", key).lower()
    return re.sub(r"_+", "_", key).strip("_")


def meminfo(text: str | None = None) -> dict[str, int]:
    """Every /proc/meminfo line: kB fields as *_bytes, counts as they are."""
    out: dict[str, int] = {}
    for line in (text if text is not None else read_text("/proc/meminfo") or "").splitlines():
        key, _, rest = line.partition(":")
        parts = rest.split()
        if not parts or not parts[0].isdigit():
            continue
        if len(parts) > 1 and parts[1] == "kB":
            out[f"{snake(key)}_bytes"] = int(parts[0]) * 1024
        else:
            out[snake(key)] = int(parts[0])
    return out


def vmstat() -> dict[str, int]:
    out = {}
    for line in (read_text("/proc/vmstat") or "").splitlines():
        key, _, value = line.partition(" ")
        if value.isdigit():
            out[key] = int(value)
    return out


def static() -> dict[str, Any]:
    info = meminfo()
    sizes = sorted(int(os.path.basename(p).split("-")[1][:-2]) * 1024
                   for p in glob.glob("/sys/kernel/mm/hugepages/hugepages-*kB"))
    return {
        "total_bytes": info.get("mem_total_bytes"),
        "swap_total_bytes": info.get("swap_total_bytes"),
        "huge_page_sizes_bytes": sizes,
        "default_huge_page_bytes": info.get("hugepagesize_bytes"),
        "transparent_hugepages": bracketed(read_text("/sys/kernel/mm/transparent_hugepage/enabled")),
        "transparent_hugepages_defrag": bracketed(read_text("/sys/kernel/mm/transparent_hugepage/defrag")),
        "swappiness": read_int("/proc/sys/vm/swappiness"),
        "overcommit_memory": read_int("/proc/sys/vm/overcommit_memory"),
        "overcommit_ratio": read_int("/proc/sys/vm/overcommit_ratio"),
        "min_free_bytes": (k * 1024) if (k := read_int("/proc/sys/vm/min_free_kbytes")) is not None else None,
        "zswap": read_text("/sys/module/zswap/parameters/enabled"),
        "ksm_running": read_int("/sys/kernel/mm/ksm/run"),
        "swap_devices": swaps(),
    }


def swaps() -> list[dict[str, Any]]:
    out = []
    for line in (read_text("/proc/swaps") or "").splitlines()[1:]:
        parts = line.split()
        if len(parts) >= 5:
            out.append({"device": parts[0], "type": parts[1], "size_bytes": int(parts[2]) * 1024,
                        "used_bytes": int(parts[3]) * 1024, "priority": int(parts[4])})
    return out


def live(rates: Rates) -> dict[str, Any]:
    info = meminfo()
    total, avail = info.get("mem_total_bytes"), info.get("mem_available_bytes")
    swap_total, swap_free = info.get("swap_total_bytes"), info.get("swap_free_bytes")
    vm = vmstat()
    # total - available, not total - free: page cache the kernel (or, on a
    # UMA GPU, the driver) can reclaim is not memory in use.
    used = total - avail if total is not None and avail is not None else None
    swap_used = swap_total - swap_free if swap_total is not None and swap_free is not None else None
    return {
        "total_bytes": total,
        "used_bytes": used,
        "available_bytes": avail,
        "free_bytes": info.get("mem_free_bytes"),
        "cached_bytes": info.get("cached_bytes"),
        "buffers_bytes": info.get("buffers_bytes"),
        "used_percent": round(100 * used / total, 2) if used is not None and total else None,
        "swap_total_bytes": swap_total,
        "swap_used_bytes": swap_used,
        "swap_used_percent": round(100 * swap_used / swap_total, 2) if swap_used is not None and swap_total else None,
        "huge_pages": {"total": info.get("huge_pages_total"), "free": info.get("huge_pages_free"),
                       "reserved": info.get("huge_pages_rsvd")},
        "rates_per_s": {k: rates.rate(f"vm/{k}", vm.get(k), 1) for k in VMSTAT_RATES if k in vm},
        "oom_kills": vm.get("oom_kill"),
        "meminfo": info,
    }


# -- pressure ---------------------------------------------------------------


def psi_level(some: float, full: float) -> str:
    """sparkview's memory pressure levels, from avg10 percentages."""
    if full > 0.10 or some > 0.30:
        return "CRITICAL"
    if full > 0.05 or some > 0.15:
        return "HIGH"
    if some > 0.05:
        return "MOD"
    return "LOW"


def psi(text: str | None) -> dict[str, Any] | None:
    """'some avg10=0.00 avg60=0.00 avg300=0.00 total=123' lines, parsed."""
    if text is None:
        return None
    out: dict[str, Any] = {}
    for line in text.splitlines():
        kind, *fields = line.split()
        values = dict(f.split("=", 1) for f in fields if "=" in f)
        out[kind] = {k: (int(v) if k == "total" else float(v)) for k, v in values.items()}
    return out


def pressure(rates: Rates, root: str = "/proc/pressure") -> dict[str, Any]:
    out: dict[str, Any] = {}
    for resource in PSI_RESOURCES:
        parsed = psi(read_text(f"{root}/{resource}"))
        if parsed is None:
            continue
        for kind, values in parsed.items():
            # total is cumulative stall in microseconds: as a rate, the share
            # of the interval spent stalled, which avg10 smooths away.
            stalled = rates.rate(f"psi/{resource}/{kind}", values.get("total"), 1)
            values["stalled_percent"] = None if stalled is None else round(stalled / 1e4, 3)
        if resource == "memory":  # the levels are memory's thresholds
            some = parsed.get("some", {}).get("avg10", 0.0)
            full = parsed.get("full", {}).get("avg10", 0.0)
            parsed["level"] = psi_level(some, full)
        out[resource] = parsed
    return out
