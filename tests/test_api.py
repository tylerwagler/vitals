"""The HTTP API and the stream, against a real server on this machine."""

from __future__ import annotations

import gzip
import http.client
import json
import threading
import time

import pytest

from vitals import server as servermod
from vitals.hub import LIVE_SECTIONS, STATIC_SECTIONS, Hub
from vitals.legacy import Exporter


@pytest.fixture
def srv(monkeypatch):
    monkeypatch.setattr(servermod, "KEEPALIVE_S", 0.4)
    hub = Hub()
    s = servermod.Server(("127.0.0.1", 0), hub, Exporter(hub.nv, hub.clock_cap), max_streams=2)
    t = threading.Thread(target=s.serve_forever, daemon=True)
    t.start()
    yield s
    hub.stop()
    s.shutdown()
    s.server_close()


def get(srv, path, headers=None):
    conn = http.client.HTTPConnection("127.0.0.1", srv.server_address[1], timeout=20)
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
        self.conn = http.client.HTTPConnection("127.0.0.1", srv.server_address[1], timeout=10)
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
    s.close()
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline and (srv.hub.subscribers or srv.hub._sampler is not None):
        time.sleep(0.1)
    # The handler notices the closed socket at its next write.
    assert not srv.hub.subscribers and srv.hub._sampler is None and srv.streams == 0
