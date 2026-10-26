"""NVIDIA GPUs through NVML: what each card is, and what it is doing.

The clock-status logic started from sparkview (github.com/parallelArchitect/
sparkview, MIT): the load-gated clock status and its thresholds. Throttle
reasons are decoded with NVML's own bit values: sparkview's table maps 0x100
to SW_POWER_CAP, but NVML says 0x100 is the display clock setting and 0x4 is
the power cap.
"""

from __future__ import annotations

import glob
import logging
import os
import re
import subprocess
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from typing import Any

import psutil

from ..rates import Rates
from ..sysfs import read_text

logger = logging.getLogger("vitals")

# sparkview's thresholds: a clock under this while loaded is throttled, and
# under this utilization the GPU is idle whatever its clock says.
CLOCK_THRESHOLD_MHZ = 1400.0
LOAD_GATE_PCT = 10

# NVML clocks-event reason bits. 0x400 is not in pynvml, but nvidia-smi calls
# it Reliability, and it is the only bit set when it says only Reliability is
# active. Bits without a confirmed name are reported as hex.
REASON_BITS: dict[int, str] = {
    0x001: "GPU_IDLE",
    0x002: "APPLICATIONS_CLOCKS_SETTING",
    0x004: "SW_POWER_CAP",
    0x008: "HW_SLOWDOWN",
    0x010: "SYNC_BOOST",
    0x020: "SW_THERMAL_SLOWDOWN",
    0x040: "HW_THERMAL_SLOWDOWN",
    0x080: "HW_POWER_BRAKE_SLOWDOWN",
    0x100: "DISPLAY_CLOCK_SETTING",
    0x400: "RELIABILITY",
}
PROBLEM_BITS = 0x004 | 0x008 | 0x020 | 0x040 | 0x080

# NVML field ids for the cumulative clocks-event counters, in nanoseconds.
EVENT_FIELDS = {
    "sw_power_cap": "NVML_FI_DEV_CLOCKS_EVENT_REASON_SW_POWER_CAP",
    "sw_thermal_slowdown": "NVML_FI_DEV_CLOCKS_EVENT_REASON_SW_THERM_SLOWDOWN",
    "hw_thermal_slowdown": "NVML_FI_DEV_CLOCKS_EVENT_REASON_HW_THERM_SLOWDOWN",
    "hw_power_brake_slowdown": "NVML_FI_DEV_CLOCKS_EVENT_REASON_HW_POWER_BRAKE_SLOWDOWN",
    "sync_boost": "NVML_FI_DEV_CLOCKS_EVENT_REASON_SYNC_BOOST",
}
CLOCKS = {"graphics": "NVML_CLOCK_GRAPHICS", "sm": "NVML_CLOCK_SM",
          "memory": "NVML_CLOCK_MEM", "video": "NVML_CLOCK_VIDEO"}
TEMP_THRESHOLDS = {"shutdown": "NVML_TEMPERATURE_THRESHOLD_SHUTDOWN",
                   "slowdown": "NVML_TEMPERATURE_THRESHOLD_SLOWDOWN",
                   "memory_max": "NVML_TEMPERATURE_THRESHOLD_MEM_MAX",
                   "gpu_max": "NVML_TEMPERATURE_THRESHOLD_GPU_MAX"}
SYSTEMD_DIRS = ("/etc/systemd/system", "/lib/systemd/system", "/usr/lib/systemd/system")


def text(value: Any) -> Any:
    return value.decode() if isinstance(value, bytes) else value


def watts(mw: Any) -> float | None:
    return round(mw / 1000, 3) if isinstance(mw, (int, float)) else None


# -- shared with the legacy snapshot ----------------------------------------


def decode_reasons(bitmask: int) -> list[str]:
    names = [name for bit, name in REASON_BITS.items() if bitmask & bit]
    known = 0
    for bit in REASON_BITS:
        known |= bit
    unknown = bitmask & ~known
    bit = 1
    while unknown:
        if unknown & bit:
            names.append(hex(bit))
            unknown &= ~bit
        bit <<= 1
    return names


