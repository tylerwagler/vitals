"""``GET /metrics``: the /metrics.json snapshot as Prometheus text, in the
reference sparkview-exporter 0.6.0's ``sparkview_*`` series, for scrapers
that key on them (the Switchyard dashboard).

Rendered from the same snapshot as /metrics.json, so the two cannot disagree
and several scrapers still cost one reading. Series names and labels are the
reference's, unchanged: dashboards already key on them. Carried over verbatim
from pulsar-gui's exporter/sparkview_exporter.py (051526e).
"""

from __future__ import annotations

PSI_LEVELS = {"LOW": 0, "MOD": 1, "HIGH": 2, "CRITICAL": 3}
CLOCK_STATES = {"IDLE": 0, "PASS": 1, "LOCKED": 2, "THROTTLED": 3}


def _esc(value) -> str:
    return str(value).replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n")


class Renderer:
    def __init__(self) -> None:
        self.lines: list[str] = []
        self._declared: set[str] = set()

    def metric(self, name, value, labels=None, help_text=None, mtype="gauge") -> None:
        if value is None:
            return
        if isinstance(value, bool):
            value = int(value)
        if name not in self._declared:
            if help_text:
                self.lines.append(f"# HELP {name} {help_text}")
            self.lines.append(f"# TYPE {name} {mtype}")
            self._declared.add(name)
        if labels:
            rendered = ",".join(f'{k}="{_esc(v)}"' for k, v in labels.items() if v is not None)
            self.lines.append(f"{name}{{{rendered}}} {value}")
        else:
            self.lines.append(f"{name} {value}")

    def text(self) -> str:
        return "\n".join(self.lines) + "\n"


