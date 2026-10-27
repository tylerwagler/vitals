"""Network interfaces, sockets and TCP, and RDMA ports."""

from __future__ import annotations

import glob
import json
import os
import re
import socket
import subprocess
from typing import Any

import psutil

from ..rates import Rates
from ..sysfs import link_name, read_int, read_text

NET_ROOT = "/sys/class/net"
IB_ROOT = "/sys/class/infiniband"
NET_COUNTERS = ("rx_bytes", "tx_bytes", "rx_packets", "tx_packets",
                "rx_errors", "tx_errors", "rx_dropped", "tx_dropped", "collisions", "multicast")
TCP_COUNTERS = ("ActiveOpens", "PassiveOpens", "AttemptFails", "EstabResets",
                "InSegs", "OutSegs", "RetransSegs", "InErrs", "OutRsts")

# The port counters pulsar-gui sums into "link errors", as the reference
# exporter ships them. Counters a port does not have are skipped.
RDMA_ERRORS = (
    "symbol_error", "link_downed", "link_error_recovery",
    "port_rcv_errors", "port_xmit_discards", "port_xmit_wait",
)
RDMA_HW_COUNTERS = (
    "out_of_sequence", "packet_seq_err", "duplicate_request", "implied_nak_seq_err",
    "local_ack_timeout_err", "req_transport_retries_exceeded", "rnr_nak_retry_err",
    "roce_adp_retrans", "roce_adp_retrans_to", "roce_slow_restart", "out_of_buffer",
    "np_ecn_marked_roce_packets", "np_cnp_sent", "rp_cnp_handled",
)


# -- interfaces -------------------------------------------------------------


def iface_kind(path: str) -> str:
    name = os.path.basename(path)
    if name == "lo":
        return "loopback"
    if os.path.isdir(f"{path}/bridge"):
        return "bridge"
    if os.path.isdir(f"{path}/bonding"):
        return "bond"
    if os.path.exists(f"{path}/device"):
        return "wireless" if os.path.isdir(f"{path}/wireless") else "physical"
    uevent = read_text(f"{path}/uevent") or ""
    match = re.search(r"DEVTYPE=(\S+)", uevent)
    return match.group(1) if match else "virtual"  # vlan, wireguard, ...; veth, tap


def interfaces() -> list[str]:
    return sorted(glob.glob(os.path.join(NET_ROOT, "*")))


def addresses() -> dict[str, list[dict[str, Any]]]:
    out: dict[str, list[dict[str, Any]]] = {}
    for iface, addrs in psutil.net_if_addrs().items():
        rows = []
        for a in addrs:
            if a.family == socket.AF_INET:
                family = "ipv4"
            elif a.family == socket.AF_INET6:
                family = "ipv6"
            else:
                continue
            prefix = None
            if a.netmask:
                try:
                    prefix = bin(int.from_bytes(socket.inet_pton(a.family, a.netmask), "big")).count("1")
                except OSError:
                    prefix = None
            rows.append({"family": family, "address": a.address.split("%")[0], "prefix": prefix})
        out[iface] = rows
    return out


def iface_static() -> list[dict[str, Any]]:
    addrs = addresses()
    out = []
    for path in interfaces():
        name = os.path.basename(path)
        speed = read_int(f"{path}/speed")
        out.append({
            "iface": name,
            "kind": iface_kind(path),
            "mac": read_text(f"{path}/address"),
            "mtu": read_int(f"{path}/mtu"),
            "driver": link_name(f"{path}/device/driver"),
            "pci": link_name(f"{path}/device") if os.path.exists(f"{path}/device") else None,
            "speed_gbps": speed / 1000 if speed is not None and speed > 0 else None,
            "duplex": read_text(f"{path}/duplex"),
            "master": link_name(f"{path}/master"),
            "members": sorted(os.listdir(f"{path}/brif")) if os.path.isdir(f"{path}/brif") else None,
            "addresses": addrs.get(name, []),
        })
    return out


# /proc/net/dev's sixteen columns, receive then transmit.
PROC_NET_DEV = ("rx_bytes", "rx_packets", "rx_errors", "rx_dropped", "rx_fifo", "rx_frame",
                "rx_compressed", "multicast", "tx_bytes", "tx_packets", "tx_errors",
                "tx_dropped", "tx_fifo", "collisions", "tx_carrier", "tx_compressed")