def clock_status(clk: float, clk_max: float, util: float, pstate: str, problem: bool) -> tuple[str, bool]:
    """sparkview's four states, load-gated. Returns (status, locked)."""
    locked = 0 < clk_max < 2900 and abs(clk - clk_max) < 50
    if util < LOAD_GATE_PCT or pstate not in ("P0", "P1", "P2"):
        return "IDLE", locked
    if locked:
        return "LOCKED", locked
    if clk < CLOCK_THRESHOLD_MHZ or problem:
        return "THROTTLED", locked
    return "PASS", locked


def gpu_memory(mem: Any, vm: Any) -> tuple[bool, int, int]:
    """(is_uma, total, used) for one GPU.

    GB10's memory is the host's, and NVML says so by not reporting it. That,
    not a total that happens to be near the host's, is the test: sparkview's
    size comparison calls a 16 GB card in a 16 GB container unified.
    """
    if mem is None:
        return True, vm.total, vm.total - vm.available
    return False, mem.total, mem.used


def parse_lgc(exec_start: str) -> float | None:
    """The max of ``nvidia-smi -lgc MIN,MAX`` (or --lock-gpu-clocks) in a unit's ExecStart."""
    match = re.search(r"(?:-lgc|--lock-gpu-clocks)[ =]+(\d+)\s*,\s*(\d+)", exec_start)
    return float(match.group(2)) if match else None


class ClockCap:
    """A lab clamp set by a ``*clock-cap.service`` unit, which NVML cannot see.

    Reported only while the unit is active: an installed but stopped unit
    clamps nothing. Checked at most every 10 s, since it costs a subprocess.
    """

    TTL_S = 10.0

    def __init__(self) -> None:
        self._at = -self.TTL_S
        self._value: dict[str, Any] | None = None
        self._lock = threading.Lock()

    def get(self) -> dict[str, Any] | None:
        with self._lock:
            now = time.monotonic()
            if now - self._at >= self.TTL_S:
                self._at = now
                self._value = self._probe()
            return self._value

    @staticmethod
    def _probe() -> dict[str, Any] | None:
        seen: set[str] = set()
        for directory in SYSTEMD_DIRS:
            for path in sorted(glob.glob(os.path.join(directory, "*clock-cap*.service"))):
                unit = os.path.basename(path)
                if unit in seen:
                    continue
                seen.add(unit)
                body = read_text(path) or ""
                cap = parse_lgc(" ".join(
                    line for line in body.splitlines() if line.strip().startswith("ExecStart")
                ))
                if cap is None:
                    continue
                try:
                    active = subprocess.run(
                        ["systemctl", "is-active", "--quiet", unit], timeout=3
                    ).returncode == 0
                except (OSError, subprocess.TimeoutExpired):
                    active = False
                if active:
                    return {"max_mhz": cap, "source": unit}
        return None


# -- NVML -------------------------------------------------------------------


class Nvml:
    """pynvml, imported and initialised lazily and retried until the driver
    is ready. Every call returns None instead of raising: a card that does
    not support a query (GB10 has no device memory, GeForce no ECC) is a
    missing reading, not a failed sample."""

    RETRY_S = 30.0

    def __init__(self) -> None:
        self.lib: Any = None
        self.ready = False
        self.error: str | None = None
        self._tried = -self.RETRY_S
        self._lock = threading.Lock()

    def init(self) -> bool:
        if self.ready:
            return True
        with self._lock:
            if self.ready:
                return True
            now = time.monotonic()
            if now - self._tried < self.RETRY_S:
                return False
            self._tried = now
            try:
                import pynvml
            except ImportError:
                self.error = "pynvml is not installed"
                return False
            try:
                pynvml.nvmlInit()
            except pynvml.NVMLError as exc:
                if self.error != str(exc):
                    logger.warning("NVML unavailable: %s", exc)
                self.error = str(exc)
                return False
            self.lib, self.ready, self.error = pynvml, True, None
            return True

    def call(self, name: str, *args: Any) -> Any:
        fn = getattr(self.lib, name, None)
        if fn is None:
            return None
        try:
            return fn(*args)
        except self.lib.NVMLError:
            return None

    def const(self, name: str) -> Any:
        return getattr(self.lib, name, None)

    def names(self, prefix: str) -> dict[int, str]:
        """NVML_DEVICE_ARCH_TURING = 6 -> {6: 'turing'}, from pynvml's own constants."""
        return {v: k[len(prefix):].lower() for k, v in vars(self.lib).items()
                if k.startswith(prefix) and isinstance(v, int) and not k.endswith(("_COUNT", "_INVALID"))}

    def handles(self) -> list[tuple[int, Any]]:
        if not self.init():
            return []
        out = []
        for index in range(self.call("nvmlDeviceGetCount") or 0):
            h = self.call("nvmlDeviceGetHandleByIndex", index)
            if h is not None:
                out.append((index, h))
        return out

    def versions(self) -> dict[str, str | None]:
        if not self.init():
            return {"driver": None, "cuda": None, "nvml": None}
        cuda = self.call("nvmlSystemGetCudaDriverVersion")
        return {
            "driver": text(self.call("nvmlSystemGetDriverVersion")),
            "cuda": f"{cuda // 1000}.{cuda % 1000 // 10}" if isinstance(cuda, int) else None,
            "nvml": text(self.call("nvmlSystemGetNVMLVersion")),
        }


