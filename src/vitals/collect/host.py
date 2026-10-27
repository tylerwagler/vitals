"""The machine itself: what it is (bare metal, VM, LXC), its OS, kernel and
firmware, and its load, processes and systemd state."""

from __future__ import annotations

import os
import platform
import socket
import time
from datetime import datetime, timezone
from typing import Any

import psutil

from ..rates import Rates
from ..sysfs import parse_kv, read_int, read_text, run, run_any

DMI_DIR = "/sys/class/dmi/id"
# World-readable DMI fields; the serials and UUID are root-only.
DMI_FIELDS = (
    "sys_vendor", "product_name", "product_version", "product_family", "product_sku",
    "board_vendor", "board_name", "board_version",
    "bios_vendor", "bios_version", "bios_date", "bios_release",
    "chassis_vendor", "chassis_version",
)
CHASSIS_TYPES = {
    1: "other", 2: "unknown", 3: "desktop", 4: "low-profile desktop", 6: "mini tower",
    7: "tower", 8: "portable", 9: "laptop", 10: "notebook", 13: "all-in-one",
    17: "main server chassis", 23: "rack mount chassis", 24: "sealed-case pc",
    28: "blade", 30: "tablet", 31: "convertible", 35: "mini pc", 36: "stick pc",
}
# Words in DMI or the device tree that only a hypervisor writes.
VM_VENDORS = {
    "qemu": "qemu", "kvm": "kvm", "vmware": "vmware", "virtualbox": "oracle",
    "innotek": "oracle", "xen": "xen", "bochs": "bochs", "parallels": "parallels",
    "amazon ec2": "amazon", "google compute engine": "google",
    "microsoft corporation virtual machine": "microsoft", "linux,dummy-virt": "qemu",
}


# -- machine type -----------------------------------------------------------


def detect_container() -> str | None:
    # LXC's own marks first: an OCI image run as a Proxmox app container
    # carries the /run/systemd/container its build left ("docker"), which
    # systemd-detect-virt then repeats.
    if os.path.exists("/dev/.lxc-boot-id") or _lxcfs_mounted():
        return "lxc"
    answer = (run_any(["systemd-detect-virt", "--container"]) or "").strip()
    if answer and answer != "none":
        return answer
    # systemd-detect-virt reads /proc/1/environ, which an unprivileged
    # exporter cannot, and may then say "none" inside a container.
    marker = read_text("/run/systemd/container")
    if marker:
        return marker
    if os.path.exists("/run/.containerenv"):
        return "podman"
    if os.path.exists("/.dockerenv"):
        return "docker"
    return None


def _lxcfs_mounted() -> bool:
    """Proxmox (and any LXD/LXC) container: lxcfs serves its /proc files."""
    mounts = read_text("/proc/mounts") or ""
    return any(line.startswith("lxcfs ") and " /proc/" in line for line in mounts.splitlines())


def detect_hypervisor() -> str | None:
    answer = (run_any(["systemd-detect-virt", "--vm"]) or "").strip()
    if answer and answer != "none":
        return answer
    if answer == "none":
        return None
    hv = read_text("/sys/hypervisor/type")
    if hv:
        return hv
    words = " ".join(filter(None, (
        read_text(f"{DMI_DIR}/sys_vendor"), read_text(f"{DMI_DIR}/product_name"),
        read_text("/sys/firmware/devicetree/base/compatible"),
    ))).lower().replace("\0", " ")
    for needle, name in VM_VENDORS.items():
        if needle in words:
            return name
    cpuinfo = read_text("/proc/cpuinfo") or ""
    if any(line.startswith("flags") and " hypervisor" in line for line in cpuinfo.splitlines()):
        return "unknown"
    return None


def machine_type() -> dict[str, Any]:
    """bare-metal, vm, lxc (or another container technology), and what runs it.

    A container in a VM is reported as the container, with the hypervisor
    alongside: the container is what the readings describe.
    """
    container, hypervisor = detect_container(), detect_hypervisor()
    if container:
        kind = "lxc" if container in ("lxc", "lxc-libvirt") else "container"
    elif hypervisor:
        kind = "vm"
    else:
        kind = "bare-metal"
    return {"type": kind, "container": container, "hypervisor": hypervisor}


# -- static -----------------------------------------------------------------


def os_release() -> dict[str, Any]:
    info = parse_kv(read_text("/etc/os-release") or read_text("/usr/lib/os-release"))
    out: dict[str, Any] = {
        "name": info.get("PRETTY_NAME") or info.get("NAME"),
        "id": info.get("ID"),
        "version_id": info.get("VERSION_ID"),
        "codename": info.get("VERSION_CODENAME"),
    }
    dgx = parse_kv(read_text("/etc/dgx-release"))
    if dgx:
        out["dgx"] = dgx
    pve = run(["pveversion"])
    if pve:
        out["proxmox"] = pve.strip()
    return out


