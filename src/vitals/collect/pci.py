"""Every PCI device, named from pci.ids when the host has it, with its driver,
NUMA node, IOMMU group and link; and the version of each driver in use."""

from __future__ import annotations

import glob
import os
import re
from functools import lru_cache
from typing import Any

from ..sysfs import link_name, read_int, read_text

PCI_ROOT = "/sys/bus/pci/devices"
PCI_IDS = ("/usr/share/misc/pci.ids", "/usr/share/hwdata/pci.ids", "/usr/share/pci.ids")
PCI_ADDR = re.compile(r"^[0-9a-f]{4}:[0-9a-f]{2}:[0-9a-f]{2}\.[0-7]$")
CLASSES = {
    0x00: "unclassified", 0x01: "storage", 0x02: "network", 0x03: "display",
    0x04: "multimedia", 0x05: "memory", 0x06: "bridge", 0x07: "communication",
    0x08: "system", 0x09: "input", 0x0a: "docking", 0x0b: "processor",
    0x0c: "serial bus", 0x0d: "wireless", 0x0e: "intelligent", 0x0f: "satellite",
    0x10: "encryption", 0x11: "signal processing", 0x12: "accelerator",
    0x13: "instrumentation", 0x40: "coprocessor",
}


@lru_cache(maxsize=4)
def pci_names(wanted: frozenset[tuple[str, str]]) -> dict[tuple[str, str | None], str]:
    """Names for the (vendor, device) ids present, from one pass over pci.ids.

    Keyed (vendor, None) for a vendor and (vendor, device) for a device. The
    file is ~1.5 MB; only what this host has is kept.
    """
    path = next((p for p in PCI_IDS if os.path.exists(p)), None)
    if path is None:
        return {}
    vendors = {v for v, _ in wanted}
    out: dict[tuple[str, str | None], str] = {}
    vendor = None
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            for line in f:
                if line.startswith("C "):
                    break  # the class section; no more vendors
                if not line.strip() or line.startswith("#"):
                    continue
                if not line.startswith("\t"):
                    vid, _, name = line.partition("  ")
                    vendor = vid if vid in vendors else None
                    if vendor:
                        out[(vendor, None)] = name.strip()
                elif vendor and not line.startswith("\t\t"):
                    did, _, name = line.strip().partition("  ")
                    if (vendor, did) in wanted:
                        out[(vendor, did)] = name.strip()
    except OSError:
        return {}
    return out


def _hex4(path: str) -> str | None:
    text = read_text(path)
    return text[2:].lower() if text and text.startswith("0x") else None


def devices() -> list[dict[str, Any]]:
    rows = []
    for dev in sorted(glob.glob(f"{PCI_ROOT}/*")):
        addr = os.path.basename(dev)
        cls = read_int(f"{dev}/class")
        parent = os.path.basename(os.path.dirname(os.path.realpath(dev)))
        rows.append({
            "address": addr,
            "vendor_id": _hex4(f"{dev}/vendor"),
            "device_id": _hex4(f"{dev}/device"),
            "subsystem_vendor_id": _hex4(f"{dev}/subsystem_vendor"),
            "subsystem_device_id": _hex4(f"{dev}/subsystem_device"),
            "revision": _hex4(f"{dev}/revision"),
            "class_id": f"{cls:06x}" if cls is not None else None,
            "class": CLASSES.get(cls >> 16) if cls is not None else None,
            "driver": link_name(f"{dev}/driver"),
            "numa_node": read_int(f"{dev}/numa_node"),
            "iommu_group": link_name(f"{dev}/iommu_group"),
            "parent": parent if PCI_ADDR.match(parent) else None,
            "link": {
                "current_speed": read_text(f"{dev}/current_link_speed"),
                "current_width": read_int(f"{dev}/current_link_width"),
                "max_speed": read_text(f"{dev}/max_link_speed"),
                "max_width": read_int(f"{dev}/max_link_width"),
            } if os.path.exists(f"{dev}/max_link_speed") else None,
        })
    names = pci_names(frozenset((r["vendor_id"], r["device_id"]) for r in rows
                                if r["vendor_id"] and r["device_id"]))
    for r in rows:
        r["vendor"] = names.get((r["vendor_id"], None))
        r["name"] = names.get((r["vendor_id"], r["device_id"]))
    return rows


def drivers(pci_rows: list[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    """Each driver bound to a device, with its module's version where the
    module has one (out-of-tree modules do; in-tree ones go with the kernel)."""
    out: dict[str, dict[str, Any]] = {}
    for r in pci_rows:
        name = r["driver"]
        if not name:
            continue
        entry = out.setdefault(name, {"devices": 0})
        entry["devices"] += 1
    for name, entry in out.items():
        module = link_name(f"/sys/bus/pci/drivers/{name}/module") or name
        entry["module"] = module
        entry["version"] = read_text(f"/sys/module/{module}/version")
        entry["srcversion"] = read_text(f"/sys/module/{module}/srcversion")
    return dict(sorted(out.items()))


def static() -> dict[str, Any]:
    rows = devices()
    return {"devices": rows, "drivers": drivers(rows)}