def _pair(value: Any) -> dict[str, Any] | None:
    if isinstance(value, (list, tuple)) and len(value) >= 2:
        return {"current": value[0], "pending": value[1]}
    return None


def device_static(nv: Nvml, index: int, h: Any, host_total: int | None) -> dict[str, Any]:
    c = nv.call
    pci = c("nvmlDeviceGetPciInfo", h)
    cc = c("nvmlDeviceGetCudaComputeCapability", h)
    mem = c("nvmlDeviceGetMemoryInfo", h)
    limits = c("nvmlDeviceGetPowerManagementLimitConstraints", h)
    arch = c("nvmlDeviceGetArchitecture", h)
    brand = c("nvmlDeviceGetBrand", h)
    compute_mode = c("nvmlDeviceGetComputeMode", h)

    nvlinks = []
    for link in range(nv.const("NVML_NVLINK_MAX_LINKS") or 0):
        state = c("nvmlDeviceGetNvLinkState", h, link)
        if state is None:
            continue
        remote = c("nvmlDeviceGetNvLinkRemotePciInfo_v2", h, link) or c("nvmlDeviceGetNvLinkRemotePciInfo", h, link)
        nvlinks.append({"link": link, "active": bool(state),
                        "version": c("nvmlDeviceGetNvLinkVersion", h, link),
                        "remote_bus_id": text(remote.busId) if remote is not None else None})

    return {
        "index": index,
        "name": text(c("nvmlDeviceGetName", h)),
        "uuid": text(c("nvmlDeviceGetUUID", h)),
        "serial": text(c("nvmlDeviceGetSerial", h)),
        "board_part_number": text(c("nvmlDeviceGetBoardPartNumber", h)),
        "brand": nv.names("NVML_BRAND_").get(brand) if brand is not None else None,
        "architecture": nv.names("NVML_DEVICE_ARCH_").get(arch) if arch is not None else None,
        "compute_capability": f"{cc[0]}.{cc[1]}" if cc else None,
        "cores": c("nvmlDeviceGetNumGpuCores", h),
        "vbios": text(c("nvmlDeviceGetVbiosVersion", h)),
        "inforom_image": text(c("nvmlDeviceGetInforomImageVersion", h)),
        "minor_number": c("nvmlDeviceGetMinorNumber", h),
        "pci": {
            "bus_id": text(pci.busId) if pci is not None else None,
            "device_id": f"{(pci.pciDeviceId >> 16) & 0xffff:04x}" if pci is not None else None,
            "subsystem_id": f"{pci.pciSubSystemId:08x}" if pci is not None else None,
            "max_link_generation": c("nvmlDeviceGetMaxPcieLinkGeneration", h),
            "max_link_width": c("nvmlDeviceGetMaxPcieLinkWidth", h),
        },
        "is_uma": mem is None,
        "memory_total_bytes": mem.total if mem is not None else host_total,
        "memory_bus_width_bits": c("nvmlDeviceGetMemoryBusWidth", h),
        "max_clocks_mhz": {k: c("nvmlDeviceGetMaxClockInfo", h, nv.const(v)) for k, v in CLOCKS.items()},
        "power_limits_watts": {
            "default": watts(c("nvmlDeviceGetPowerManagementDefaultLimit", h)),
            "min": watts(limits[0]) if limits else None,
            "max": watts(limits[1]) if limits else None,
            "enforced": watts(c("nvmlDeviceGetEnforcedPowerLimit", h)),
        },
        "temperature_thresholds_celsius": {
            k: c("nvmlDeviceGetTemperatureThreshold", h, nv.const(v))
            for k, v in TEMP_THRESHOLDS.items() if nv.const(v) is not None
        },
        "persistence_mode": c("nvmlDeviceGetPersistenceMode", h),
        "compute_mode": nv.names("NVML_COMPUTEMODE_").get(compute_mode) if compute_mode is not None else None,
        "ecc_mode": _pair(c("nvmlDeviceGetEccMode", h)),
        "mig_mode": _pair(c("nvmlDeviceGetMigMode", h)),
        "display_active": c("nvmlDeviceGetDisplayActive", h),
        "fans": c("nvmlDeviceGetNumFans", h),
        "nvlink": nvlinks,
    }