RATED = ("rx_bytes", "tx_bytes", "rx_packets", "tx_packets",
         "rx_errors", "tx_errors", "rx_dropped", "tx_dropped")


def proc_net_dev(text: str | None = None) -> dict[str, dict[str, int]]:
    """Every interface's counters from one file, instead of ten sysfs files each."""
    out = {}
    for line in (text if text is not None else read_text("/proc/net/dev") or "").splitlines()[2:]:
        name, _, rest = line.partition(":")
        fields = rest.split()
        if len(fields) >= len(PROC_NET_DEV):
            out[name.strip()] = dict(zip(PROC_NET_DEV, (int(f) for f in fields)))
    return out


def iface_live(rates: Rates) -> list[dict[str, Any]]:
    counters = proc_net_dev()
    out = []
    for path in interfaces():
        name = os.path.basename(path)
        row: dict[str, Any] = {
            "iface": name,
            "state": (read_text(f"{path}/operstate") or "unknown").upper(),
        }
        stats = counters.get(name, {})
        for c in NET_COUNTERS:
            value = stats.get(c)
            row[c] = value
            if c in RATED:
                row[f"{c}_per_s"] = rates.rate(f"net/{name}/{c}", value, 1)
        rx, tx = row.get("rx_bytes_per_s"), row.get("tx_bytes_per_s")
        row["rx_bits_per_s"] = None if rx is None else rx * 8
        row["tx_bits_per_s"] = None if tx is None else tx * 8
        out.append(row)
    return out


def sockstat() -> dict[str, dict[str, int]]:
    """/proc/net/sockstat: 'TCP: inuse 5 orphan 0 tw 2 alloc 7 mem 1'."""
    out: dict[str, dict[str, int]] = {}
    for path in ("/proc/net/sockstat", "/proc/net/sockstat6"):
        for line in (read_text(path) or "").splitlines():
            proto, _, rest = line.partition(":")
            fields = rest.split()
            out[proto.strip().lower()] = {fields[i]: int(fields[i + 1])
                                          for i in range(0, len(fields) - 1, 2)
                                          if fields[i + 1].lstrip("-").isdigit()}
    return out


def tcp(rates: Rates) -> dict[str, Any] | None:
    lines = [line.split() for line in (read_text("/proc/net/snmp") or "").splitlines()
             if line.startswith("Tcp:")]
    if len(lines) < 2:
        return None
    values = dict(zip(lines[0][1:], (int(v) for v in lines[1][1:])))
    out: dict[str, Any] = {"established": values.get("CurrEstab")}
    for c in TCP_COUNTERS:
        if c in values:
            out[f"{snake(c)}_per_s"] = rates.rate(f"tcp/{c}", values[c], 2)
    segs, retrans = out.get("out_segs_per_s"), out.get("retrans_segs_per_s")
    out["retransmit_percent"] = round(100 * retrans / segs, 3) if segs and retrans is not None else None
    return out


def snake(name: str) -> str:
    return re.sub(r"(?<=[a-z])(?=[A-Z])", "_", name).lower()


# -- RDMA -------------------------------------------------------------------


def _after_colon(text: str | None) -> str | None:
    """'4: ACTIVE' -> 'ACTIVE'."""
    return None if text is None else text.split(":", 1)[-1].strip()


def rdma_netlink_counters() -> dict[tuple[str, int], dict[str, int]] | None:
    """Every port's hw counters from one ``rdma statistic`` netlink query.

    Each read of a sysfs hw_counters file is its own firmware query, 2-3 ms
    apiece, which made the RDMA section most of a scrape. None when the tool
    is missing or fails, and the sysfs files are read instead.
    """
    if not os.path.isdir(IB_ROOT) or not os.listdir(IB_ROOT):
        return None
    try:
        result = subprocess.run(["rdma", "-j", "statistic", "show"],
                                capture_output=True, text=True, timeout=3)
        rows = json.loads(result.stdout) if result.returncode == 0 else None
    except (OSError, subprocess.TimeoutExpired, ValueError):
        return None
    if not isinstance(rows, list):
        return None
    return {
        (row["ifname"], row["port"]): {k: v for k, v in row.items() if isinstance(v, int)}
        for row in rows
        if isinstance(row, dict) and "ifname" in row and isinstance(row.get("port"), int)
    }