def dmi() -> dict[str, Any] | None:
    out: dict[str, Any] = {k: v for k in DMI_FIELDS if (v := read_text(f"{DMI_DIR}/{k}"))}
    chassis = read_int(f"{DMI_DIR}/chassis_type")
    if chassis is not None:
        out["chassis_type"] = CHASSIS_TYPES.get(chassis, str(chassis))
    model = read_text("/sys/firmware/devicetree/base/model")
    if model:
        out["devicetree_model"] = model.strip("\0")
    return out or None


def local_timezone() -> str | None:
    try:
        target = os.readlink("/etc/localtime")
    except OSError:
        return read_text("/etc/timezone")
    return target.split("zoneinfo/", 1)[-1]


def static() -> dict[str, Any]:
    uname = platform.uname()
    boot = psutil.boot_time()
    return {
        "hostname": socket.gethostname(),
        "machine": machine_type(),
        "os": os_release(),
        "kernel": {
            "release": uname.release,
            "version": uname.version,
            "arch": uname.machine,
            "cmdline": read_text("/proc/cmdline"),
            "tainted": read_int("/proc/sys/kernel/tainted"),
        },
        "firmware": dmi(),
        "boot_time": boot,
        "boot_time_iso": datetime.fromtimestamp(boot, timezone.utc).isoformat(),
        "timezone": local_timezone(),
    }


# -- live -------------------------------------------------------------------


class Systemd:
    """System state and failed units, at most every 10 s (two subprocesses)."""

    TTL_S = 10.0

    def __init__(self) -> None:
        self._at = -self.TTL_S
        self._value: dict[str, Any] | None = None

    def get(self) -> dict[str, Any] | None:
        now = time.monotonic()
        if now - self._at >= self.TTL_S:
            self._at = now
            self._value = self._probe()
        return self._value

    @staticmethod
    def _probe() -> dict[str, Any] | None:
        state = (run_any(["systemctl", "is-system-running"]) or "").strip()
        if not state:
            return None
        failed = run(["systemctl", "list-units", "--state=failed", "--plain",
                      "--no-legend", "--no-pager"]) or ""
        synced = (run(["timedatectl", "show", "-p", "NTPSynchronized", "--value"]) or "").strip()
        return {
            "state": state,
            "failed_units": [line.split()[0] for line in failed.splitlines() if line.strip()],
            "time_synchronized": {"yes": True, "no": False}.get(synced),
        }


def proc_stat() -> dict[str, int]:
    out = {}
    for line in (read_text("/proc/stat") or "").splitlines():
        key, _, rest = line.partition(" ")
        if key in ("ctxt", "intr", "processes", "procs_running", "procs_blocked"):
            out[key] = int(rest.split()[0])
    return out


def live(rates: Rates, systemd: Systemd) -> dict[str, Any]:
    stat = proc_stat()
    load = read_text("/proc/loadavg")
    fields = load.split() if load else []
    running_threads, _, threads = fields[3].partition("/") if len(fields) > 3 else ("", "", "")
    file_nr = (read_text("/proc/sys/fs/file-nr") or "").split()
    cpus = os.cpu_count() or 1
    uptime = read_text("/proc/uptime")
    try:
        users = len(psutil.users())
    except OSError:
        users = None
    return {
        "hostname": socket.gethostname(),
        "time": time.time(),
        "uptime_seconds": float(uptime.split()[0]) if uptime else None,
        "load_average": [float(x) for x in fields[:3]] if len(fields) >= 3 else None,
        "load_per_cpu": [round(float(x) / cpus, 3) for x in fields[:3]] if len(fields) >= 3 else None,
        "processes": {
            "total": len(psutil.pids()),
            "threads": int(threads) if threads.isdigit() else None,
            "running": stat.get("procs_running"),
            "blocked": stat.get("procs_blocked"),
        },
        "users": users,
        "file_handles": {
            "allocated": int(file_nr[0]) if file_nr else None,
            "max": int(file_nr[2]) if len(file_nr) > 2 else None,
        },
        "context_switches_per_s": rates.rate("host/ctxt", stat.get("ctxt"), 1),
        "interrupts_per_s": rates.rate("host/intr", stat.get("intr"), 1),
        "forks_per_s": rates.rate("host/processes", stat.get("processes"), 2),
        "systemd": systemd.get(),
    }