def topology(nv: Nvml, handles: list[tuple[int, Any]]) -> list[dict[str, Any]]:
    """How each pair of GPUs is connected, and what peer-to-peer it allows."""
    levels = nv.names("NVML_TOPOLOGY_")
    caps = {name.lower(): nv.const(f"NVML_P2P_CAPS_INDEX_{name}")
            for name in ("READ", "WRITE", "NVLINK", "ATOMICS")}
    ok = nv.const("NVML_P2P_STATUS_OK")
    out = []
    for i, (a, ha) in enumerate(handles):
        for b, hb in handles[i + 1:]:
            level = nv.call("nvmlDeviceGetTopologyCommonAncestor", ha, hb)
            p2p = {}
            for name, idx in caps.items():
                if idx is not None:
                    status = nv.call("nvmlDeviceGetP2PStatus", ha, hb, idx)
                    p2p[name] = None if status is None else status == ok
            out.append({"a": a, "b": b, "level": levels.get(level) if level is not None else None, "p2p": p2p})
    return out


def static(nv: Nvml, host_total: int | None) -> dict[str, Any]:
    handles = nv.handles()
    if not nv.ready:
        return {"available": False, "error": nv.error, "devices": []}
    return {
        "available": True,
        **nv.versions(),
        "count": len(handles),
        "devices": [device_static(nv, i, h, host_total) for i, h in handles],
        "topology": topology(nv, handles),
    }


# -- live -------------------------------------------------------------------


class GpuProcesses:
    """Who is on each GPU: names and owners, never command lines, which can
    carry secrets (an --api-key)."""

    def __init__(self) -> None:
        self._procs: dict[int, psutil.Process] = {}
        self._seen: set[int] = set()

    def prune(self) -> None:
        for pid in set(self._procs) - self._seen:
            del self._procs[pid]
        self._seen = set()

    def rows(self, nv: Nvml, h: Any) -> list[dict[str, Any]]:
        out, seen = [], set()
        for fn, kind in (("nvmlDeviceGetComputeRunningProcesses", "compute"),
                         ("nvmlDeviceGetGraphicsRunningProcesses", "graphics")):
            for p in nv.call(fn, h) or []:
                if p.pid in seen:
                    continue
                seen.add(p.pid)
                self._seen.add(p.pid)
                mem = getattr(p, "usedGpuMemory", None)
                row: dict[str, Any] = {"pid": p.pid, "type": kind, "name": None, "user": None,
                                       "gpu_memory_bytes": mem if isinstance(mem, int) else None}
                try:
                    proc = self._procs.get(p.pid)
                    if proc is None or not proc.is_running():
                        proc = self._procs[p.pid] = psutil.Process(p.pid)
                    row["name"], row["user"] = proc.name(), proc.username()
                except (psutil.Error, KeyError):
                    pass  # another PID namespace: the container cannot see the host's PIDs
                out.append(row)
        return sorted(out, key=lambda r: r["gpu_memory_bytes"] or 0, reverse=True)


