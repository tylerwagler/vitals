"""The HTTP API and the stream, black-box, against a real server.

By default the server is this checkout's Python one, in-process. With
``VITALS_BIN`` set (``VITALS_BIN=rust/target/release/vitals``, or
``VITALS_BIN="python3 dist/vitals.pyz"``) the same tests run against that
build, started on a free port: this suite is the contract every
implementation answers to.
"""

from __future__ import annotations

import gzip
import http.client
import json
import os
import shlex
import socket
import subprocess
import threading
import time
from pathlib import Path

import pytest

from vitals import server as servermod
from vitals.hub import LIVE_SECTIONS, STATIC_SECTIONS, Hub
from vitals.legacy import Exporter

FIXTURES = Path(__file__).parent / "fixtures"
TEST_FLAGS = ["--max-streams", "2", "--keepalive", "0.4"]


class Target:
    def __init__(self, port: int) -> None:
        self.host, self.port = "127.0.0.1", port

    def health(self) -> dict:
        return get_json(self, "/health")[1]


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@pytest.fixture
def srv():
    command = os.environ.get("VITALS_BIN")
    if command:
        port = free_port()
        proc = subprocess.Popen([*shlex.split(command), "--host", "127.0.0.1", "--port", str(port), *TEST_FLAGS],
                                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        target = Target(port)
        deadline = time.monotonic() + 15
        while True:
            try:
                target.health()
                break
            except OSError:
                if proc.poll() is not None or time.monotonic() > deadline:
                    proc.kill()
                    raise RuntimeError(f"{command} did not start: {proc.stderr.read().decode()[-2000:]}")
                time.sleep(0.05)
        yield target
        proc.terminate()
        try:
            proc.wait(5)
        except subprocess.TimeoutExpired:
            proc.kill()
        return
    hub = Hub()
    s = servermod.Server(("127.0.0.1", 0), hub, Exporter(hub.nv, hub.clock_cap), max_streams=2, keepalive_s=0.4)
    t = threading.Thread(target=s.serve_forever, daemon=True)
    t.start()
    yield Target(s.server_address[1])
    hub.stop()
    s.shutdown()
    s.server_close()


def get(srv, path, headers=None):
    conn = http.client.HTTPConnection(srv.host, srv.port, timeout=20)
    conn.request("GET", path, headers=headers or {})
    resp = conn.getresponse()
    body = resp.read()
    conn.close()
    if resp.getheader("Content-Encoding") == "gzip":
        body = gzip.decompress(body)
    return resp, body


def get_json(srv, path):
    resp, body = get(srv, path)
    return resp.status, json.loads(body)


class Stream:
    """A minimal SSE reader: (event, data) pairs, keepalives as (None, None)."""

    def __init__(self, srv, path):
        self.conn = http.client.HTTPConnection(srv.host, srv.port, timeout=10)
        self.conn.request("GET", path, headers={"Accept": "text/event-stream"})
        self.resp = self.conn.getresponse()

    def next(self):
        event, data, comment = None, None, False
        while True:
            line = self.resp.fp.readline().decode()
            if line == "":
                raise EOFError
            line = line.rstrip("\n")
            if not line:
                if event or data or comment:
                    return event, (json.loads(data) if data else None)
                continue
            if line.startswith(":"):
                comment = True
            elif line.startswith("event:"):
                event = line[6:].strip()
            elif line.startswith("data:"):
                data = line[5:].strip()

    def until(self, name):
        while True:
            event, data = self.next()
            if event == name:
                return data

    def close(self):
        # The response holds the socket open past conn.close() on its own.
        self.resp.close()
        self.conn.close()


def test_index_and_health(srv):
    status, doc = get_json(srv, "/")
    assert status == 200 and doc["service"] == "vitals" and "/v1/stream" in doc["endpoints"]
    status, doc = get_json(srv, "/health")
    assert status == 200 and doc["status"] == "ok" and doc["streams"] == 0
    assert get_json(srv, "/healthz")[0] == 200


def test_static_has_every_section_and_a_stable_generation(srv):
    status, doc = get_json(srv, "/v1/static")
    assert status == 200
    assert set(STATIC_SECTIONS) <= set(doc)
    assert doc["host"]["machine"]["type"] in ("bare-metal", "vm", "lxc", "container")
    assert doc["host"]["hostname"] and doc["host"]["kernel"]["release"]
    assert doc["exporter"]["version"]
    _, again = get_json(srv, "/v1/static?refresh=1")
    assert again["generation"] == doc["generation"]
    _, one = get_json(srv, "/v1/static/cpu")
    assert set(one) == {"hostname", "generation", "collected_at", "collection_seconds", "cpu"}


def test_live_has_every_section_and_rates_on_the_second_sample(srv):
    _, first = get_json(srv, "/v1/live")
    assert set(LIVE_SECTIONS) <= set(first)
    time.sleep(0.3)
    _, doc = get_json(srv, "/v1/live")
    assert doc["seq"] > first["seq"] and doc["interval_s"] > 0
    assert doc["memory"]["used_bytes"] > 0 and doc["memory"]["total_bytes"] > 0
    assert doc["cpu"]["utilization_percent"] is not None
    assert any(f["mountpoint"] == "/" and f["used_percent"] is not None for f in doc["storage"]["filesystems"])
    assert doc["host"]["uptime_seconds"] > 0
    _, part = get_json(srv, "/v1/live/memory")
    assert "memory" in part and "cpu" not in part


def test_bad_requests(srv):
    assert get_json(srv, "/v1/live?sections=nope")[0] == 400
    assert get_json(srv, "/v1/processes?sort=nope")[0] == 400
    status, doc = get_json(srv, "/nope")
    assert status == 404 and "endpoints" in doc


def test_processes(srv):
    status, doc = get_json(srv, "/v1/processes?top=5&sort=memory")
    assert status == 200 and len(doc["processes"]) == 5 and doc["totals"]["processes"] >= 5
    rss = [p["memory_rss_bytes"] or 0 for p in doc["processes"]]
    assert rss == sorted(rss, reverse=True)
    assert "cmdline" not in doc["processes"][0]


def test_gzip(srv):
    resp, body = get(srv, "/v1/static", {"Accept-Encoding": "gzip"})
    assert resp.getheader("Content-Encoding") == "gzip" and json.loads(body)["generation"]


def test_metrics_json_keeps_the_reference_shape(srv):
    status, doc = get_json(srv, "/metrics.json")
    assert status == 200
    assert {"hostname", "exporter_version", "timestamp", "info", "gpus", "memory", "pressure",
            "cpu", "spbm", "thermal_zones", "hwmon_temperatures", "network", "rdma",
            "processes"} <= set(doc)


def test_stream_says_hello_then_samples_in_order(srv):
    s = Stream(srv, "/v1/stream?interval=0.25&sections=memory,cpu")
    assert s.resp.status == 200 and s.resp.getheader("Content-Type") == "text/event-stream"
    hello = s.until("hello")
    assert hello["interval_s"] == 0.25 and hello["sections"] == ["memory", "cpu"] and hello["static_generation"]
    a, b = s.until("sample"), s.until("sample")
    assert b["seq"] > a["seq"] and set(b) >= {"memory", "cpu", "seq", "time"} and "gpus" not in b
    assert b["interval_s"] == pytest.approx(0.25, abs=0.2)
    s.close()


def test_a_slow_subscriber_gets_its_own_interval(srv):
    fast = Stream(srv, "/v1/stream?interval=0.25")
    slow = Stream(srv, "/v1/stream?interval=1")
    slow.until("hello")
    fast.until("hello")
    t0 = time.monotonic()
    first = slow.until("sample")
    second = slow.until("sample")
    gap = time.monotonic() - t0
    assert second["seq"] - first["seq"] >= 2  # skipped the fast stream's samples between
    assert 0.6 <= gap <= 2.5
    fast.close()
    slow.close()


def test_stream_cap_answers_503_with_retry_after(srv):
    a, b = Stream(srv, "/v1/stream"), Stream(srv, "/v1/stream")
    a.until("hello")
    b.until("hello")
    resp, _ = get(srv, "/v1/stream")
    assert resp.status == 503 and resp.getheader("Retry-After")
    a.close()
    b.close()


def test_keepalive_when_nothing_is_due(srv):
    s = Stream(srv, "/v1/stream?interval=60")
    s.until("hello")
    s.until("sample")
    assert s.next() == (None, None)  # a ': keepalive' comment
    s.close()


def test_sampler_stops_when_the_last_subscriber_leaves(srv):
    s = Stream(srv, "/v1/stream?interval=0.25")
    s.until("sample")
    assert srv.health()["streams"] == 1 and srv.health()["sampling"]
    s.close()
    # The server notices the closed socket at its next write.
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline and (srv.health()["streams"] or srv.health()["sampling"]):
        time.sleep(0.1)
    health = srv.health()
    assert health["streams"] == 0 and not health["sampling"]


def keys(d, *path):
    for p in path:
        d = d[p]
    return set(d)


@pytest.mark.parametrize(
    "path",
    [(), ("info",), ("gpus", 0), ("gpus", 0, "throttle"), ("gpus", 0, "identity"),
     ("gpus", 0, "clock_event_seconds"), ("memory",), ("pressure",), ("cpu",),
     ("spbm",), ("thermal_zones", 0), ("hwmon_temperatures", 0), ("network", 0),
     ("rdma", 0), ("rdma", 0, "errors"), ("rdma", 0, "hw_counters")],
)
def test_metrics_json_has_the_reference_fields_this_host_has(srv, path):
    _, ours = get_json(srv, "/metrics.json")
    ref = json.loads((FIXTURES / "reference-0.6.0.json").read_text())
    try:
        mine = keys(ours, *path)
    except (KeyError, IndexError, TypeError):
        pytest.skip(f"this host has no {'/'.join(map(str, path))}")
    if path == ("gpus", 0, "throttle") and mine == {"available"}:
        pytest.skip("this GPU reports no clock state")
    if path == ("gpus", 0, "clock_event_seconds") and not mine:
        pytest.skip("this GPU has no clocks-event counters")
    if path in (("rdma", 0, "hw_counters"), ("rdma", 0, "errors")) and not mine:
        pytest.skip("this port has no counters")  # a down port seen from a container has none
    assert keys(ref, *path) <= mine
