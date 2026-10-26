"""The collectors, on fake /sys trees and a fake NVML."""

from __future__ import annotations

import os
import threading
import time
from collections import namedtuple
from types import SimpleNamespace as NS

import pytest

from vitals import hub as hubmod
from vitals.collect import cpu, gpu, host, memory, pci, sensors, storage
from vitals.rates import Rates
from vitals.sysfs import bracketed, cpu_list, parse_kv


# -- primitives -------------------------------------------------------------


def test_rates_are_per_second_and_never_from_a_reset():
    r = Rates()
    r.begin(100.0)
    assert r.rate("x", 10) is None  # first sight
    r.end()
    r.begin(102.0)
    assert r.rate("x", 30) == 10.0
    assert r.rate("new", 5) is None
    r.end()
    r.begin(103.0)
    assert r.rate("x", 1) is None  # went backwards: a reset
    assert r.rate("missing", None) is None
    r.end()
    r.begin(104.0)
    assert r.rate("x", 3) == 2.0  # the reset value is the new baseline


def test_sysfs_helpers():
    assert bracketed("always [madvise] never") == "madvise"
    assert cpu_list("0-3,8,10-11") == [0, 1, 2, 3, 8, 10, 11]
    assert cpu_list("") == []
    assert parse_kv('NAME="Debian GNU/Linux"\n# c\nID=debian\n') == {"NAME": "Debian GNU/Linux", "ID": "debian"}


# -- memory and pressure ----------------------------------------------------


def test_meminfo_names_and_units():
    info = memory.meminfo("MemTotal:  16 kB\nActive(anon):  2 kB\nHugePages_Total:  4\nSReclaimable: 1 kB\n")
    assert info == {"mem_total_bytes": 16384, "active_anon_bytes": 2048,
                    "huge_pages_total": 4, "sreclaimable_bytes": 1024}


def test_pressure_reads_every_resource_and_rates_stall_time(tmp_path):
    for res in ("cpu", "memory", "io"):
        (tmp_path / res).write_text("some avg10=0.20 avg60=0.10 avg300=0.00 total=1000000\n"
                                    "full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n")
    r = Rates()
    r.begin(0.0)
    memory.pressure(r, str(tmp_path))
    r.end()
    for res in ("cpu", "memory", "io"):
        (tmp_path / res).write_text("some avg10=0.20 avg60=0.10 avg300=0.00 total=1500000\n"
                                    "full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n")
    r.begin(1.0)
    out = memory.pressure(r, str(tmp_path))
    assert set(out) == {"cpu", "memory", "io"}
    assert out["memory"]["some"]["stalled_percent"] == 50.0  # 0.5 s stalled in 1 s
    assert out["memory"]["level"] == "HIGH" and "level" not in out["cpu"]


def test_psi_levels():
    assert memory.psi_level(0.0, 0.0) == "LOW"
    assert memory.psi_level(0.0, 0.2) == "CRITICAL"


# -- cpu --------------------------------------------------------------------


def test_cpu_breakdown_is_percent_of_the_interval():
    T = namedtuple("T", "user nice system idle iowait irq softirq steal guest guest_nice")
    r = Rates()
    r.begin(0.0)
    assert cpu._breakdown(r, "c", T(10, 0, 10, 100, 0, 0, 0, 0, 5, 0)) is None
    r.end()
    r.begin(1.0)
    # 1 s: 0.5 user (0.2 of it guest), 0.25 system, 0.25 idle
    b = cpu._breakdown(r, "c", T(10.5, 0, 10.25, 100.25, 0, 0, 0, 0, 5.2, 0))
    assert b["user"] == 50.0 and b["system"] == 25.0 and b["idle"] == 25.0
    assert b["utilization"] == 75.0  # guest is inside user, not added again


def test_cpu_temperature_is_the_hottest_cpu_sensor():
    hw = [
        {"chip": "mlx5", "kind": "temp", "value": 70.0},
        {"chip": "acpitz", "kind": "temp", "value": 35.8},
        {"chip": "acpitz", "kind": "temp", "value": 41.2},
        {"chip": "acpitz", "kind": "fan", "value": 900.0},
    ]
    assert cpu.temperature(hw) == 41.2
    assert cpu.temperature([]) is None


