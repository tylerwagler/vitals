"""The HTTP API: request/reply JSON, and a Server-Sent Events stream.

The stream follows pulsar-server's /metrics/stream: one connection per
subscriber, ``: keepalive`` comments while there is nothing to say, a cap on
subscribers that answers 503 + Retry-After instead of queueing, and no
backpressure: each subscriber is sent the latest sample when it is due, so
a slow reader skips samples instead of holding anything up. Unlike pulsar's,
the event carries the sample itself: here the sample is the whole point, and
it is a few KB, not a re-scrape.
"""

from __future__ import annotations

import gzip
import json
import logging
import socket
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from urllib.parse import parse_qs, urlsplit

from . import __version__
from .collect.processes import SORTS
from .hub import LIVE_SECTIONS, STATIC_SECTIONS, Hub, pick
from .legacy import Exporter

logger = logging.getLogger("vitals")

KEEPALIVE_S = 15.0
MAX_STREAMS = 8
RETRY_AFTER_S = 5
MIN_INTERVAL_S, MAX_INTERVAL_S = 0.25, 60.0
WRITE_TIMEOUT_S = 10.0
GZIP_OVER = 2048
STATIC_ENVELOPE = ("hostname", "generation", "collected_at", "collection_seconds")

ENDPOINTS = {
    "/": "this index",
    "/health": "liveness, version, NVML and stream state",
    "/v1/static[/<section>]": "what the machine is and has; ?refresh=1 re-collects",
    "/v1/live[/<section>]": "one live sample; ?sections=a,b",
    "/v1/processes": "the process table; ?top=25&sort=cpu|memory|gpu",
    "/v1/stream": "Server-Sent Events: hello, sample, static; ?interval=1&sections=a,b&static=1",
    "/metrics.json": "pulsar-gui's cluster snapshot (reference sparkview-exporter 0.6.0 shape)",
}


class BadRequest(ValueError):
    pass


def sections_param(query: dict[str, list[str]], allowed: tuple[str, ...], path_section: str | None) -> tuple[str, ...] | None:
    names = [path_section] if path_section else []
    for value in query.get("sections", []):
        names.extend(n.strip() for n in value.split(",") if n.strip())
    if not names:
        return None
    unknown = [n for n in names if n not in allowed]
    if unknown:
        raise BadRequest(f"unknown section {', '.join(unknown)}; sections are {', '.join(allowed)}")
    return tuple(dict.fromkeys(names))


def number(query: dict[str, list[str]], name: str, default: float, lo: float, hi: float) -> float:
    if name not in query:
        return default
    try:
        value = float(query[name][-1])
    except ValueError:
        raise BadRequest(f"{name} must be a number") from None
    return min(max(value, lo), hi)


def flag(query: dict[str, list[str]], name: str) -> bool:
    return query.get(name, ["0"])[-1].lower() in ("1", "true", "yes", "")


