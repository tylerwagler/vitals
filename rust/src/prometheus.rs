//! `GET /metrics`: the /metrics.json snapshot as Prometheus text, in the
//! reference sparkview-exporter 0.6.0's `sparkview_*` series, for scrapers
//! that key on them (the Switchyard dashboard).
//!
//! Rendered from the same snapshot as /metrics.json, so the two cannot
//! disagree and several scrapers still cost one reading. Series names and
//! labels are the reference's, unchanged: dashboards already key on them. A
//! translation of pulsar-gui's exporter (051526e), line for line.

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;

fn psi_level(level: &Value) -> i64 {
    match level.as_str() {
        Some("LOW") => 0, Some("MOD") => 1, Some("HIGH") => 2, Some("CRITICAL") => 3, _ => -1,
    }
}

fn clock_state(status: &Value) -> i64 {
    match status.as_str() {
        Some("IDLE") => 0, Some("PASS") => 1, Some("LOCKED") => 2, Some("THROTTLED") => 3, _ => -1,
    }
}

fn esc(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// A label or sample value as Python's str() writes it.
fn text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(if *b { "True" } else { "False" }.into()),
        other => Some(other.to_string()),
    }
}

type Labels = Vec<(&'static str, Value)>;

#[derive(Default)]
struct Renderer {
    lines: Vec<String>,
    declared: HashSet<String>,
}

impl Renderer {
    fn metric(&mut self, name: &str, value: &Value, labels: &Labels, help: &str, mtype: &str) {
        let value = match value {
            Value::Null => return,
            Value::Bool(b) => i64::from(*b).to_string(),
            v => match text(v) {
                Some(t) => t,
                None => return,
            },
        };
        if self.declared.insert(name.to_string()) {
            if !help.is_empty() {
                self.lines.push(format!("# HELP {name} {help}"));
            }
            self.lines.push(format!("# TYPE {name} {mtype}"));
        }
        let rendered: Vec<String> = labels
            .iter()
            .filter_map(|(k, v)| text(v).map(|t| format!("{k}=\"{}\"", esc(&t))))
            .collect();
        if labels.is_empty() {
            self.lines.push(format!("{name} {value}"));
        } else {
            self.lines.push(format!("{name}{{{}}} {value}", rendered.join(",")));
        }
    }

    fn gauge(&mut self, name: &str, value: &Value, labels: &Labels, help: &str) {
        self.metric(name, value, labels, help, "gauge");
    }

    fn counter(&mut self, name: &str, value: &Value, labels: &Labels, help: &str) {
        self.metric(name, value, labels, help, "counter");
    }
}

fn with(base: &Labels, extra: &[(&'static str, Value)]) -> Labels {
    let mut l = base.clone();
    for (k, v) in extra {
        if let Some(slot) = l.iter_mut().find(|(lk, _)| lk == k) {
            slot.1 = v.clone();
        } else {
            l.push((k, v.clone()));
        }
    }
    l
}

fn sorted(v: &Value) -> BTreeMap<String, Value> {
    v.as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default()
}

pub fn render(s: &Value) -> String {
    let mut r = Renderer::default();
    let hostname = s["hostname"].clone();
    let host: Labels = vec![("host", hostname.clone())];
    let num = |v: f64| Value::from(v);

    r.gauge("sparkview_up", &Value::from(1), &host, "1 if the exporter collected a snapshot.");
    r.gauge("sparkview_scrape_duration_seconds", &s["scrape_duration_seconds"], &host, "Time spent collecting this snapshot.");
    let info = &s["info"];
    let or_unknown = |v: &Value| match v.as_str() {
        Some(t) if !t.is_empty() => Value::from(t),
        _ => Value::from("unknown"),
    };
    r.gauge("sparkview_build_info", &Value::from(1), &vec![
        ("host", hostname.clone()),
        ("exporter_version", s["exporter_version"].clone()),
        ("driver", or_unknown(&info["driver"])),
        ("cuda", or_unknown(&info["cuda"])),
        ("kernel", or_unknown(&info["kernel"])),
        ("gpu_name", or_unknown(&info["gpu_name"])),
    ], "Static build/driver labels; value is always 1.");
    let uptime = s["uptime_seconds"].as_f64().map(|u| num(crate::sysfs::round(u, 1))).unwrap_or(Value::Null);
    r.gauge("sparkview_uptime_seconds", &uptime, &host, "Host uptime.");

    // --- GPU ---
    for g in s["gpus"].as_array().into_iter().flatten() {
        let lbl: Labels = vec![("host", hostname.clone()), ("gpu", g["index"].clone()), ("name", g["name"].clone())];
        r.gauge("sparkview_gpu_utilization_percent", &g["utilization_percent"], &lbl, "GPU utilization percent.");
        r.gauge("sparkview_gpu_temperature_celsius", &g["temperature_celsius"], &lbl, "GPU temperature.");
        r.gauge("sparkview_gpu_power_watts", &g["power_watts"], &lbl, "GPU power draw via NVML.");
        r.gauge("sparkview_gpu_memory_total_bytes", &g["memory_total_bytes"], &lbl, "GPU memory total; on coherent UMA this is system MemTotal.");
        r.gauge("sparkview_gpu_memory_used_bytes", &g["memory_used_bytes"], &lbl, "GPU memory used; on coherent UMA this is MemTotal-MemAvailable.");
        r.gauge("sparkview_gpu_uma_coherent", &g["is_uma"], &lbl, "1 when GB10-style coherent unified memory is detected.");
        r.gauge("sparkview_gpu_clock_cap_mhz", &g["clock_cap"]["max_mhz"], &lbl,
                "Graphics clock ceiling applied by gpu-clock-cap.service; absent when no lab clamp is active. NVML cannot report a locked clock range.");
        for (reason, secs) in sorted(&g["clock_event_seconds"]) {
            r.counter("sparkview_gpu_clock_event_seconds_total", &secs, &with(&lbl, &[("reason", Value::from(reason))]),
                      "Cumulative time the GPU clock was held back, per reason.");
        }
        let ident = &g["identity"];
        if ident.as_object().is_some_and(|m| !m.is_empty()) {
            r.gauge("sparkview_gpu_info", &Value::from(1), &with(&lbl, &[
                ("uuid", ident["uuid"].clone()), ("pci_bus_id", ident["pci_bus_id"].clone()), ("vbios", ident["vbios"].clone()),
            ]), "Static GPU identity labels; value is always 1.");
        }
        let t = &g["throttle"];
        if t["available"].as_bool() == Some(true) {
            r.gauge("sparkview_gpu_clock_mhz", &t["clk_mhz"], &lbl, "Current graphics clock.");
            r.gauge("sparkview_gpu_clock_max_mhz", &t["clk_max_mhz"], &lbl, "Max graphics clock.");
            r.gauge("sparkview_gpu_clock_threshold_mhz", &t["clk_threshold_mhz"], &lbl, "Clock below which this GPU is called THROTTLED under load.");
            r.gauge("sparkview_gpu_clock_state", &Value::from(clock_state(&t["status"])), &lbl, "Load-gated clock state: 0=IDLE 1=PASS 2=LOCKED 3=THROTTLED.");
            r.gauge("sparkview_gpu_throttled", &Value::from(i64::from(t["status"] == "THROTTLED")), &lbl, "1 when the GPU is throttled under load.");
            r.gauge("sparkview_gpu_problem_throttle", &t["problem_throttle"], &lbl, "1 when NVML reports a problem throttle reason.");
            for reason in t["throttle_reasons"].as_array().into_iter().flatten() {
                r.gauge("sparkview_gpu_throttle_reason", &Value::from(1), &with(&lbl, &[("reason", reason.clone())]),
                        "Active NVML throttle reasons, one series per active reason.");
            }
        }
    }

    // --- system memory / swap ---
    let m = &s["memory"];
    r.gauge("sparkview_memory_total_bytes", &m["total_bytes"], &host, "System memory total.");
    r.gauge("sparkview_memory_used_bytes", &m["used_bytes"], &host, "System memory used (MemTotal-MemAvailable).");
    r.gauge("sparkview_memory_available_bytes", &m["available_bytes"], &host, "MemAvailable.");
    r.gauge("sparkview_memory_used_percent", &m["percent"], &host, "System memory used percent.");
    r.gauge("sparkview_swap_total_bytes", &m["swap_total_bytes"], &host, "Swap total.");
    r.gauge("sparkview_swap_used_bytes", &m["swap_used_bytes"], &host, "Swap used.");
    r.gauge("sparkview_swap_used_percent", &m["swap_percent"], &host, "Swap used percent.");

    // --- PSI ---
    let psi = &s["pressure"];
    if psi["available"].as_bool() == Some(true) {
        r.gauge("sparkview_psi_memory_some_avg10", &psi["some_avg10"], &host, "PSI memory 'some' avg10.");
        r.gauge("sparkview_psi_memory_full_avg10", &psi["full_avg10"], &host, "PSI memory 'full' avg10.");
        r.gauge("sparkview_psi_memory_level", &Value::from(psi_level(&psi["level"])), &host, "Memory pressure level: 0=LOW 1=MOD 2=HIGH 3=CRITICAL.");
    }

    // --- CPU ---
    let c = &s["cpu"];
    r.gauge("sparkview_cpu_utilization_percent", &c["utilization_percent"], &host, "Mean CPU utilization across cores.");
    r.gauge("sparkview_cpu_cores_total", &c["cores_total"], &host, "Total CPU cores.");
    r.gauge("sparkview_cpu_cores_active", &c["cores_active"], &host, "Cores with non-zero usage.");
    r.gauge("sparkview_cpu_temperature_celsius", &c["temperature_celsius"], &host, "CPU package temperature.");

    // --- GB10 rails / temps / energy via spark_hwmon (spbm) ---
    let spbm = &s["spbm"];
    if spbm.as_object().is_some_and(|m| !m.is_empty()) {
        for (rail, ch) in sorted(&spbm["power_watts"]) {
            let lbl: Labels = vec![("host", hostname.clone()), ("rail", Value::from(rail.clone()))];
            r.gauge("sparkview_spbm_power_watts", &ch["input"], &lbl, "Instantaneous draw per spark_hwmon power rail.");
            r.gauge("sparkview_spbm_power_cap_watts", &ch["cap"], &lbl, "Configured cap for this rail.");
            r.gauge("sparkview_spbm_power_max_watts", &ch["max"], &lbl, "Maximum settable cap for this rail.");
            r.gauge("sparkview_spbm_power_min_watts", &ch["min"], &lbl, "Minimum settable cap for this rail.");
            if let Some(v) = spbm["cap_exceeded"].get(&rail) {
                r.gauge("sparkview_spbm_cap_exceeded", v, &lbl, "1 when a rail's instantaneous draw exceeds its cap.");
            }
        }
        for (sensor, ch) in sorted(&spbm["temperatures_celsius"]) {
            r.gauge("sparkview_spbm_temperature_celsius", &ch["input"], &vec![("host", hostname.clone()), ("sensor", Value::from(sensor))],
                    "Per-sensor temperature from spark_hwmon.");
        }
        for (rail, ch) in sorted(&spbm["energy_joules"]) {
            r.counter("sparkview_spbm_energy_joules_total", &ch["input"], &vec![("host", hostname.clone()), ("rail", Value::from(rail))],
                      "Cumulative energy per rail since boot.");
        }
        r.gauge("sparkview_spbm_gpu_peak_watts", &spbm["peak_gpu_watts"], &host, "Peak GPU rail draw seen since the exporter started.");
        r.gauge("sparkview_tj_rise_celsius", &spbm["tj_rise_celsius"], &host, "Junction temperature rise in degrees C.");
        r.gauge("sparkview_pl_level", &spbm["pl_level"], &host, "Active power-limit level reported by spark_hwmon.");
        r.gauge("sparkview_prochot", &spbm["prochot"], &host,
                "Raw PROCHOT line state. Reads 1 on healthy GB10 nodes at full clock on this firmware, so correlate with sparkview_gpu_clock_state rather than alerting on this alone.");
    }

    // --- every thermal sensor on the platform ---
    for z in s["thermal_zones"].as_array().into_iter().flatten() {
        r.gauge("sparkview_thermal_zone_celsius", &z["celsius"], &vec![("host", hostname.clone()), ("zone", z["zone"].clone()), ("type", z["type"].clone())],
                "Per-zone temperature from /sys/class/thermal.");
    }
    for t in s["hwmon_temperatures"].as_array().into_iter().flatten() {
        r.gauge("sparkview_hwmon_temperature_celsius", &t["celsius"], &vec![("host", hostname.clone()), ("chip", t["chip"].clone()), ("sensor", t["sensor"].clone())],
                "Every temp input exposed under /sys/class/hwmon.");
    }

    // --- network (ConnectX-7) ---
    for n in s["network"].as_array().into_iter().flatten() {
        let lbl: Labels = vec![("host", hostname.clone()), ("iface", n["iface"].clone())];
        r.gauge("sparkview_net_connectx7", &n["cx7"], &lbl, "1 when the interface is a ConnectX-7 / mlx5 link.");
        r.gauge("sparkview_net_up", &Value::from(i64::from(n["state"] == "UP")), &lbl, "1 when operstate is UP.");
        r.gauge("sparkview_net_speed_gbps", &n["speed_gbps"], &lbl, "Negotiated link speed in Gbps.");
        r.counter("sparkview_net_receive_bytes_total", &n["rx_bytes"], &lbl, "Bytes received.");
        r.counter("sparkview_net_transmit_bytes_total", &n["tx_bytes"], &lbl, "Bytes transmitted.");
        r.counter("sparkview_net_receive_packets_total", &n["rx_packets"], &lbl, "Packets received.");
        r.counter("sparkview_net_transmit_packets_total", &n["tx_packets"], &lbl, "Packets transmitted.");
        r.counter("sparkview_net_receive_errors_total", &n["rx_errors"], &lbl, "Receive errors.");
        r.counter("sparkview_net_transmit_errors_total", &n["tx_errors"], &lbl, "Transmit errors.");
        r.counter("sparkview_net_receive_dropped_total", &n["rx_dropped"], &lbl, "Receive drops.");
    }

    // --- RDMA ports (traffic the netdev counters above cannot see) ---
    for p in s["rdma"].as_array().into_iter().flatten() {
        let lbl: Labels = vec![("host", hostname.clone()), ("device", p["device"].clone()), ("port", p["port"].clone()), ("netdev", p["netdev"].clone())];
        r.gauge("sparkview_rdma_port_up", &Value::from(i64::from(p["state"] == "ACTIVE")), &lbl, "1 when the RDMA port state is ACTIVE.");
        r.gauge("sparkview_rdma_port_rate_gbps", &p["rate_gbps"], &lbl, "Negotiated RDMA port rate in Gbps.");
        r.counter("sparkview_rdma_transmit_bytes_total", &p["tx_bytes"], &lbl, "Bytes transmitted on the RDMA port (port_xmit_data x 4).");
        r.counter("sparkview_rdma_receive_bytes_total", &p["rx_bytes"], &lbl, "Bytes received on the RDMA port (port_rcv_data x 4).");
        r.counter("sparkview_rdma_transmit_packets_total", &p["tx_packets"], &lbl, "Packets transmitted on the RDMA port.");
        r.counter("sparkview_rdma_receive_packets_total", &p["rx_packets"], &lbl, "Packets received on the RDMA port.");
        let mut counters = sorted(&p["errors"]);
        counters.extend(sorted(&p["hw_counters"]));
        for (name, value) in counters {
            r.counter("sparkview_rdma_counter_total", &value, &with(&lbl, &[("counter", Value::from(name))]),
                      "RDMA port error, retransmission and congestion counters.");
        }
    }

    // --- GPU processes ---
    let mut cpu_seen = HashSet::new();
    for p in s["processes"].as_array().into_iter().flatten() {
        let lbl: Labels = vec![("host", hostname.clone()), ("gpu", p["gpu"].clone()), ("pid", p["pid"].clone()),
                               ("user", p["user"].clone()), ("cmd", p["cmd"].clone())];
        r.gauge("sparkview_process_gpu_memory_bytes", &p["gpu_mem"], &lbl,
                "Per-process GPU memory usage. A process spanning several cards gets one series per GPU, so this sums correctly per host.");
        // CPU usage belongs to the process, not the card. Emit it once per pid
        // and leave the gpu label off, or a multi-GPU process would be counted twice.
        if cpu_seen.insert(p["pid"].to_string()) {
            let no_gpu: Labels = lbl.iter().filter(|(k, _)| *k != "gpu").cloned().collect();
            r.gauge("sparkview_process_cpu_percent", &p["cpu_pct"], &no_gpu, "Per-process CPU usage.");
        }
    }

    r.lines.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    #[test]
    fn prometheus_text_keeps_the_reference_series() {
        let reference: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/reference-0.6.0.json")).unwrap();
        let text = format!("\n{}", super::render(&reference));
        for series in ["sparkview_up", "sparkview_build_info", "sparkview_gpu_utilization_percent",
                       "sparkview_gpu_temperature_celsius", "sparkview_gpu_power_watts",
                       "sparkview_gpu_memory_used_bytes", "sparkview_gpu_clock_mhz",
                       "sparkview_memory_used_percent", "sparkview_net_up", "sparkview_prochot",
                       "sparkview_uptime_seconds", "sparkview_rdma_transmit_bytes_total"] {
            assert!(text.contains(&format!("\n{series}")), "{series}");
        }
    }
}