# -- sensors ----------------------------------------------------------------


def test_hwmon_reads_every_kind_with_units_and_limits(tmp_path):
    hw = tmp_path / "hwmon3"
    hw.mkdir()
    (hw / "name").write_text("nct6798\n")
    (hw / "temp1_input").write_text("45500\n")
    (hw / "temp1_label").write_text("CPUTIN\n")
    (hw / "temp1_crit").write_text("100000\n")
    (hw / "fan2_input").write_text("1200\n")
    (hw / "fan2_alarm").write_text("1\n")
    (hw / "in0_input").write_text("1104\n")
    (hw / "power1_average").write_text("12500000\n")
    (hw / "curr1_input").write_text("2500\n")
    live = {(s["kind"], s["sensor"]): s for s in sensors.hwmon_live(str(tmp_path))}
    assert live[("temp", "CPUTIN")]["value"] == 45.5
    assert live[("fan", "fan2")] | {} == {"hwmon": "hwmon3", "chip": "nct6798", "sensor": "fan2",
                                         "kind": "fan", "value": 1200.0, "unit": "rpm", "alarm": True}
    assert live[("in", "in0")]["value"] == 1.104 and live[("in", "in0")]["unit"] == "volts"
    assert live[("power", "power1")]["value"] == 12.5
    assert live[("curr", "curr1")]["value"] == 2.5
    [chip] = sensors.hwmon_static(str(tmp_path))
    temp = next(s for s in chip["sensors"] if s["kind"] == "temp")
    assert temp["limits"] == {"crit": 100.0}


def test_spbm_status_falls_back_to_the_acpi_device(tmp_path):
    hw = tmp_path / "hwmon" / "hwmon6"
    hw.mkdir(parents=True)
    (hw / "name").write_text("spbm\n")
    (hw / "power1_label").write_text("pl1\n")
    (hw / "power1_input").write_text("18762000\n")
    acpi = tmp_path / "acpi"
    acpi.mkdir()
    (acpi / "pl_level").write_text("2\n")
    out = sensors.Spbm(str(tmp_path / "hwmon"), str(acpi)).collect()
    assert out["power_watts"]["pl1"] == {"input": 18.762}
    assert out["pl_level"] == 2


# -- pci --------------------------------------------------------------------


def test_pci_names_keep_only_what_is_present(tmp_path, monkeypatch):
    ids = tmp_path / "pci.ids"
    ids.write_text("# comment\n10de  NVIDIA Corporation\n\t1eb0  TU104GL [Quadro RTX 5000]\n"
                   "\t\t10de 129f  Quadro RTX 5000\n\t2204  GA102\n15b3  Mellanox\n\t1021  CX7\n"
                   "C 03  Display controller\n\t00  VGA\n")
    monkeypatch.setattr(pci, "PCI_IDS", (str(ids),))
    pci.pci_names.cache_clear()
    names = pci.pci_names(frozenset({("10de", "1eb0")}))
    assert names == {("10de", None): "NVIDIA Corporation", ("10de", "1eb0"): "TU104GL [Quadro RTX 5000]"}
    pci.pci_names.cache_clear()


# -- storage ----------------------------------------------------------------


def test_a_hung_network_mount_is_unresponsive_not_a_hang(monkeypatch):
    release = threading.Event()

    def hang(path):
        release.wait(5)
        raise OSError("gone")

    st = storage.Statvfs()
    monkeypatch.setattr(storage.os, "statvfs", hang)
    started = time.monotonic()
    assert st.get("/mnt/nas", network=True) == (None, False)
    assert st.get("/mnt/nas", network=True) == (None, False)  # not asked again while stuck
    assert time.monotonic() - started < 1.5
    release.set()


def test_local_statvfs_is_direct():
    result, responsive = storage.Statvfs().get("/", network=False)
    assert responsive and result is not None and result.f_blocks > 0


# -- host -------------------------------------------------------------------


