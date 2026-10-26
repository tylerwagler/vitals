"""Reading /sys, /proc and small tools without letting any of them take a
reading down with it: every helper returns None for a value it cannot get."""

from __future__ import annotations

import os
import re
import shutil
import subprocess
from pathlib import Path


def read_text(path: str | Path) -> str | None:
    try:
        return Path(path).read_text().strip()
    except (OSError, UnicodeDecodeError):
        return None


def read_int(path: str | Path) -> int | None:
    text = read_text(path)
    try:
        return int(text, 0) if text is not None else None
    except ValueError:
        return None


def scaled(path: str | Path, scale: float, digits: int = 3) -> float | None:
    value = read_int(path)
    return None if value is None else round(value * scale, digits)


def link_name(path: str | Path) -> str | None:
    """The basename a symlink points at: a device's driver, its IOMMU group."""
    try:
        return os.path.basename(os.readlink(path))
    except OSError:
        return None


def bracketed(text: str | None) -> str | None:
    """'always [madvise] never' -> 'madvise', the kernel's way of marking a choice."""
    if text is None:
        return None
    match = re.search(r"\[([^\]]+)\]", text)
    return match.group(1) if match else text


def natural_key(path: str) -> tuple:
    """hwmon2 before hwmon10."""
    return tuple(int(part) if part.isdigit() else part
                 for part in re.split(r"(\d+)", os.path.basename(path)))


def parse_kv(text: str | None, sep: str = "=") -> dict[str, str]:
    """KEY=value lines (os-release, uevent), quotes stripped."""
    out: dict[str, str] = {}
    for line in (text or "").splitlines():
        key, found, value = line.partition(sep)
        if found and key.strip() and not key.startswith("#"):
            out[key.strip()] = value.strip().strip('"').strip("'")
    return out


def cpu_list(text: str | None) -> list[int]:
    """'0-3,8,10-11' -> [0, 1, 2, 3, 8, 10, 11]."""
    out: list[int] = []
    for part in (text or "").split(","):
        lo, _, hi = part.strip().partition("-")
        if lo.isdigit():
            out.extend(range(int(lo), int(hi or lo) + 1))
    return out


def run(argv: list[str], timeout: float = 3.0) -> str | None:
    """A tool's stdout, or None if it is missing, fails or hangs."""
    if shutil.which(argv[0]) is None:
        return None
    try:
        result = subprocess.run(argv, capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.TimeoutExpired):
        return None
    return result.stdout if result.returncode == 0 else None


def run_any(argv: list[str], timeout: float = 3.0) -> str | None:
    """Like run, but keeps stdout whatever the exit code: ``systemctl
    is-system-running`` says "degraded" and exits 1."""
    if shutil.which(argv[0]) is None:
        return None
    try:
        return subprocess.run(argv, capture_output=True, text=True, timeout=timeout).stdout
    except (OSError, subprocess.TimeoutExpired):
        return None