def rdma(root: str = IB_ROOT,
         hw_counters: dict[tuple[str, int], dict[str, int]] | None = None) -> list[dict[str, Any]]:
    out = []
    for dev in sorted(glob.glob(os.path.join(root, "*"))):
        nets = sorted(os.listdir(f"{dev}/device/net")) if os.path.isdir(f"{dev}/device/net") else []
        for port_dir in sorted(glob.glob(f"{dev}/ports/*")):
            try:
                port_no = int(os.path.basename(port_dir))
            except ValueError:
                continue
            ndev = read_text(f"{port_dir}/gid_attrs/ndevs/0")
            rate = read_text(f"{port_dir}/rate")
            rate_match = re.match(r"([\d.]+)\s*Gb/sec", rate or "")
            counters, hw = f"{port_dir}/counters", f"{port_dir}/hw_counters"

            def words(name: str) -> int | None:
                value = read_int(f"{counters}/{name}")
                return None if value is None else value * 4

            out.append({
                "device": os.path.basename(dev),
                "port": port_no,
                "netdev": ndev or (nets[port_no - 1] if 0 < port_no <= len(nets) else None),
                "state": _after_colon(read_text(f"{port_dir}/state")),
                "phys_state": _after_colon(read_text(f"{port_dir}/phys_state")),
                "rate_gbps": float(rate_match.group(1)) if rate_match else None,
                "link_layer": read_text(f"{port_dir}/link_layer"),
                "tx_bytes": words("port_xmit_data"),
                "rx_bytes": words("port_rcv_data"),
                "tx_packets": read_int(f"{counters}/port_xmit_packets"),
                "rx_packets": read_int(f"{counters}/port_rcv_packets"),
                "errors": {k: v for k in RDMA_ERRORS
                           if (v := read_int(f"{counters}/{k}")) is not None},
                "hw_counters": _hw_counters(hw, (hw_counters or {}).get((os.path.basename(dev), port_no))),
            })
    return out


def _hw_counters(sysfs_dir: str, netlink: dict[str, int] | None) -> dict[str, int]:
    if netlink is not None:
        return {k: netlink[k] for k in RDMA_HW_COUNTERS if k in netlink}
    return {k: v for k in RDMA_HW_COUNTERS if (v := read_int(f"{sysfs_dir}/{k}")) is not None}


def rdma_static(root: str = IB_ROOT) -> list[dict[str, Any]]:
    out = []
    for dev in sorted(glob.glob(os.path.join(root, "*"))):
        ports = []
        for port_dir in sorted(glob.glob(f"{dev}/ports/*")):
            ports.append({"port": os.path.basename(port_dir),
                          "link_layer": read_text(f"{port_dir}/link_layer"),
                          "gid0": read_text(f"{port_dir}/gids/0")})
        out.append({
            "device": os.path.basename(dev),
            "node_type": _after_colon(read_text(f"{dev}/node_type")),
            "hca_type": read_text(f"{dev}/hca_type"),
            "board_id": read_text(f"{dev}/board_id"),
            "firmware": read_text(f"{dev}/fw_ver"),
            "node_guid": read_text(f"{dev}/node_guid"),
            "pci": link_name(f"{dev}/device"),
            "driver": link_name(f"{dev}/device/driver"),
            "netdevs": sorted(os.listdir(f"{dev}/device/net")) if os.path.isdir(f"{dev}/device/net") else [],
            "ports": ports,
        })
    return out


def rdma_live(rates: Rates) -> list[dict[str, Any]]:
    ports = rdma(hw_counters=rdma_netlink_counters())
    for p in ports:
        key = f"rdma/{p['device']}/{p['port']}"
        for c in ("tx_bytes", "rx_bytes", "tx_packets", "rx_packets"):
            p[f"{c}_per_s"] = rates.rate(f"{key}/{c}", p[c], 1)
        p["tx_bits_per_s"] = None if p["tx_bytes_per_s"] is None else p["tx_bytes_per_s"] * 8
        p["rx_bits_per_s"] = None if p["rx_bytes_per_s"] is None else p["rx_bytes_per_s"] * 8
    return ports


def static() -> dict[str, Any]:
    return {"interfaces": iface_static(), "rdma": rdma_static()}


def live(rates: Rates) -> dict[str, Any]:
    return {"interfaces": iface_live(rates), "sockets": sockstat(), "tcp": tcp(rates)}
