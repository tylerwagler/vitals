"""The exporter against the two things it answers to: pulsar-gui, and the
reference exporter's shape.

``sparkview-sparky.json`` is a real snapshot from sparky (one GB10, idle, its
TP ports down). ``reference-0.6.0.json`` is the reference exporter's snapshot
from the work pair, copied from pulsar-gui's fixtures.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

import sparkview_exporter as sx

FIXTURES = Path(__file__).parent / "fixtures"


def load(name: str) -> dict:
    return json.loads((FIXTURES / name).read_text())


# -- shape ------------------------------------------------------------------


def keys(d: dict, *path: str | int) -> set[str]:
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
def test_every_reference_field_is_present(path):
    ours, ref = load("sparkview-sparky.json"), load("reference-0.6.0.json")
    assert keys(ref, *path) <= keys(ours, *path)


def test_spbm_channels_match_the_reference():
    ours, ref = load("sparkview-sparky.json")["spbm"], load("reference-0.6.0.json")["spbm"]
    for group in ("power_watts", "temperatures_celsius", "energy_joules", "cap_exceeded"):
        assert set(ref[group]) <= set(ours[group]), group
    assert set(ref["power_watts"]["pl1"]) == set(ours["power_watts"]["pl1"])


# -- through pulsar-gui -----------------------------------------------------


def test_pulsar_gui_reads_a_full_row():
    cluster = pytest.importorskip("pulsar_gui.cluster")
    config = pytest.importorskip("pulsar_gui.config")
    sample = load("sparkview-sparky.json")
    cp = cluster.ClusterPoller(config.Config(base_url="http://sparky:8000"))
    cp.topology = cluster.parse_topology({"tp": {
        "nranks": 1, "transport": "rdma",
        "ranks": [{"rank": 0, "host": "sparky", "rdma_device": "rocep1s0f1", "rdma_port": 1}],
    }})
    cp.topology_at = 1000.0

    first = dict(sample, timestamp=1000.0)
    later = json.loads(json.dumps(first))
    later["timestamp"] = 1001.0
    for port in later["rdma"]:
        if port["device"] == "rocep1s0f1":
            port["tx_bytes"] += 1_250_000_000  # 10 Gb in one second
    cp.ingest("sparky", first, 1000.0)
    cp.ingest("sparky", later, 1001.0)
    row = cp.frame(now=1001.2)["ranks"][0]

    assert row["agent"]["state"] == "ok"
    for section, fields in {
        "gpu": ("name", "util_pct", "temp_c", "clock_mhz", "clock_max_mhz", "clock_status"),
        "power": ("gpu_w", "pkg_w", "pkg_cap_w", "sys_w", "sys_cap_w"),
        "memory": ("used_bytes", "total_bytes", "psi_level"),
        "cpu": ("util_pct", "temp_c"),
    }.items():
        missing = [f for f in fields if row[section][f] is None]
        assert not missing, f"{section}: {missing}"
    assert row["link"]["netdev"] == "enp1s0f1np1"
    assert row["link"]["tx_bps"] == pytest.approx(1e10)
    assert row["gpu"]["reasons"] == []  # RELIABILITY is quiet

    detail = cp.detail(0, now=1001.2)
    assert detail["identity"]["uuid"].startswith("GPU-")
    assert detail["rails"] and detail["thermal"]


# -- pieces -----------------------------------------------------------------


def test_reasons_use_nvml_bits():
    assert sx.decode_reasons(0x400) == ["RELIABILITY"]
    assert sx.decode_reasons(0x004) == ["SW_POWER_CAP"]
    assert sx.decode_reasons(0x100) == ["DISPLAY_CLOCK_SETTING"]
    assert sx.decode_reasons(0x201) == ["GPU_IDLE", "0x200"]


@pytest.mark.parametrize(
    "clk, clk_max, util, pstate, problem, status",
    [
        (208, 3003, 0, "P8", False, "IDLE"),
        (2400, 3003, 90, "P0", False, "PASS"),
        (1200, 3003, 90, "P0", False, "THROTTLED"),
        (2400, 3003, 90, "P0", True, "THROTTLED"),
        (2195, 2200, 90, "P0", False, "LOCKED"),
    ],
)
def test_clock_status(clk, clk_max, util, pstate, problem, status):
    assert sx.clock_status(clk, clk_max, util, pstate, problem)[0] == status


def test_lgc_cap_is_read_from_exec_start():
    assert sx.parse_lgc("ExecStart=/usr/bin/nvidia-smi -lgc 300,2200") == 2200.0
    assert sx.parse_lgc("ExecStart=nvidia-smi --lock-gpu-clocks=300,2300") == 2300.0
    assert sx.parse_lgc("ExecStart=/usr/bin/true") is None


def test_psi_levels():
    assert sx.psi_level(0.0, 0.0) == "LOW"
    assert sx.psi_level(0.10, 0.0) == "MOD"
    assert sx.psi_level(0.20, 0.0) == "HIGH"
    assert sx.psi_level(0.0, 0.2) == "CRITICAL"


def test_uptime_format():
    assert sx.format_uptime(54270.32) == "15h 4m"
    assert sx.format_uptime(3 * 86400 + 29 * 60) == "3d 0h 29m"


def test_rdma_reads_words_and_states(tmp_path):
    port = tmp_path / "rocex" / "ports" / "1"
    (port / "counters").mkdir(parents=True)
    (port / "hw_counters").mkdir()
    (port / "gid_attrs" / "ndevs").mkdir(parents=True)
    (port / "state").write_text("4: ACTIVE\n")
    (port / "phys_state").write_text("5: LinkUp\n")
    (port / "rate").write_text("200 Gb/sec (4X HDR)\n")
    (port / "link_layer").write_text("Ethernet\n")
    (port / "gid_attrs" / "ndevs" / "0").write_text("eth7\n")
    (port / "counters" / "port_xmit_data").write_text("100\n")
    (port / "counters" / "symbol_error").write_text("2\n")
    (port / "hw_counters" / "out_of_sequence").write_text("3\n")

    [row] = sx.rdma(str(tmp_path))
    assert (row["device"], row["port"], row["netdev"]) == ("rocex", 1, "eth7")
    assert (row["state"], row["phys_state"], row["rate_gbps"]) == ("ACTIVE", "LinkUp", 200.0)
    assert row["tx_bytes"] == 400 and row["rx_bytes"] is None
    assert row["errors"] == {"symbol_error": 2}
    assert row["hw_counters"] == {"out_of_sequence": 3}


def test_spbm_status_falls_back_to_the_acpi_device(tmp_path):
    hw = tmp_path / "hwmon" / "hwmon6"
    hw.mkdir(parents=True)
    (hw / "name").write_text("spbm\n")
    (hw / "power1_label").write_text("pl1\n")
    (hw / "power1_input").write_text("18762000\n")
    (hw / "power1_cap").write_text("140000000\n")
    (hw / "prochot").write_text("1\n")
    acpi = tmp_path / "acpi"
    acpi.mkdir()
    (acpi / "tj_max_c").write_text("36\n")

    out = sx.Spbm(str(tmp_path / "hwmon"), str(acpi)).collect()
    assert out["power_watts"]["pl1"] == {"input": 18.762, "cap": 140.0}
    assert out["cap_exceeded"] == {"pl1": False}
    assert out["prochot"] == 1 and out["pl_level"] is None
    assert out["tj_rise_celsius"] == 3.6


def test_rdma_prefers_netlink_counters_over_sysfs(tmp_path):
    port = tmp_path / "rocex" / "ports" / "1"
    (port / "hw_counters").mkdir(parents=True)
    (port / "hw_counters" / "out_of_sequence").write_text("3\n")
    netlink = {("rocex", 1): {"out_of_sequence": 9, "rx_write_requests": 5, "port": 1}}

    [row] = sx.rdma(str(tmp_path), hw_counters=netlink)
    assert row["hw_counters"] == {"out_of_sequence": 9}  # only the counters the GUI reads

    [row] = sx.rdma(str(tmp_path), hw_counters={})  # netlink knew nothing of this port
    assert row["hw_counters"] == {"out_of_sequence": 3}


def test_cpu_temperature_is_the_hottest_cpu_sensor():
    temps = [
        {"chip": "mlx5", "sensor": "temp1", "celsius": 70.0},
        {"chip": "acpitz", "sensor": "temp1", "celsius": 35.8},
        {"chip": "acpitz", "sensor": "temp2", "celsius": 41.2},
        {"chip": "acpitz", "sensor": "temp3", "celsius": None},
    ]
    assert sx.cpu(temps)["temperature_celsius"] == 41.2
    assert sx.cpu([])["temperature_celsius"] is None
