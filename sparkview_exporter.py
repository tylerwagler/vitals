"""sparkview-exporter: one host's GPU and hardware state as JSON, for pulsar-gui.

Serves ``GET /metrics.json`` on :9420, in the shape pulsar-gui's cluster view
reads (its ``tests/fixtures/sparkview-*.json`` are the reference). Every
reading is taken when the request arrives; nothing is sampled in the
background, so an idle exporter costs nothing.

The collection logic started from sparkview (github.com/parallelArchitect/
sparkview, MIT): the load-gated clock status, the PSI levels, and the spbm
rail layout. It differs where the hardware said otherwise:

- Throttle reasons are decoded with NVML's own bit values. sparkview's table
  maps 0x100 to SW_POWER_CAP; NVML says 0x100 is the display clock setting
  and 0x4 is the power cap.
- spbm's prochot / pl_level / tj_max_c sit in the hwmon directory on current
  kernels, not under the ACPI device, so both places are tried.
- RDMA ports are read from /sys/class/infiniband directly. Their data
  counters are in 4-byte words, per the IB spec.

A reading that cannot be taken is left out or null, never zero: pulsar-gui
treats a zero as a reading.
"""

from __future__ import annotations

import argparse
import glob
import json
import logging
import os
import platform
import pwd
import re
import subprocess
import threading
import time
from datetime import datetime
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

import psutil

try:
    import pynvml
except ImportError:  # the exporter still reports the host without a GPU stack
    pynvml = None

__version__ = "0.1.0"

logger = logging.getLogger("sparkview-exporter")

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

# The port counters pulsar-gui sums into "link errors", as the reference
# exporter ships them. Counters a port does not have are skipped.
RDMA_ERRORS = (
    "symbol_error", "link_downed", "link_error_recovery",
    "port_rcv_errors", "port_xmit_discards", "port_xmit_wait",
)
RDMA_HW_COUNTERS = (
    "out_of_sequence", "packet_seq_err", "duplicate_request", "implied_nak_seq_err",
    "local_ack_timeout_err", "req_transport_retries_exceeded", "rnr_nak_retry_err",
    "roce_adp_retrans", "roce_adp_retrans_to", "roce_slow_restart", "out_of_buffer",
    "np_ecn_marked_roce_packets", "np_cnp_sent", "rp_cnp_handled",
)

SPBM_ACPI_DIR = "/sys/bus/platform/devices/NVDA8800:00"
SYSTEMD_DIRS = ("/etc/systemd/system", "/lib/systemd/system", "/usr/lib/systemd/system")
UMA_THRESHOLD = 0.9


# ---------------------------------------------------------------------------
# sysfs primitives
# ---------------------------------------------------------------------------


def read_text(path: str | Path) -> str | None:
    try:
        return Path(path).read_text().strip()
    except (OSError, UnicodeDecodeError):
        return None


def read_int(path: str | Path) -> int | None:
    text = read_text(path)
    try:
        return int(text) if text is not None else None
    except ValueError:
        return None


def scaled(path: str | Path, scale: float, digits: int = 3) -> float | None:
    value = read_int(path)
    return None if value is None else round(value * scale, digits)