@pytest.mark.parametrize("container, hypervisor, kind", [
    (None, None, "bare-metal"), (None, "kvm", "vm"), ("lxc", None, "lxc"),
    ("lxc", "kvm", "lxc"), ("docker", None, "container"),
])
def test_machine_type(monkeypatch, container, hypervisor, kind):
    monkeypatch.setattr(host, "detect_container", lambda: container)
    monkeypatch.setattr(host, "detect_hypervisor", lambda: hypervisor)
    assert host.machine_type() == {"type": kind, "container": container, "hypervisor": hypervisor}


# -- static generation ------------------------------------------------------


def test_generation_ignores_a_downtrained_link():
    a = {"pci": {"devices": [{"address": "0000:01:00.0", "link": {"current_speed": "2.5 GT/s", "max_speed": "16 GT/s"}}]}}
    b = {"pci": {"devices": [{"address": "0000:01:00.0", "link": {"current_speed": "16 GT/s", "max_speed": "16 GT/s"}}]}}
    c = {"pci": {"devices": [{"address": "0000:01:00.0", "link": {"current_speed": "16 GT/s", "max_speed": "8 GT/s"}}]}}
    assert hubmod.generation_of(a) == hubmod.generation_of(b) != hubmod.generation_of(c)


# -- gpu (fake NVML) --------------------------------------------------------


class FakeNVMLError(Exception):
    pass


def fake_nvml(uma: bool = False) -> NS:
    """Enough of pynvml for one Turing card; anything else is unsupported."""
    def unsupported(*_):
        raise FakeNVMLError("Not Supported")

    lib = NS(
        NVMLError=FakeNVMLError, NVML_SUCCESS=0,
        NVML_TEMPERATURE_GPU=0, NVML_CLOCK_GRAPHICS=0, NVML_CLOCK_SM=1, NVML_CLOCK_MEM=2, NVML_CLOCK_VIDEO=3,
        NVML_PCIE_UTIL_TX_BYTES=0, NVML_PCIE_UTIL_RX_BYTES=1,
        NVML_DEVICE_ARCH_TURING=6, NVML_DEVICE_ARCH_AMPERE=7, NVML_BRAND_QUADRO=2,
        NVML_TOPOLOGY_SINGLE=10, NVML_NVLINK_MAX_LINKS=2,
        NVML_P2P_CAPS_INDEX_READ=0, NVML_P2P_STATUS_OK=0,
        nvmlInit=lambda: None,
        nvmlDeviceGetCount=lambda: 2,
        nvmlDeviceGetHandleByIndex=lambda i: f"h{i}",
        nvmlSystemGetDriverVersion=lambda: b"615.71.10",
        nvmlSystemGetCudaDriverVersion=lambda: 13030,
        nvmlSystemGetNVMLVersion=lambda: "13.615.71",
        nvmlDeviceGetName=lambda h: b"Quadro RTX 5000",
        nvmlDeviceGetUUID=lambda h: f"GPU-{h}",
        nvmlDeviceGetArchitecture=lambda h: 6,
        nvmlDeviceGetBrand=lambda h: 2,
        nvmlDeviceGetCudaComputeCapability=lambda h: (7, 5),
        nvmlDeviceGetPciInfo=lambda h: NS(busId=b"00000000:3B:00.0", pciDeviceId=0x1EB010DE, pciSubSystemId=0x129F10DE),
        nvmlDeviceGetMemoryInfo=unsupported if uma else (lambda h: NS(total=16 << 30, used=4 << 30, free=12 << 30)),
        nvmlDeviceGetUtilizationRates=lambda h: NS(gpu=90, memory=40),
        nvmlDeviceGetEncoderUtilization=lambda h: [3, 1000],
        nvmlDeviceGetClockInfo=lambda h, c: [1200, 1200, 7000, 1100][c],
        nvmlDeviceGetMaxClockInfo=lambda h, c: [1815, 1815, 7001, 1650][c],
        nvmlDeviceGetPerformanceState=lambda h: 0,
        nvmlDeviceGetCurrentClocksEventReasons=lambda h: 0x004,
        nvmlDeviceGetPowerUsage=lambda h: 124500,
        nvmlDeviceGetEnforcedPowerLimit=lambda h: 125000,
        nvmlDeviceGetPowerManagementLimitConstraints=lambda h: (125000, 230000),
        nvmlDeviceGetTotalEnergyConsumption=lambda h: 1_000_000,
        nvmlDeviceGetTemperature=lambda h, s: 61,
        nvmlDeviceGetNumFans=lambda h: 1,
        nvmlDeviceGetFanSpeed_v2=lambda h, i: 45,
        nvmlDeviceGetPcieThroughput=lambda h, k: 100,
        nvmlDeviceGetNvLinkState=lambda h, link: 1 if link == 0 else unsupported(),
        nvmlDeviceGetNvLinkVersion=lambda h, link: 2,
        nvmlDeviceGetNvLinkRemotePciInfo_v2=lambda h, link: NS(busId=b"00000000:5E:00.0"),
        nvmlDeviceGetTopologyCommonAncestor=lambda a, b: 10,
        nvmlDeviceGetP2PStatus=lambda a, b, i: 0,
        nvmlDeviceGetEccMode=unsupported,
        nvmlDeviceGetComputeRunningProcesses=lambda h: [NS(pid=os.getpid(), usedGpuMemory=1 << 30)] if h == "h0" else [],
    )
    return lib