class Server(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, address: tuple[str, int], hub: Hub, legacy: Exporter, max_streams: int = MAX_STREAMS) -> None:
        super().__init__(address, Handler)
        self.hub = hub
        self.legacy = legacy
        self.max_streams = max_streams
        self.streams = 0
        self.streams_lock = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    server: Server
    server_version = f"vitals/{__version__}"
    protocol_version = "HTTP/1.1"  # keep-alive for pollers; a stream closes its own

    # -- plumbing -----------------------------------------------------------

    def log_message(self, fmt: str, *args: Any) -> None:
        logger.debug("%s " + fmt, self.address_string(), *args)

    def _common_headers(self) -> None:
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Cache-Control", "no-store")

    def send_json(self, status: int, payload: Any, pretty: bool = False,
                  extra: dict[str, str] | None = None) -> None:
        if isinstance(payload, bytes):
            body = payload
        else:
            body = json.dumps(payload, indent=2 if pretty else None,
                              separators=None if pretty else (",", ":")).encode()
        if pretty and isinstance(payload, bytes):
            body = json.dumps(json.loads(payload), indent=2).encode()
        gz = len(body) > GZIP_OVER and "gzip" in self.headers.get("Accept-Encoding", "")
        if gz:
            body = gzip.compress(body, compresslevel=5)
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Vary", "Accept-Encoding")
        if gz:
            self.send_header("Content-Encoding", "gzip")
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self._common_headers()
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def error_json(self, status: int, message: str, extra: dict[str, str] | None = None) -> None:
        self.send_json(status, {"error": message}, extra=extra)

    def do_OPTIONS(self) -> None:  # noqa: N802 - http.server's name
        self.send_response(204)
        self.send_header("Access-Control-Allow-Methods", "GET, HEAD, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "*")
        self.send_header("Content-Length", "0")
        self._common_headers()
        self.end_headers()

    def do_HEAD(self) -> None:  # noqa: N802
        self.do_GET()

    # -- routes -------------------------------------------------------------

    def do_GET(self) -> None:  # noqa: N802
        url = urlsplit(self.path)
        path = url.path.rstrip("/") or "/"
        query = parse_qs(url.query, keep_blank_values=True)
        pretty = flag(query, "pretty") if "pretty" in query else False
        try:
            if path == "/":
                self.index(pretty)
            elif path in ("/health", "/healthz"):
                self.health(pretty)
            elif path == "/metrics.json":
                self.metrics_json()
            elif path == "/v1/static" or path.startswith("/v1/static/"):
                self.static(path, query, pretty)
            elif path == "/v1/live" or path.startswith("/v1/live/"):
                self.live(path, query, pretty)
            elif path == "/v1/processes":
                self.processes(query, pretty)
            elif path == "/v1/stream":
                self.stream(query)
            else:
                self.send_json(404, {"error": f"no such endpoint {path}", "endpoints": ENDPOINTS})
        except BadRequest as exc:
            self.error_json(400, str(exc))
        except (BrokenPipeError, ConnectionResetError, TimeoutError):
            self.close_connection = True
        except Exception as exc:  # noqa: BLE001 - one bad request must not kill the server
            logger.exception("%s failed", path)
            self.close_connection = True
            try:
                self.error_json(500, f"{type(exc).__name__}: {exc}")
            except OSError:
                pass

    def index(self, pretty: bool) -> None:
        self.send_json(200, {
            "service": "vitals", "version": __version__,
            "hostname": socket.gethostname(),
            "endpoints": ENDPOINTS,
            "live_sections": list(LIVE_SECTIONS), "static_sections": list(STATIC_SECTIONS),
        }, pretty)

    def health(self, pretty: bool) -> None:
        hub = self.server.hub
        hub.nv.init()
        self.send_json(200, {
            "status": "ok",
            "service": "vitals",
            "version": __version__,
            "hostname": socket.gethostname(),
            "uptime_seconds": round(time.time() - hub.started_at, 1),
            "nvml": hub.nv.ready,
            "nvml_error": hub.nv.error,
            "streams": self.server.streams,
            "max_streams": self.server.max_streams,
            "sampling": bool(hub.subscribers),
            "seq": hub.seq,
            "static_generation": hub.static_generation,
        }, pretty)

    def metrics_json(self) -> None:
        self.send_json(200, self.server.legacy.body())

    def static(self, path: str, query: dict[str, list[str]], pretty: bool) -> None:
        sub = path[len("/v1/static/"):] if path.startswith("/v1/static/") else None
        sections = sections_param(query, STATIC_SECTIONS, sub)
        doc = self.server.hub.static(refresh=flag(query, "refresh") if "refresh" in query else False)
        self.send_json(200, pick(doc, sections, STATIC_ENVELOPE), pretty,
                       {"ETag": f'"{doc["generation"]}"'})

    def live(self, path: str, query: dict[str, list[str]], pretty: bool) -> None:
        sub = path[len("/v1/live/"):] if path.startswith("/v1/live/") else None
        sections = sections_param(query, LIVE_SECTIONS, sub)
        hub = self.server.hub
        doc = hub.live()
        self.send_json(200, hub.encoded(doc, sections), pretty)

    def processes(self, query: dict[str, list[str]], pretty: bool) -> None:
        top = int(number(query, "top", 25, 1, 10000))
        sort = query.get("sort", ["cpu"])[-1]
        if sort not in SORTS:
            raise BadRequest(f"sort is one of {', '.join(SORTS)}")
        self.send_json(200, self.server.hub.process_table(top, sort), pretty)

    # -- the stream ---------------------------------------------------------

    def _event(self, name: str | None, data: bytes | None = None, event_id: int | None = None) -> None:
        if name is None:
            frame = b": keepalive\n\n"
        else:
            frame = (f"id: {event_id}\n" if event_id is not None else "").encode()
            frame += f"event: {name}\ndata: ".encode() + (data or b"{}") + b"\n\n"
        self.wfile.write(frame)
        self.wfile.flush()

    def stream(self, query: dict[str, list[str]]) -> None:
        srv, hub = self.server, self.server.hub
        sections = sections_param(query, LIVE_SECTIONS, None)
        interval = number(query, "interval", 1.0, MIN_INTERVAL_S, MAX_INTERVAL_S)
        with srv.streams_lock:
            at_cap = srv.streams >= srv.max_streams
            if not at_cap:
                srv.streams += 1
        if at_cap:
            # Refused rather than queued: a client that cannot stream should
            # poll /v1/live, which costs the same sample.
            self.error_json(503, "too many streams", {"Retry-After": str(RETRY_AFTER_S)})
            return
        self.close_connection = True
        sid = None
        try:
            static_doc = hub.static()
            self.connection.settimeout(WRITE_TIMEOUT_S)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.send_header("X-Accel-Buffering", "no")
            self._common_headers()
            self.end_headers()
            self.wfile.write(b"retry: 3000\n\n")
            hello: dict[str, Any] = {
                "service": "vitals", "version": __version__, "hostname": static_doc["hostname"],
                "interval_s": interval, "keepalive_s": KEEPALIVE_S,
                "sections": list(sections or LIVE_SECTIONS),
                "static_generation": static_doc["generation"],
            }
            if flag(query, "static") if "static" in query else False:
                hello["static"] = static_doc
            self._event("hello", json.dumps(hello, separators=(",", ":")).encode())
            sid = hub.subscribe(interval)
            self._pump(sid, interval, sections, static_doc["generation"])
        except (BrokenPipeError, ConnectionResetError, TimeoutError, OSError):
            pass  # the subscriber went away
        finally:
            if sid is not None:
                hub.unsubscribe(sid)
            with srv.streams_lock:
                srv.streams -= 1

    def _pump(self, sid: int, interval: float, sections: tuple[str, ...] | None, generation: str) -> None:
        hub = self.server.hub
        last_seq, next_due, last_write = 0, 0.0, time.monotonic()
        while True:
            with hub.cond:
                deadline = last_write + KEEPALIVE_S
                while not hub.stopping:
                    fresh = hub.seq != last_seq and time.monotonic() >= next_due
                    if fresh or hub.static_generation != generation:
                        break
                    wait = min(deadline, next_due if hub.seq != last_seq else deadline) - time.monotonic()
                    if wait <= 0:
                        break
                    hub.cond.wait(wait)
                if hub.stopping:
                    return
                doc, seq, gen = hub.latest, hub.seq, hub.static_generation
            now = time.monotonic()
            sent = False
            if gen != generation and gen is not None:
                generation = gen
                self._event("static", json.dumps({"generation": gen}).encode())
                sent = True
            if doc is not None and seq != last_seq and now >= next_due:
                self._event("sample", hub.encoded(doc, sections), seq)
                last_seq = seq
                # Due again one interval on, with slack for the sampler's own
                # jitter, so a 5 s subscriber on a 1 s sampler gets every 5th.
                next_due = now + interval * 0.9
                sent = True
            if not sent and now - last_write >= KEEPALIVE_S:
                self._event(None)
                sent = True
            if sent:
                last_write = now