def _events(nv: Nvml, h: Any) -> dict[str, float]:
    wanted = {key: nv.const(const) for key, const in EVENT_FIELDS.items() if nv.const(const) is not None}
    values = nv.call("nvmlDeviceGetFieldValues", h, list(wanted.values())) if wanted else None
    if values is None:
        return {}
    return {
        key: round(v.value.ullVal / 1e9, 6)
        for key, v in zip(wanted, values)
        if v.nvmlReturn == nv.lib.NVML_SUCCESS
    }


# A card's max graphics clock does not change, and each query is an ioctl
# (~5 ms of CPU on a Turing card), so it is asked once per card.
_max_clock: dict[str, Any] = {}


def device_live(nv: Nvml, index: int, h: Any, rates: Rates, vm: Any,
                clock_cap: dict[str, Any] | None, procs: GpuProcesses) -> dict[str, Any]:
    c = nv.call
    uuid = text(c("nvmlDeviceGetUUID", h))
    key = f"gpu/{uuid or index}"
    util = c("nvmlDeviceGetUtilizationRates", h)
    enc = c("nvmlDeviceGetEncoderUtilization", h)
    dec = c("nvmlDeviceGetDecoderUtilization", h)
    uma, total, used = gpu_memory(c("nvmlDeviceGetMemoryInfo", h), vm)
    bar1 = c("nvmlDeviceGetBAR1MemoryInfo", h)
    energy_mj = c("nvmlDeviceGetTotalEnergyConsumption", h)
    energy_rate = rates.rate(f"{key}/energy", energy_mj)
    clocks = {k: c("nvmlDeviceGetClockInfo", h, nv.const(v)) for k, v in CLOCKS.items()}
    clk_max = _max_clock.get(uuid)
    if clk_max is None:
        clk_max = _max_clock[uuid] = c("nvmlDeviceGetMaxClockInfo", h, nv.const("NVML_CLOCK_GRAPHICS"))
    pstate_no = c("nvmlDeviceGetPerformanceState", h)
    pstate = f"P{pstate_no}" if pstate_no is not None else None
    raw = c("nvmlDeviceGetCurrentClocksEventReasons", h)
    if raw is None:
        raw = c("nvmlDeviceGetCurrentClocksThrottleReasons", h)
    util_pct = util.gpu if util is not None else None

    throttle: dict[str, Any] = {"available": False}
    if raw is not None and clocks["graphics"] is not None and clk_max is not None and pstate:
        problem = bool(raw & PROBLEM_BITS)
        status, locked = clock_status(float(clocks["graphics"]), float(clk_max), util_pct or 0, pstate, problem)
        throttle = {"available": True, "status": status, "reasons": decode_reasons(raw),
                    "raw": raw, "problem": problem, "locked": locked,
                    "threshold_mhz": CLOCK_THRESHOLD_MHZ}

    events = _events(nv, h)
    event_pct = {}
    for name, seconds in events.items():
        r = rates.rate(f"{key}/event/{name}", seconds, 6)
        event_pct[name] = None if r is None else round(100 * r, 2)

    kib = 1024  # NVML's PCIe throughput is in KB/s, 1024-byte
    tx = c("nvmlDeviceGetPcieThroughput", h, nv.const("NVML_PCIE_UTIL_TX_BYTES"))
    rx = c("nvmlDeviceGetPcieThroughput", h, nv.const("NVML_PCIE_UTIL_RX_BYTES"))

    ecc = None
    mode = c("nvmlDeviceGetEccMode", h)
    if mode and mode[0]:
        corrected, uncorrected = nv.const("NVML_MEMORY_ERROR_TYPE_CORRECTED"), nv.const("NVML_MEMORY_ERROR_TYPE_UNCORRECTED")
        volatile, aggregate = nv.const("NVML_VOLATILE_ECC"), nv.const("NVML_AGGREGATE_ECC")
        ecc = {
            "volatile_corrected": c("nvmlDeviceGetTotalEccErrors", h, corrected, volatile),
            "volatile_uncorrected": c("nvmlDeviceGetTotalEccErrors", h, uncorrected, volatile),
            "aggregate_corrected": c("nvmlDeviceGetTotalEccErrors", h, corrected, aggregate),
            "aggregate_uncorrected": c("nvmlDeviceGetTotalEccErrors", h, uncorrected, aggregate),
        }
    remapped = c("nvmlDeviceGetRemappedRows", h)
    fans = c("nvmlDeviceGetNumFans", h) or 0

    return {
        "index": index,
        "uuid": uuid,
        "name": text(c("nvmlDeviceGetName", h)),
        "utilization": {
            "gpu_percent": util_pct,
            "memory_percent": util.memory if util is not None else None,
            "encoder_percent": enc[0] if enc else None,
            "decoder_percent": dec[0] if dec else None,
        },
        "memory": {
            "is_uma": uma, "total_bytes": total, "used_bytes": used,
            "free_bytes": total - used if total is not None and used is not None else None,
            "used_percent": round(100 * used / total, 2) if total else None,
            "bar1_total_bytes": bar1.bar1Total if bar1 is not None else None,
            "bar1_used_bytes": bar1.bar1Used if bar1 is not None else None,
        },
        "temperature_celsius": c("nvmlDeviceGetTemperature", h, nv.const("NVML_TEMPERATURE_GPU")),
        "fan_speeds_percent": [c("nvmlDeviceGetFanSpeed_v2", h, k) for k in range(fans)],
        "power": {
            "draw_watts": watts(c("nvmlDeviceGetPowerUsage", h)),
            "limit_watts": watts(c("nvmlDeviceGetEnforcedPowerLimit", h)),
            "energy_joules": round(energy_mj / 1000, 3) if isinstance(energy_mj, int) else None,
            # mJ per second is mW: the mean draw over the interval
            "average_watts": None if energy_rate is None else round(energy_rate / 1000, 3),
        },
        "clocks_mhz": clocks,
        "max_graphics_clock_mhz": clk_max,
        "pstate": pstate,
        "throttle": throttle,
        "clock_event_seconds": events,
        "clock_event_percent": event_pct,
        "clock_cap": clock_cap,
        "pcie": {
            "link_generation": c("nvmlDeviceGetCurrPcieLinkGeneration", h),
            "link_width": c("nvmlDeviceGetCurrPcieLinkWidth", h),
            "tx_bytes_per_s": tx * kib if isinstance(tx, int) else None,
            "rx_bytes_per_s": rx * kib if isinstance(rx, int) else None,
            "replay_count": c("nvmlDeviceGetPcieReplayCounter", h),
        },
        "ecc_errors": ecc,
        "remapped_rows": {"corrected": remapped[0], "uncorrected": remapped[1],
                          "pending": bool(remapped[2]), "failed": bool(remapped[3])}
        if remapped else None,
        "processes": procs.rows(nv, h),
    }