def render_prometheus(s: dict) -> str:
    r = Renderer()
    host = {"host": s["hostname"]}

    r.metric("sparkview_up", 1, host, "1 if the exporter collected a snapshot.")
    r.metric(
        "sparkview_scrape_duration_seconds",
        s["scrape_duration_seconds"],
        host,
        "Time spent collecting this snapshot.",
    )
    info = s.get("info") or {}
    r.metric(
        "sparkview_build_info",
        1,
        {
            "host": s["hostname"],
            "exporter_version": s["exporter_version"],
            "driver": info.get("driver") or "unknown",
            "cuda": info.get("cuda") or "unknown",
            "kernel": info.get("kernel") or "unknown",
            "gpu_name": info.get("gpu_name") or "unknown",
        },
        "Static build/driver labels; value is always 1.",
    )
    r.metric("sparkview_uptime_seconds", round(s["uptime_seconds"], 1), host, "Host uptime.")

    # --- GPU ---
    for g in s["gpus"]:
        lbl = {"host": s["hostname"], "gpu": g["index"], "name": g["name"]}
        r.metric("sparkview_gpu_utilization_percent", g["utilization_percent"], lbl,
                 "GPU utilization percent.")
        r.metric("sparkview_gpu_temperature_celsius", g["temperature_celsius"], lbl,
                 "GPU temperature.")
        r.metric("sparkview_gpu_power_watts", g["power_watts"], lbl, "GPU power draw via NVML.")
        r.metric("sparkview_gpu_memory_total_bytes", g["memory_total_bytes"], lbl,
                 "GPU memory total; on coherent UMA this is system MemTotal.")
        r.metric("sparkview_gpu_memory_used_bytes", g["memory_used_bytes"], lbl,
                 "GPU memory used; on coherent UMA this is MemTotal-MemAvailable.")
        r.metric("sparkview_gpu_uma_coherent", g["is_uma"], lbl,
                 "1 when GB10-style coherent unified memory is detected.")
        cap = g.get("clock_cap") or {}
        r.metric("sparkview_gpu_clock_cap_mhz", cap.get("max_mhz"), lbl,
                 "Graphics clock ceiling applied by gpu-clock-cap.service; absent when "
                 "no lab clamp is active. NVML cannot report a locked clock range.")
        for reason, secs in sorted((g.get("clock_event_seconds") or {}).items()):
            r.metric("sparkview_gpu_clock_event_seconds_total", secs, {**lbl, "reason": reason},
                     "Cumulative time the GPU clock was held back, per reason.",
                     mtype="counter")
        ident = g.get("identity") or {}
        if ident:
            r.metric("sparkview_gpu_info", 1,
                     {**lbl, "uuid": ident.get("uuid"), "pci_bus_id": ident.get("pci_bus_id"),
                      "vbios": ident.get("vbios")},
                     "Static GPU identity labels; value is always 1.")
        t = g.get("throttle") or {}
        if t.get("available"):
            r.metric("sparkview_gpu_clock_mhz", t.get("clk_mhz"), lbl, "Current graphics clock.")
            r.metric("sparkview_gpu_clock_max_mhz", t.get("clk_max_mhz"), lbl, "Max graphics clock.")
            r.metric("sparkview_gpu_clock_threshold_mhz", t.get("clk_threshold_mhz"), lbl,
                     "Clock below which this GPU is called THROTTLED under load.")
            r.metric(
                "sparkview_gpu_clock_state",
                CLOCK_STATES.get(t.get("status"), -1),
                lbl,
                "Load-gated clock state: 0=IDLE 1=PASS 2=LOCKED 3=THROTTLED.",
            )
            r.metric("sparkview_gpu_throttled", 1 if t.get("status") == "THROTTLED" else 0, lbl,
                     "1 when the GPU is throttled under load.")
            r.metric("sparkview_gpu_problem_throttle", t.get("problem_throttle"), lbl,
                     "1 when NVML reports a problem throttle reason.")
            for reason in t.get("throttle_reasons") or []:
                r.metric("sparkview_gpu_throttle_reason", 1, {**lbl, "reason": reason},
                         "Active NVML throttle reasons, one series per active reason.")

    # --- system memory / swap ---
    m = s["memory"]
    r.metric("sparkview_memory_total_bytes", m["total_bytes"], host, "System memory total.")
    r.metric("sparkview_memory_used_bytes", m["used_bytes"], host,
             "System memory used (MemTotal-MemAvailable).")
    r.metric("sparkview_memory_available_bytes", m["available_bytes"], host, "MemAvailable.")
    r.metric("sparkview_memory_used_percent", m["percent"], host, "System memory used percent.")
    r.metric("sparkview_swap_total_bytes", m["swap_total_bytes"], host, "Swap total.")
    r.metric("sparkview_swap_used_bytes", m["swap_used_bytes"], host, "Swap used.")
    r.metric("sparkview_swap_used_percent", m["swap_percent"], host, "Swap used percent.")

    # --- PSI ---
    psi = s.get("pressure") or {}
    if psi.get("available"):
        r.metric("sparkview_psi_memory_some_avg10", psi.get("some_avg10"), host,
                 "PSI memory 'some' avg10.")
        r.metric("sparkview_psi_memory_full_avg10", psi.get("full_avg10"), host,
                 "PSI memory 'full' avg10.")
        r.metric("sparkview_psi_memory_level", PSI_LEVELS.get(psi.get("level"), -1), host,
                 "Memory pressure level: 0=LOW 1=MOD 2=HIGH 3=CRITICAL.")

    # --- CPU ---
    c = s["cpu"]
    r.metric("sparkview_cpu_utilization_percent", c["utilization_percent"], host,
             "Mean CPU utilization across cores.")
    r.metric("sparkview_cpu_cores_total", c["cores_total"], host, "Total CPU cores.")
    r.metric("sparkview_cpu_cores_active", c["cores_active"], host, "Cores with non-zero usage.")
    r.metric("sparkview_cpu_temperature_celsius", c["temperature_celsius"], host,
             "CPU package temperature.")

    # --- GB10 rails / temps / energy via spark_hwmon (spbm) ---
    spbm = s.get("spbm")
    if spbm:
        for rail, ch in sorted(spbm["power_watts"].items()):
            lbl = {"host": s["hostname"], "rail": rail}
            r.metric("sparkview_spbm_power_watts", ch.get("input"), lbl,
                     "Instantaneous draw per spark_hwmon power rail.")
            r.metric("sparkview_spbm_power_cap_watts", ch.get("cap"), lbl,
                     "Configured cap for this rail.")
            r.metric("sparkview_spbm_power_max_watts", ch.get("max"), lbl,
                     "Maximum settable cap for this rail.")
            r.metric("sparkview_spbm_power_min_watts", ch.get("min"), lbl,
                     "Minimum settable cap for this rail.")
            if rail in spbm["cap_exceeded"]:
                r.metric("sparkview_spbm_cap_exceeded", spbm["cap_exceeded"][rail], lbl,
                         "1 when a rail's instantaneous draw exceeds its cap.")
        for sensor, ch in sorted(spbm["temperatures_celsius"].items()):
            r.metric("sparkview_spbm_temperature_celsius", ch.get("input"),
                     {"host": s["hostname"], "sensor": sensor},
                     "Per-sensor temperature from spark_hwmon.")
        for rail, ch in sorted(spbm["energy_joules"].items()):
            r.metric("sparkview_spbm_energy_joules_total", ch.get("input"),
                     {"host": s["hostname"], "rail": rail},
                     "Cumulative energy per rail since boot.", mtype="counter")
        r.metric("sparkview_spbm_gpu_peak_watts", spbm["peak_gpu_watts"], host,
                 "Peak GPU rail draw seen since the exporter started.")
        r.metric("sparkview_tj_rise_celsius", spbm["tj_rise_celsius"], host,
                 "Junction temperature rise in degrees C.")
        r.metric("sparkview_pl_level", spbm["pl_level"], host,
                 "Active power-limit level reported by spark_hwmon.")
        r.metric("sparkview_prochot", spbm["prochot"], host,
                 "Raw PROCHOT line state. Reads 1 on healthy GB10 nodes at full clock "
                 "on this firmware, so correlate with sparkview_gpu_clock_state rather "
                 "than alerting on this alone.")

    # --- every thermal sensor on the platform ---
    for z in s.get("thermal_zones") or []:
        r.metric("sparkview_thermal_zone_celsius", z["celsius"],
                 {"host": s["hostname"], "zone": z["zone"], "type": z["type"]},
                 "Per-zone temperature from /sys/class/thermal.")
    for t in s.get("hwmon_temperatures") or []:
        r.metric("sparkview_hwmon_temperature_celsius", t["celsius"],
                 {"host": s["hostname"], "chip": t["chip"], "sensor": t["sensor"]},
                 "Every temp input exposed under /sys/class/hwmon.")

    # --- network (ConnectX-7) ---
    for n in s["network"]:
        lbl = {"host": s["hostname"], "iface": n["iface"]}
        r.metric("sparkview_net_connectx7", n["cx7"], lbl,
                 "1 when the interface is a ConnectX-7 / mlx5 link.")
        r.metric("sparkview_net_up", 1 if n["state"] == "UP" else 0, lbl, "1 when operstate is UP.")
        r.metric("sparkview_net_speed_gbps", n["speed_gbps"], lbl,
                 "Negotiated link speed in Gbps.")
        r.metric("sparkview_net_receive_bytes_total", n["rx_bytes"], lbl, "Bytes received.",
                 mtype="counter")
        r.metric("sparkview_net_transmit_bytes_total", n["tx_bytes"], lbl, "Bytes transmitted.",
                 mtype="counter")
        r.metric("sparkview_net_receive_packets_total", n["rx_packets"], lbl, "Packets received.",
                 mtype="counter")
        r.metric("sparkview_net_transmit_packets_total", n["tx_packets"], lbl,
                 "Packets transmitted.", mtype="counter")
        r.metric("sparkview_net_receive_errors_total", n["rx_errors"], lbl, "Receive errors.",
                 mtype="counter")
        r.metric("sparkview_net_transmit_errors_total", n["tx_errors"], lbl, "Transmit errors.",
                 mtype="counter")
        r.metric("sparkview_net_receive_dropped_total", n["rx_dropped"], lbl, "Receive drops.",
                 mtype="counter")

    # --- RDMA ports (traffic the netdev counters above cannot see) ---
    for p in s.get("rdma") or []:
        lbl = {"host": s["hostname"], "device": p["device"], "port": p["port"],
               "netdev": p["netdev"]}
        r.metric("sparkview_rdma_port_up", 1 if p["state"] == "ACTIVE" else 0, lbl,
                 "1 when the RDMA port state is ACTIVE.")
        r.metric("sparkview_rdma_port_rate_gbps", p["rate_gbps"], lbl,
                 "Negotiated RDMA port rate in Gbps.")
        r.metric("sparkview_rdma_transmit_bytes_total", p["tx_bytes"], lbl,
                 "Bytes transmitted on the RDMA port (port_xmit_data x 4).", mtype="counter")
        r.metric("sparkview_rdma_receive_bytes_total", p["rx_bytes"], lbl,
                 "Bytes received on the RDMA port (port_rcv_data x 4).", mtype="counter")
        r.metric("sparkview_rdma_transmit_packets_total", p["tx_packets"], lbl,
                 "Packets transmitted on the RDMA port.", mtype="counter")
        r.metric("sparkview_rdma_receive_packets_total", p["rx_packets"], lbl,
                 "Packets received on the RDMA port.", mtype="counter")
        for name, value in sorted({**p["errors"], **p["hw_counters"]}.items()):
            r.metric("sparkview_rdma_counter_total", value, {**lbl, "counter": name},
                     "RDMA port error, retransmission and congestion counters.",
                     mtype="counter")

    # --- GPU processes (label cardinality scales with --process-limit) ---
    cpu_seen = set()
    for p in s["processes"]:
        lbl = {
            "host": s["hostname"],
            "gpu": p.get("gpu"),
            "pid": p.get("pid"),
            "user": p.get("user"),
            "cmd": p.get("cmd"),
        }
        r.metric("sparkview_process_gpu_memory_bytes", p.get("gpu_mem"), lbl,
                 "Per-process GPU memory usage. A process spanning several cards "
                 "gets one series per GPU, so this sums correctly per host.")
        # CPU usage belongs to the process, not the card. Emit it once per pid and
        # leave the gpu label off, or a multi-GPU process would be counted twice.
        if p.get("pid") not in cpu_seen:
            cpu_seen.add(p.get("pid"))
            r.metric("sparkview_process_cpu_percent", p.get("cpu_pct"),
                     {k: v for k, v in lbl.items() if k != "gpu"},
                     "Per-process CPU usage.")

    return r.text()
