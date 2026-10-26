"""Disks and filesystems: what is attached and mounted, how full it is, and
how busy."""

from __future__ import annotations

import glob
import os
import threading
from concurrent.futures import ThreadPoolExecutor
from concurrent.futures import TimeoutError as FutureTimeout
from typing import Any

import psutil

from ..rates import Rates
from ..sysfs import bracketed, read_int, read_text

BLOCK_ROOT = "/sys/block"
SKIP_DISKS = ("loop", "ram", "fd", "sr")
# Mounts that are not storage. Everything else, network filesystems included.
PSEUDO_FS = {
    "proc", "sysfs", "devtmpfs", "devpts", "tmpfs", "cgroup", "cgroup2", "securityfs",
    "pstore", "bpf", "tracefs", "debugfs", "mqueue", "hugetlbfs", "configfs", "fusectl",
    "autofs", "binfmt_misc", "rpc_pipefs", "efivarfs", "nsfs", "ramfs", "squashfs",
    "fuse.lxcfs", "fuse.portal", "fuse.gvfsd-fuse", "nfsd", "selinuxfs", "overlay",
}
SKIP_MOUNT_PREFIXES = ("/proc", "/sys", "/run/credentials", "/snap/", "/var/lib/docker/")
NETWORK_FS = ("nfs", "nfs4", "cifs", "smb3", "ceph", "glusterfs", "9p", "fuse.sshfs", "fuse.rclone")


def transport(name: str) -> str | None:
    path = os.path.realpath(f"{BLOCK_ROOT}/{name}")
    for needle, kind in (("/nvme", "nvme"), ("/virtio", "virtio"), ("/usb", "usb"),
                         ("/ata", "sata"), ("/host", "scsi"), ("/mmc", "mmc")):
        if needle in path:
            return kind
    if name.startswith("dm-"):
        return "device-mapper"
    if name.startswith("md"):
        return "md-raid"
    if name.startswith("zd"):
        return "zvol"
    if name.startswith("zram"):
        return "zram"
    return None


def disks() -> list[str]:
    return sorted(os.path.basename(p) for p in glob.glob(f"{BLOCK_ROOT}/*")
                  if not os.path.basename(p).startswith(SKIP_DISKS))


def block_static() -> list[dict[str, Any]]:
    out = []
    for name in disks():
        base = f"{BLOCK_ROOT}/{name}"
        dev = f"{base}/device"
        size = read_int(f"{base}/size")
        parts = []
        for p in sorted(glob.glob(f"{base}/{name}*")):
            if os.path.exists(f"{p}/partition"):
                psize = read_int(f"{p}/size")
                parts.append({"name": os.path.basename(p),
                              "size_bytes": psize * 512 if psize is not None else None})
        out.append({
            "name": name,
            "dm_name": read_text(f"{base}/dm/name"),
            "model": read_text(f"{dev}/model"),
            "vendor": read_text(f"{dev}/vendor"),
            "serial": read_text(f"{dev}/serial") or read_text(f"{base}/serial"),
            "firmware": read_text(f"{dev}/firmware_rev") or read_text(f"{dev}/rev"),
            "transport": transport(name),
            "size_bytes": size * 512 if size is not None else None,
            "rotational": bool(read_int(f"{base}/queue/rotational")),
            "removable": bool(read_int(f"{base}/removable")),
            "read_only": bool(read_int(f"{base}/ro")),
            "logical_block_bytes": read_int(f"{base}/queue/logical_block_size"),
            "physical_block_bytes": read_int(f"{base}/queue/physical_block_size"),
            "scheduler": bracketed(read_text(f"{base}/queue/scheduler")),
            "holders": sorted(os.listdir(f"{base}/holders")) if os.path.isdir(f"{base}/holders") else [],
            "partitions": parts,
        })
    return out