_pool: ThreadPoolExecutor | None = None


def live(nv: Nvml, rates: Rates, vm: Any, clock_cap: ClockCap, procs: GpuProcesses) -> list[dict[str, Any]]:
    """Every GPU, the cards in parallel: a card's queries take ~100 ms (the
    PCIe throughput counters sample for 20 ms each, the event reasons ~25),
    and NVML is thread-safe, so N cards cost one card's time."""
    global _pool
    handles = nv.handles()
    cap = clock_cap.get() if handles else None
    if len(handles) > 1:
        if _pool is None:
            _pool = ThreadPoolExecutor(max_workers=8, thread_name_prefix="vitals-gpu")
        out = list(_pool.map(lambda ih: device_live(nv, ih[0], ih[1], rates, vm, cap, procs), handles))
    else:
        out = [device_live(nv, i, h, rates, vm, cap, procs) for i, h in handles]
    procs.prune()
    return out


def process_memory(nv: Nvml) -> dict[int, list[dict[str, Any]]]:
    """pid -> the GPUs it holds memory on, for the process table."""
    out: dict[int, list[dict[str, Any]]] = {}
    for index, h in nv.handles():
        for fn in ("nvmlDeviceGetComputeRunningProcesses", "nvmlDeviceGetGraphicsRunningProcesses"):
            for p in nv.call(fn, h) or []:
                mem = getattr(p, "usedGpuMemory", None)
                rows = out.setdefault(p.pid, [])
                if not any(r["gpu"] == index for r in rows):
                    rows.append({"gpu": index, "bytes": mem if isinstance(mem, int) else None})
    return out
