#!/usr/bin/env python3
"""Run two vitals builds side by side on this host and diff what they say.

    scripts/compare.py "python3 dist/vitals.pyz" rust/target/release/vitals

Static values must be equal. Live readings move between two reads, so for
those only the shape is compared: the same keys, and a reading present on
one side is present on the other (one null and one not is a difference).
Exits 1 if anything differs.
"""

from __future__ import annotations

import gzip
import json
import shlex
import socket
import subprocess
import sys
import time
import urllib.request

# Expected to differ: the exporter describing itself, and when things were read.
SKIP = {
    "static": {"exporter", "generation", "collected_at", "collection_seconds"},
    "live": {"seq", "time", "interval_s", "duration_s", "static_generation", "processes", "uptime_seconds",
             "slow_readings_age_s"},
    "metrics": {"exporter_version", "timestamp", "uptime_seconds", "scrape_duration_seconds", "time", "uptime",
                "processes"},
}
# Live lists whose membership moves on its own.
VOLATILE_LISTS = {"processes", "failed_units"}


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def start(command: str) -> tuple[subprocess.Popen, int]:
    port = free_port()
    proc = subprocess.Popen([*shlex.split(command), "--host", "127.0.0.1", "--port", str(port)],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    deadline = time.monotonic() + 20
    while True:
        try:
            fetch(port, "/health")
            return proc, port
        except OSError:
            if proc.poll() is not None or time.monotonic() > deadline:
                proc.kill()
                sys.exit(f"{command} did not start")
            time.sleep(0.1)


def fetch(port: int, path: str):
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", headers={"Accept-Encoding": "gzip"})
    with urllib.request.urlopen(req, timeout=60) as r:
        body = r.read()
        if r.headers.get("Content-Encoding") == "gzip":
            body = gzip.decompress(body)
    return json.loads(body)


def kind(v) -> str:
    if v is None:
        return "null"
    if isinstance(v, bool):
        return "bool"
    if isinstance(v, (int, float)):
        return "number"
    return type(v).__name__


def diff(a, b, path: str, mode: str, out: list[str]) -> None:
    if kind(a) != kind(b):
        out.append(f"{path}: {kind(a)} {json.dumps(a)[:80]}  vs  {kind(b)} {json.dumps(b)[:80]}")
        return
    if isinstance(a, dict):
        for k in sorted(set(a) | set(b)):
            if k in SKIP[mode]:
                continue
            if k not in a or k not in b:
                out.append(f"{path}.{k}: only in {'B' if k not in a else 'A'}")
                continue
            diff(a[k], b[k], f"{path}.{k}", mode, out)
    elif isinstance(a, list):
        if path.rsplit(".", 1)[-1] in VOLATILE_LISTS and mode != "static":
            return
        if len(a) != len(b):
            out.append(f"{path}: {len(a)} items vs {len(b)}")
        for i, (x, y) in enumerate(zip(a, b)):
            diff(x, y, f"{path}[{i}]", mode, out)
    elif mode == "static" and a != b:
        if isinstance(a, float) or isinstance(b, float):
            if abs(a - b) <= 1e-6 * max(1.0, abs(a)):
                return
        out.append(f"{path}: {json.dumps(a)[:80]}  vs  {json.dumps(b)[:80]}")


def main() -> int:
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    a, b = start(sys.argv[1]), start(sys.argv[2])
    try:
        found = 0
        for mode, path in (("static", "/v1/static"), ("live", "/v1/live"), ("metrics", "/metrics.json")):
            if mode != "static":
                # two reads each, so both have rates
                fetch(a[1], path), fetch(b[1], path)
                time.sleep(1.0)
            out: list[str] = []
            diff(fetch(a[1], path), fetch(b[1], path), mode, mode, out)
            print(f"== {path}: {len(out)} difference(s)")
            for line in out:
                print("  " + line)
            found += len(out)
        return 1 if found else 0
    finally:
        for proc, _ in (a, b):
            proc.terminate()


if __name__ == "__main__":
    sys.exit(main())
