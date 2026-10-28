"""Every hwmon sensor (temperatures, fans, voltages, currents, power, energy),
thermal zones, cooling devices, and GB10's spbm power rails."""

from __future__ import annotations

import glob
import os
import re
import threading
import time
from typing import Any

from ..sysfs import link_name, natural_key, read_int, read_text, scaled

HWMON_ROOT = "/sys/class/hwmon"
THERMAL_ROOT = "/sys/class/thermal"
SPBM_ACPI_DIR = "/sys/bus/platform/devices/NVDA8800:00"

SENSOR_RE = re.compile(r"^(temp|fan|in|curr|power|energy|humidity|freq)(\d+)_(input|average)$")
# kind -> (unit, scale from the sysfs integer)
UNITS: dict[str, tuple[str, float]] = {
    "temp": ("celsius", 1e-3), "fan": ("rpm", 1.0), "in": ("volts", 1e-3),
    "curr": ("amps", 1e-3), "power": ("watts", 1e-6), "energy": ("joules", 1e-6),
    "humidity": ("percent", 1e-3), "freq": ("mhz", 1e-6),
}
THRESHOLDS = ("min", "max", "lcrit", "crit", "emergency", "crit_hyst", "max_hyst", "cap")


def _channels(hw: str) -> list[tuple[str, str, str]]:
    """(kind, channel prefix, value file) per sensor, preferring _input over _average."""
    found: dict[tuple[str, str], str] = {}
    for path in glob.glob(f"{hw}/*_input") + glob.glob(f"{hw}/*_average"):
        match = SENSOR_RE.match(os.path.basename(path))
        if not match:
            continue
        kind, n, suffix = match.groups()
        key = (kind, f"{kind}{n}")
        if key not in found or suffix == "input":
            found[key] = path
    return [(kind, prefix, path) for (kind, prefix), path in
            sorted(found.items(), key=lambda kv: (kv[0][0], natural_key(kv[0][1])))]


def hwmon_chips(root: str = HWMON_ROOT) -> list[str]:
    return sorted(glob.glob(os.path.join(root, "hwmon*")), key=natural_key)


class HwmonLayout:
    """Which sensors each chip has, their labels and alarm files, re-read once
    a minute. A sample then opens only the value files: labels are half the
    files and never change."""

    TTL_S = 60.0

    def __init__(self, root: str = HWMON_ROOT) -> None:
        self.root = root
        self._at = -self.TTL_S
        self._chips: list[tuple[str, str | None, list[tuple[str, str, str, str, str | None]]]] = []
        self._lock = threading.Lock()

    def get(self) -> list[tuple[str, str | None, list[tuple[str, str, str, str, str | None]]]]:
        with self._lock:
            now = time.monotonic()
            if now - self._at >= self.TTL_S:
                self._at = now
                chips = []
                for hw in hwmon_chips(self.root):
                    sensors = []
                    for kind, prefix, path in _channels(hw):
                        alarm = f"{hw}/{prefix}_alarm"
                        sensors.append((kind, prefix, path, read_text(f"{hw}/{prefix}_label") or prefix,
                                        alarm if os.path.exists(alarm) else None))
                    chips.append((os.path.basename(hw), read_text(f"{hw}/name"), sensors))
                self._chips = chips
            return self._chips


def hwmon_live(root: str = HWMON_ROOT, layout: HwmonLayout | None = None) -> list[dict[str, Any]]:
    out = []
    for name, chip, channels in (layout or HwmonLayout(root)).get():
        for kind, _prefix, path, label, alarm_path in channels:
            unit, scale = UNITS[kind]
            row: dict[str, Any] = {
                "hwmon": name, "chip": chip, "sensor": label,
                "kind": kind, "value": scaled(path, scale, 3), "unit": unit,
            }
            if alarm_path is not None:
                alarm = read_int(alarm_path)
                if alarm is not None:
                    row["alarm"] = bool(alarm)
            out.append(row)
    return out