# ---------------------------------------------------------------------------
# GPU
# ---------------------------------------------------------------------------


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
        self._at = 0.0
        self._value: dict[str, Any] | None = None

    def get(self) -> dict[str, Any] | None:
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
                text = read_text(path) or ""
                cap = parse_lgc(" ".join(
                    line for line in text.splitlines() if line.strip().startswith("ExecStart")
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


class Gpus:
    """NVML, initialised lazily and retried if the driver was not ready."""

    def __init__(self) -> None:
        self.ready = False
        self._procs: dict[int, psutil.Process] = {}

    def _init(self) -> bool:
        if self.ready:
            return True
        if pynvml is None:
            return False
        try:
            pynvml.nvmlInit()
        except pynvml.NVMLError as exc:
            logger.warning("NVML unavailable: %s", exc)
            return False
        self.ready = True
        return True

    @staticmethod
    def _call(fn: Any, *args: Any) -> Any:
        try:
            return fn(*args)
        except pynvml.NVMLError:
            return None

    def driver(self) -> dict[str, str]:
        if not self._init():
            return {"driver": "", "cuda": ""}
        driver = self._call(pynvml.nvmlSystemGetDriverVersion) or ""
        cuda = self._call(pynvml.nvmlSystemGetCudaDriverVersion)
        return {
            "driver": driver.decode() if isinstance(driver, bytes) else driver,
            "cuda": f"{cuda // 1000}.{cuda % 1000 // 10}" if isinstance(cuda, int) else "",
        }

    def collect(self, vm: Any, clock_cap: dict[str, Any] | None) -> tuple[list[dict], list[dict]]:
        if not self._init():
            return [], []
        count = self._call(pynvml.nvmlDeviceGetCount) or 0
        gpus, processes = [], []
        for index in range(count):
            handle = self._call(pynvml.nvmlDeviceGetHandleByIndex, index)
            if handle is None:
                continue
            gpu, procs = self._one(index, handle, vm, clock_cap)
            gpus.append(gpu)
            processes.extend(procs)
        return gpus, processes

    def _one(self, index: int, h: Any, vm: Any, clock_cap: dict[str, Any] | None) -> tuple[dict, list[dict]]:
        call = self._call
        name = call(pynvml.nvmlDeviceGetName, h)
        name = name.decode() if isinstance(name, bytes) else name
        util = call(pynvml.nvmlDeviceGetUtilizationRates, h)
        util_pct = util.gpu if util is not None else None
        power_mw = call(pynvml.nvmlDeviceGetPowerUsage, h)

        # GB10's memory is the host's: NVML reports it as not supported, and
        # when it does report, a total near the host's means the same thing.
        mem = call(pynvml.nvmlDeviceGetMemoryInfo, h)
        uma = mem is None or vm.total * UMA_THRESHOLD <= mem.total <= vm.total * 1.1
        mem_total = vm.total if uma else mem.total
        mem_used = vm.total - vm.available if uma else mem.used

        gpu: dict[str, Any] = {
            "index": index,
            "name": name,
            "utilization_percent": util_pct,
            "temperature_celsius": call(pynvml.nvmlDeviceGetTemperature, h, pynvml.NVML_TEMPERATURE_GPU),
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
        call = self._call
        clk = call(pynvml.nvmlDeviceGetClockInfo, h, pynvml.NVML_CLOCK_GRAPHICS)
        clk_max = call(pynvml.nvmlDeviceGetMaxClockInfo, h, pynvml.NVML_CLOCK_GRAPHICS)
        pstate_no = call(pynvml.nvmlDeviceGetPerformanceState, h)
        raw = call(pynvml.nvmlDeviceGetCurrentClocksEventReasons, h)
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

        pci = self._call(pynvml.nvmlDeviceGetPciInfo, h)
        return {
            "uuid": text(self._call(pynvml.nvmlDeviceGetUUID, h)),
            "pci_bus_id": text(pci.busId) if pci is not None else None,
            "vbios": text(self._call(pynvml.nvmlDeviceGetVbiosVersion, h)),
        }

    def _events(self, h: Any) -> dict[str, float]:
        wanted = {key: getattr(pynvml, const) for key, const in EVENT_FIELDS.items()
                  if hasattr(pynvml, const)}
        values = self._call(pynvml.nvmlDeviceGetFieldValues, h, list(wanted.values()))
        if values is None:
            return {}
        return {
            key: round(v.value.ullVal / 1e9, 6)
            for key, v in zip(wanted, values)
            if v.nvmlReturn == pynvml.NVML_SUCCESS
        }

    def _processes(self, h: Any) -> list[dict[str, Any]]:
        running = []
        for fn in ("nvmlDeviceGetComputeRunningProcesses", "nvmlDeviceGetGraphicsRunningProcesses"):
            running.extend(self._call(getattr(pynvml, fn), h) or [])
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


# ---------------------------------------------------------------------------
# host
# ---------------------------------------------------------------------------


def psi_level(some: float, full: float) -> str:
    if full > 0.10 or some > 0.30:
        return "CRITICAL"
    if full > 0.05 or some > 0.15:
        return "HIGH"
    if some > 0.05:
        return "MOD"
    return "LOW"


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


def cpu() -> dict[str, Any]:
    # Non-blocking: utilization since the previous call (the previous scrape).
    per_core = psutil.cpu_percent(None, percpu=True)
    temp = None
    try:
        temps = psutil.sensors_temperatures()
        for key in ("coretemp", "k10temp", "cpu_thermal", "acpitz"):
            if temps.get(key):
                temp = max(t.current for t in temps[key])
                break
    except (AttributeError, OSError):
        pass
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


def find_hwmon(name: str, root: str = "/sys/class/hwmon") -> str | None:
    for path in sorted(glob.glob(os.path.join(root, "hwmon*"))):
        if (read_text(f"{path}/name") or "").startswith(name):
            return path
    return None


class Spbm:
    """GB10 power rails from the spark_hwmon driver, every channel it labels."""

    def __init__(self, root: str = "/sys/class/hwmon", acpi_dir: str = SPBM_ACPI_DIR) -> None:
        self.root = root
        self.acpi_dir = acpi_dir
        self.peak_gpu_w: float | None = None

    def _status(self, hw: str, name: str) -> str:
        path = os.path.join(hw, name)
        return path if os.path.exists(path) else os.path.join(self.acpi_dir, name)

    def collect(self) -> dict[str, Any] | None:
        hw = find_hwmon("spbm", self.root)
        if hw is None:
            return None
        power: dict[str, dict[str, float]] = {}
        for label_path in glob.glob(f"{hw}/power*_label"):
            base, label = label_path[: -len("_label")], read_text(label_path)
            if not label:
                continue
            channel = {k: scaled(f"{base}_{k}", 1e-6) for k in ("input", "cap", "max", "min")
                       if os.path.exists(f"{base}_{k}")}
            power[label] = {k: v for k, v in channel.items() if v is not None}

        def labelled(prefix: str, scale: float) -> dict[str, dict[str, float]]:
            out = {}
            for label_path in glob.glob(f"{hw}/{prefix}*_label"):
                label = read_text(label_path)
                value = scaled(label_path[: -len("_label")] + "_input", scale)
                if label and value is not None:
                    out[label] = {"input": value}
            return out

        gpu_w = power.get("gpu", {}).get("input")
        if gpu_w is not None and (self.peak_gpu_w is None or gpu_w > self.peak_gpu_w):
            self.peak_gpu_w = gpu_w
        tj = read_int(self._status(hw, "tj_max_c"))
        return {
            "hwmon": hw,
            "power_watts": dict(sorted(power.items())),
            "temperatures_celsius": labelled("temp", 1e-3),
            "energy_joules": labelled("energy", 1e-6),
            "peak_gpu_watts": self.peak_gpu_w,
            "cap_exceeded": {
                label: ch["input"] > ch["cap"]
                for label, ch in sorted(power.items()) if "input" in ch and "cap" in ch
            },
            # decidegrees of rise over the idle point, per spbm.c
            "tj_rise_celsius": None if tj is None else round(tj * 0.1, 1),
            "prochot": read_int(self._status(hw, "prochot")),
            "pl_level": read_int(self._status(hw, "pl_level")),
        }


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


def _after_colon(text: str | None) -> str | None:
    """'4: ACTIVE' -> 'ACTIVE'."""
    return None if text is None else text.split(":", 1)[-1].strip()


def rdma(root: str = "/sys/class/infiniband") -> list[dict[str, Any]]:
    out = []
    for dev in sorted(glob.glob(os.path.join(root, "*"))):
        nets = sorted(os.listdir(f"{dev}/device/net")) if os.path.isdir(f"{dev}/device/net") else []
        for port_dir in sorted(glob.glob(f"{dev}/ports/*")):
            try:
                port_no = int(os.path.basename(port_dir))
            except ValueError:
                continue
            ndev = read_text(f"{port_dir}/gid_attrs/ndevs/0")
            rate = read_text(f"{port_dir}/rate")
            rate_match = re.match(r"([\d.]+)\s*Gb/sec", rate or "")
            counters, hw = f"{port_dir}/counters", f"{port_dir}/hw_counters"

            def words(name: str) -> int | None:
                value = read_int(f"{counters}/{name}")
                return None if value is None else value * 4

            out.append({
                "device": os.path.basename(dev),
                "port": port_no,
                "netdev": ndev or (nets[port_no - 1] if 0 < port_no <= len(nets) else None),
                "state": _after_colon(read_text(f"{port_dir}/state")),
                "phys_state": _after_colon(read_text(f"{port_dir}/phys_state")),
                "rate_gbps": float(rate_match.group(1)) if rate_match else None,
                "link_layer": read_text(f"{port_dir}/link_layer"),
                "tx_bytes": words("port_xmit_data"),
                "rx_bytes": words("port_rcv_data"),
                "tx_packets": read_int(f"{counters}/port_xmit_packets"),
                "rx_packets": read_int(f"{counters}/port_rcv_packets"),
                "errors": {k: v for k in RDMA_ERRORS
                           if (v := read_int(f"{counters}/{k}")) is not None},
                "hw_counters": {k: v for k in RDMA_HW_COUNTERS
                                if (v := read_int(f"{hw}/{k}")) is not None},
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


# ---------------------------------------------------------------------------
# the snapshot
# ---------------------------------------------------------------------------


class Exporter:
    def __init__(self) -> None:
        self.gpus = Gpus()
        self.spbm = Spbm()
        self.clock_cap = ClockCap()
        self._lock = threading.Lock()
        self._cached: tuple[float, bytes] | None = None
        psutil.cpu_percent(None, percpu=True)  # prime the first interval

    def snapshot(self) -> dict[str, Any]:
        started = time.perf_counter()
        vm = psutil.virtual_memory()
        gpus, processes = self.gpus.collect(vm, self.clock_cap.get())
        up = uptime_seconds()
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
            "cpu": cpu(),
            "spbm": self.spbm.collect(),
            "thermal_zones": thermal_zones(),
            "hwmon_temperatures": hwmon_temperatures(),
            "network": network(),
            "rdma": rdma(),
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


def make_handler(exporter: Exporter) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        server_version = f"sparkview-exporter/{__version__}"

        def do_GET(self) -> None:  # noqa: N802 - http.server's name
            path = self.path.split("?", 1)[0]
            if path in ("/metrics.json", "/"):
                try:
                    body, status = exporter.body(), 200
                except Exception as exc:  # noqa: BLE001 - one bad scrape must not kill the server
                    logger.exception("scrape failed")
                    body, status = json.dumps({"error": str(exc)}).encode(), 500
                self._send(status, body)
            elif path == "/healthz":
                self._send(200, b'{"status": "ok"}')
            else:
                self._send(404, b'{"error": "not found"}')

        def _send(self, status: int, body: bytes) -> None:
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, fmt: str, *args: Any) -> None:
            logger.debug("%s " + fmt, self.address_string(), *args)

    return Handler


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(prog="sparkview-exporter", description=__doc__.splitlines()[0])
    parser.add_argument("--host", default="0.0.0.0", help="listen address (default 0.0.0.0)")
    parser.add_argument("--port", type=int, default=9420, help="listen port (default 9420)")
    parser.add_argument("--once", action="store_true", help="print one snapshot and exit")
    parser.add_argument("-v", "--verbose", action="store_true", help="log every request")
    args = parser.parse_args(argv)
    logging.basicConfig(level=logging.DEBUG if args.verbose else logging.INFO,
                        format="%(levelname)s %(message)s")

    exporter = Exporter()
    if args.once:
        time.sleep(0.2)  # give the CPU counters an interval
        print(json.dumps(exporter.snapshot(), indent=2))
        return
    server = ThreadingHTTPServer((args.host, args.port), make_handler(exporter))
    logger.info("sparkview-exporter %s on http://%s:%d/metrics.json", __version__, args.host, args.port)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
