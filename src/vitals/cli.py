"""``vitals``: serve the API, or print one reading and exit."""

from __future__ import annotations

import argparse
import json
import logging
import os
import sys
import time

from . import __version__

logger = logging.getLogger("vitals")


def _vendored_modules_beside_the_archive() -> None:
    """Run as ``python3 /opt/vitals/vitals.pyz``, a ``pynvml.py`` copied next
    to the archive (containers without nvidia-ml-py) is importable."""
    archive = sys.argv[0] if sys.argv else ""
    if archive.endswith(".pyz"):
        here = os.path.dirname(os.path.abspath(archive))
        if here not in sys.path:
            sys.path.append(here)


def main(argv: list[str] | None = None) -> None:
    _vendored_modules_beside_the_archive()
    parser = argparse.ArgumentParser(prog="vitals", description=__doc__.splitlines()[0])
    parser.add_argument("--host", default="0.0.0.0", help="listen address (default 0.0.0.0)")
    parser.add_argument("--port", type=int, default=9420, help="listen port (default 9420)")
    parser.add_argument("--max-streams", type=int, default=8, help="concurrent /v1/stream subscribers (default 8)")
    parser.add_argument("--keepalive", type=float, default=15.0, help=argparse.SUPPRESS)  # tests shorten it
    parser.add_argument("--once", nargs="?", const="all", metavar="WHAT",
                        choices=("all", "static", "live", "processes", "metrics"),
                        help="print one reading and exit: all (default), static, live, processes, metrics")
    parser.add_argument("-V", "--version", action="version", version=f"vitals {__version__}")
    parser.add_argument("-v", "--verbose", action="store_true", help="log every request")
    args = parser.parse_args(argv)
    logging.basicConfig(level=logging.DEBUG if args.verbose else logging.INFO,
                        format="%(levelname)s %(message)s")

    from .hub import Hub
    from .legacy import Exporter
    from .server import Server

    hub = Hub()
    legacy = Exporter(hub.nv, hub.clock_cap)
    if args.once:
        print(json.dumps(once(hub, legacy, args.once), indent=2, default=str))
        return

    server = Server((args.host, args.port), hub, legacy, max_streams=args.max_streams, keepalive_s=args.keepalive)
    hub.listen = f"{args.host}:{server.server_address[1]}"
    logger.info("vitals %s on http://%s/ (stream: /v1/stream)", __version__, hub.listen)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        hub.stop()
        server.server_close()


def once(hub, legacy, what: str) -> dict:
    if what == "metrics":
        time.sleep(0.2)  # give the CPU counters an interval
        return legacy.snapshot()
    if what == "processes":
        return hub.process_table(25, "cpu")
    out = {}
    if what in ("all", "static"):
        out["static"] = hub.static()
    if what in ("all", "live"):
        hub.take()  # the first sample has no rates; the second does
        time.sleep(0.5)
        out["live"] = hub.take()
    return out if what == "all" else out[what]