def mounts() -> list[Any]:
    seen, out = set(), []
    for p in psutil.disk_partitions(all=True):
        # An overlay is a container's root (Docker, OCI); anywhere else it is
        # a layer of someone else's.
        if (p.fstype in PSEUDO_FS and p.mountpoint != "/") or p.mountpoint.startswith(SKIP_MOUNT_PREFIXES):
            continue
        if p.mountpoint in seen:
            continue
        seen.add(p.mountpoint)
        out.append(p)
    return out


def fs_static() -> list[dict[str, Any]]:
    return [{"device": p.device, "mountpoint": p.mountpoint, "fstype": p.fstype,
             "read_only": "ro" in p.opts.split(","), "network": p.fstype in NETWORK_FS}
            for p in mounts()]


class Statvfs:
    """statvfs that cannot hang a sample.

    A dead NFS server blocks statvfs indefinitely. Network mounts are asked
    from a small pool and given half a second; one that does not answer is
    reported unresponsive, and not asked again until that call returns.
    """

    TIMEOUT_S = 0.5

    def __init__(self) -> None:
        self._pool = ThreadPoolExecutor(max_workers=4, thread_name_prefix="statvfs")
        self._stuck: dict[str, Any] = {}
        self._lock = threading.Lock()

    def get(self, path: str, network: bool) -> tuple[os.statvfs_result | None, bool]:
        """(result, responsive)."""
        if not network:
            try:
                return os.statvfs(path), True
            except OSError:
                return None, True
        with self._lock:
            pending = self._stuck.get(path)
            if pending is not None and not pending.done():
                return None, False
            self._stuck.pop(path, None)
        future = self._pool.submit(os.statvfs, path)
        try:
            return future.result(self.TIMEOUT_S), True
        except FutureTimeout:
            with self._lock:
                self._stuck[path] = future
            return None, False
        except OSError:
            return None, True


def filesystems(statvfs: Statvfs) -> list[dict[str, Any]]:
    out = []
    for p in mounts():
        network = p.fstype in NETWORK_FS
        st, responsive = statvfs.get(p.mountpoint, network)
        row: dict[str, Any] = {"mountpoint": p.mountpoint, "device": p.device, "fstype": p.fstype}
        if not responsive:
            row["responsive"] = False
        if st is not None and st.f_blocks:
            total = st.f_blocks * st.f_frsize
            free = st.f_bavail * st.f_frsize
            used = (st.f_blocks - st.f_bfree) * st.f_frsize
            row.update({
                "total_bytes": total, "used_bytes": used, "free_bytes": free,
                # as df counts it: used over what a user could fill
                "used_percent": round(100 * used / (used + free), 2) if used + free else None,
                "inodes_total": st.f_files or None,
                "inodes_used": (st.f_files - st.f_ffree) if st.f_files else None,
            })
        out.append(row)
    return out


def disk_io(rates: Rates) -> list[dict[str, Any]]:
    try:
        counters = psutil.disk_io_counters(perdisk=True) or {}
    except OSError:
        return []
    wanted = set(disks())
    out = []
    for name, c in sorted(counters.items()):
        if name not in wanted:
            continue
        key = f"disk/{name}"
        busy = rates.rate(f"{key}/busy", getattr(c, "busy_time", None))
        out.append({
            "disk": name,
            "read_bytes": c.read_bytes, "write_bytes": c.write_bytes,
            "reads": c.read_count, "writes": c.write_count,
            "read_bytes_per_s": rates.rate(f"{key}/rb", c.read_bytes, 1),
            "write_bytes_per_s": rates.rate(f"{key}/wb", c.write_bytes, 1),
            "reads_per_s": rates.rate(f"{key}/r", c.read_count, 2),
            "writes_per_s": rates.rate(f"{key}/w", c.write_count, 2),
            # busy_time is milliseconds: per second of wall time, as a percent
            "busy_percent": None if busy is None else round(min(busy / 10, 100.0), 2),
        })
    return out


def static() -> dict[str, Any]:
    return {"block_devices": block_static(), "filesystems": fs_static()}


def live(rates: Rates, statvfs: Statvfs) -> dict[str, Any]:
    return {"filesystems": filesystems(statvfs), "disks": disk_io(rates)}