def nvml_with(lib: NS) -> gpu.Nvml:
    nv = gpu.Nvml()
    nv.lib, nv.ready = lib, True
    return nv


def test_gpu_static_from_nvml():
    out = gpu.static(nvml_with(fake_nvml()), host_total=64 << 30)
    assert out["available"] and out["driver"] == "615.71.10" and out["cuda"] == "13.3"
    d = out["devices"][0]
    assert (d["architecture"], d["brand"], d["compute_capability"]) == ("turing", "quadro", "7.5")
    assert d["pci"]["device_id"] == "1eb0" and d["pci"]["bus_id"] == "00000000:3B:00.0"
    assert d["power_limits_watts"]["min"] == 125.0 and d["power_limits_watts"]["max"] == 230.0
    assert d["memory_total_bytes"] == 16 << 30 and not d["is_uma"]
    assert d["nvlink"] == [{"link": 0, "active": True, "version": 2, "remote_bus_id": "00000000:5E:00.0"}]
    assert d["ecc_mode"] is None and d["serial"] is None  # unsupported is None, not an error
    assert out["topology"] == [{"a": 0, "b": 1, "level": "single", "p2p": {"read": True}}]


def test_gpu_static_on_uma_uses_the_host_memory():
    d = gpu.static(nvml_with(fake_nvml(uma=True)), host_total=128 << 30)["devices"][0]
    assert d["is_uma"] and d["memory_total_bytes"] == 128 << 30


def test_gpu_live_from_nvml():
    nv, r = nvml_with(fake_nvml()), Rates()
    vm = NS(total=64 << 30, available=60 << 30)
    r.begin(0.0)
    gpu.live(nv, r, vm, gpu.ClockCap(), gpu.GpuProcesses())
    r.end()
    nv.lib.nvmlDeviceGetTotalEnergyConsumption = lambda h: 1_125_000  # +125 J in 1 s
    r.begin(1.0)
    g = gpu.live(nv, r, vm, gpu.ClockCap(), gpu.GpuProcesses())[0]
    assert g["utilization"] == {"gpu_percent": 90, "memory_percent": 40, "encoder_percent": 3, "decoder_percent": None}
    assert g["memory"]["used_percent"] == 25.0
    assert g["power"] == {"draw_watts": 124.5, "limit_watts": 125.0, "energy_joules": 1125.0, "average_watts": 125.0}
    assert g["clocks_mhz"] == {"graphics": 1200, "sm": 1200, "memory": 7000, "video": 1100}
    assert g["throttle"]["status"] == "THROTTLED" and g["throttle"]["reasons"] == ["SW_POWER_CAP"]
    assert g["pcie"]["tx_bytes_per_s"] == 100 * 1024
    assert g["fan_speeds_percent"] == [45]
    assert g["ecc_errors"] is None
    [p] = g["processes"]
    assert p["pid"] == os.getpid() and p["type"] == "compute" and p["name"] and p["gpu_memory_bytes"] == 1 << 30


def test_reasons_use_nvml_bits():
    assert gpu.decode_reasons(0x201) == ["GPU_IDLE", "0x200"]