def hwmon_static(root: str = HWMON_ROOT) -> list[dict[str, Any]]:
    out = []
    for hw in hwmon_chips(root):
        chip = read_text(f"{hw}/name")
        device = os.path.realpath(f"{hw}/device") if os.path.exists(f"{hw}/device") else None
        sensors = []
        for kind, prefix, _ in _channels(hw):
            unit, scale = UNITS[kind]
            limits = {t: v for t in THRESHOLDS
                      if (v := scaled(f"{hw}/{prefix}_{t}", scale, 3)) is not None}
            sensors.append({"sensor": read_text(f"{hw}/{prefix}_label") or prefix,
                            "kind": kind, "unit": unit, **({"limits": limits} if limits else {})})
        out.append({"hwmon": os.path.basename(hw), "chip": chip,
                    "device": os.path.basename(device) if device else None,
                    "driver": link_name(f"{hw}/device/driver"), "sensors": sensors})
    return out


def thermal_zones(root: str = THERMAL_ROOT) -> list[dict[str, Any]]:
    return [
        {"zone": os.path.basename(z), "type": read_text(f"{z}/type"),
         "celsius": scaled(f"{z}/temp", 1e-3, 2)}
        for z in sorted(glob.glob(os.path.join(root, "thermal_zone*")), key=natural_key)
    ]


def thermal_static(root: str = THERMAL_ROOT) -> list[dict[str, Any]]:
    out = []
    for z in sorted(glob.glob(os.path.join(root, "thermal_zone*")), key=natural_key):
        trips = []
        for t in sorted(glob.glob(f"{z}/trip_point_*_temp"), key=natural_key):
            base = t[: -len("_temp")]
            trips.append({"type": read_text(f"{base}_type"), "celsius": scaled(t, 1e-3, 2)})
        out.append({"zone": os.path.basename(z), "type": read_text(f"{z}/type"),
                    "policy": read_text(f"{z}/policy"), "trip_points": trips})
    return out


class CoolingDevices:
    """Cooling device states every 10 s: on ACPI hosts each cur_state read
    can be a firmware call, and a host has one per CPU."""

    TTL_S = 10.0

    def __init__(self) -> None:
        self._at = -self.TTL_S
        self._value: list[dict[str, Any]] = []

    def get(self) -> list[dict[str, Any]]:
        now = time.monotonic()
        if now - self._at >= self.TTL_S:
            self._at = now
            self._value = cooling_devices()
        return self._value


def cooling_devices(root: str = THERMAL_ROOT) -> list[dict[str, Any]]:
    return [
        {"device": os.path.basename(d), "type": read_text(f"{d}/type"),
         "state": read_int(f"{d}/cur_state"), "max_state": read_int(f"{d}/max_state")}
        for d in sorted(glob.glob(os.path.join(root, "cooling_device*")), key=natural_key)
    ]


def find_hwmon(name: str, root: str = HWMON_ROOT) -> str | None:
    for path in sorted(glob.glob(os.path.join(root, "hwmon*"))):
        if (read_text(f"{path}/name") or "").startswith(name):
            return path
    return None


class Spbm:
    """GB10 power rails from the spark_hwmon driver, every channel it labels."""

    def __init__(self, root: str = HWMON_ROOT, acpi_dir: str = SPBM_ACPI_DIR) -> None:
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


def static() -> dict[str, Any]:
    return {"hwmon": hwmon_static(), "thermal_zones": thermal_static()}


def live(hwmon: list[dict[str, Any]], spbm: Spbm, cooling: CoolingDevices | None = None) -> dict[str, Any]:
    temps = [s["value"] for s in hwmon if s["kind"] == "temp" and s["value"] is not None]
    return {
        "hottest_celsius": max(temps) if temps else None,
        "hwmon": hwmon,
        "thermal_zones": thermal_zones(),
        "cooling_devices": cooling.get() if cooling is not None else cooling_devices(),
        "spbm": spbm.collect(),
    }
