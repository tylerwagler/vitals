//! `GET /metrics.json`: the snapshot pulsar-gui's cluster view reads.
//!
//! Its shape is the reference sparkview-exporter 0.6.0's, as pulsar-gui's
//! `tests/fixtures/sparkview-*.json` capture it, and it does not change: new
//! data goes in the /v1 API. Readings are taken when a request arrives and
//! shared by requests within 250 ms. A reading that cannot be taken is left
//! out or null, never zero: pulsar-gui treats a zero as a reading.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{json, Map, Value};

use crate::collect::gpu::{clock_status, decode_reasons, gpu_memory, Gpu, U32, CLOCK_THRESHOLD_MHZ, PROBLEM_BITS};
use crate::collect::memory::{meminfo, psi_level};
use crate::collect::network::{rdma, rdma_netlink_counters, IB_ROOT};
use crate::collect::processes::read_proc;
use crate::collect::sensors::{thermal_zones, Spbm, HWMON_ROOT, SPBM_ACPI_DIR, THERMAL_ROOT};
use crate::hub::Hub;
use crate::os;
use crate::sysfs::{basename, exists, list, list_natural, read_int, read_text, round, scaled};
use crate::VERSION;

const CPU_TEMP_CHIPS: &[&str] = &["coretemp", "k10temp", "cpu_thermal", "acpitz"];

#[derive(Default)]
struct State {
    cached: Option<(Instant, Arc<Vec<u8>>)>,
    // psutil.cpu_percent(percpu=True)'s previous times, per CPU
    cpu_prev: HashMap<i64, (f64, f64)>,
    // GPU processes' (start ticks, cpu seconds, at) at the previous scrape
    procs: HashMap<i64, (i64, f64, f64)>,
}

pub struct Exporter {
    hub: Arc<Hub>,
    spbm: Spbm,
    state: Mutex<State>,
}

/// (total, busy) seconds of each CPU, as psutil counts busy: all but idle and
/// iowait, guest time being inside user already.
fn cpu_busy() -> Vec<(i64, f64, f64)> {
    let hz = os::clock_ticks();
    read_text("/proc/stat")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("cpu")?;
            let mut f = rest.split_whitespace();
            let n: i64 = f.next()?.parse().ok()?;
            let v: Vec<f64> = f.take(10).filter_map(|x| x.parse::<f64>().ok()).map(|t| t / hz).collect();
            if v.len() < 4 {
                return None;
            }
            let total: f64 = v.iter().take(8).sum();
            let idle = v[3] + v.get(4).copied().unwrap_or(0.0);
            Some((n, total, total - idle))
        })
        .collect()
}

fn format_uptime(seconds: Option<f64>) -> String {
    let Some(s) = seconds else { return String::new() };
    let minutes = (s / 60.0).floor() as i64;
    let (days, rem) = (minutes / (24 * 60), minutes % (24 * 60));
    let (hours, mins) = (rem / 60, rem % 60);
    if days > 0 { format!("{days}d {hours}h {mins}m") } else { format!("{hours}h {mins}m") }
}

fn local_time() -> String {
    let t = os::now() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let mut buf = [0u8; 32];
    // SAFETY: tm and buf are valid; strftime writes at most buf.len() bytes.
    let n = unsafe {
        libc::localtime_r(&t, &mut tm);
        libc::strftime(buf.as_mut_ptr() as *mut libc::c_char, buf.len(), c"%I:%M:%S %p".as_ptr(), &tm)
    };
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

fn hwmon_temperatures() -> Vec<Value> {
    let mut out = Vec::new();
    for hw in list_natural(HWMON_ROOT, |n| n.starts_with("hwmon")) {
        let chip = read_text(hw.join("name"));
        for inp in list(&hw, |n| n.starts_with("temp") && n.ends_with("_input")) {
            let p = inp.to_string_lossy();
            let sensor = &p[..p.len() - "_input".len()];
            out.push(json!({
                "chip": chip,
                "sensor": read_text(format!("{sensor}_label")).filter(|l| !l.is_empty()).unwrap_or_else(|| basename(sensor)),
                "celsius": scaled(&inp, 1e-3, 2),
            }));
        }
    }
    out
}

fn network() -> Vec<Value> {
    list("/sys/class/net", |_| true)
        .iter()
        .filter(|p| exists(p.join("device")))
        .map(|path| {
            let stats = path.join("statistics");
            let speed = read_int(path.join("speed"));
            let driver = crate::sysfs::realpath(path.join("device/driver")).map(basename).unwrap_or_default();
            json!({
                "iface": basename(path),
                "state": read_text(path.join("operstate")).unwrap_or_else(|| "unknown".into()).to_uppercase(),
                "speed_gbps": speed.filter(|s| *s > 0).map(|s| s as f64 / 1000.0),
                "rx_bytes": read_int(stats.join("rx_bytes")),
                "tx_bytes": read_int(stats.join("tx_bytes")),
                "rx_packets": read_int(stats.join("rx_packets")),
                "tx_packets": read_int(stats.join("tx_packets")),
                "rx_errors": read_int(stats.join("rx_errors")),
                "tx_errors": read_int(stats.join("tx_errors")),
                "rx_dropped": read_int(stats.join("rx_dropped")),
                "cx7": driver == "mlx5_core",
            })
        })
        .collect()
}

fn pressure() -> Value {
    let Some(text) = read_text("/proc/pressure/memory") else { return json!({"available": false}) };
    let mut avg: HashMap<String, f64> = HashMap::new();
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let Some(kind) = f.next() else { continue };
        for field in f {
            if let Some(v) = field.strip_prefix("avg10=").and_then(|v| v.parse().ok()) {
                avg.insert(kind.to_string(), v);
            }
        }
    }
    let (some, full) = (avg.get("some").copied().unwrap_or(0.0), avg.get("full").copied().unwrap_or(0.0));
    json!({"available": true, "some_avg10": some, "full_avg10": full, "level": psi_level(some, full)})
}

impl Exporter {
    pub fn new(hub: Arc<Hub>) -> Self {
        let ex = Exporter { hub, spbm: Spbm::new(HWMON_ROOT, SPBM_ACPI_DIR), state: Mutex::new(State::default()) };
        ex.cpu(&[]); // prime the first interval
        ex
    }

    fn cpu(&self, hwmon_temps: &[Value]) -> Value {
        let now = cpu_busy();
        let mut s = self.state.lock().unwrap();
        let per_core: Vec<f64> = now
            .iter()
            .map(|(n, total, busy)| {
                let pct = match s.cpu_prev.get(n) {
                    Some((pt, pb)) if total > pt => round(((busy - pb) / (total - pt) * 100.0).clamp(0.0, 100.0), 1),
                    // psutil, primed at start, never sees this; keep it quiet
                    _ => 0.0,
                };
                pct
            })
            .collect();
        s.cpu_prev = now.iter().map(|(n, t, b)| (*n, (*t, *b))).collect();
        let mut temp = None;
        for chip in CPU_TEMP_CHIPS {
            let readings: Vec<f64> = hwmon_temps.iter().filter(|t| t["chip"] == *chip).filter_map(|t| t["celsius"].as_f64()).collect();
            if !readings.is_empty() {
                temp = readings.into_iter().reduce(f64::max);
                break;
            }
        }
        json!({
            "utilization_percent": if per_core.is_empty() { Value::Null } else { json!(per_core.iter().sum::<f64>() / per_core.len() as f64) },
            "cores_total": os::cpu_count(),
            "cores_active": per_core.iter().filter(|c| **c > 0.0).count(),
            "temperature_celsius": temp,
        })
    }

    fn memory(info: &Map<String, Value>) -> Value {
        let g = |k: &str| info.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        let (total, avail) = (g("mem_total_bytes"), g("mem_available_bytes"));
        let (st, sf) = (g("swap_total_bytes"), g("swap_free_bytes"));
        let pct = |used: i64, total: i64| if total > 0 { round(used as f64 / total as f64 * 100.0, 1) } else { 0.0 };
        json!({
            // total - available, not psutil's "used": on a UMA host the page
            // cache the GPU can reclaim is not memory in use.
            "total_bytes": total,
            "used_bytes": total - avail,
            "available_bytes": avail,
            "percent": pct(total - avail, total),
            "swap_total_bytes": st,
            "swap_used_bytes": st - sf,
            "swap_percent": pct(st - sf, st),
        })
    }

    fn gpu_row(&self, g: &Gpu, total: i64, avail: i64, clock_cap: &Value,
               seen: &mut std::collections::HashSet<i64>) -> (Value, Vec<Value>) {
        let nv = &self.hub.nv;
        let util = g.utilization();
        let util_pct = util.map(|u| u.0);
        let (uma, mem_total, mem_used) = gpu_memory(g.memory(), total, avail);
        let clk = g.clock(0);
        let uuid = g.uuid();
        let clk_max = nv.max_graphics_clock(g, uuid.as_deref().unwrap_or(""));
        let pstate = g.u32_of(U32::PerformanceState);
        let raw = nv.reasons(g); // shared with /v1, at most once a second
        let throttle = match (clk, clk_max, pstate, raw) {
            (Some(clk), Some(max), Some(ps), Some(raw)) => {
                let ps = format!("P{ps}");
                let problem = raw & PROBLEM_BITS != 0;
                let (status, locked) = clock_status(clk as f64, max as f64, util_pct.unwrap_or(0) as f64, &ps, problem);
                json!({
                    "available": true, "status": status, "clk_mhz": clk as f64, "clk_max_mhz": max as f64,
                    "clk_threshold_mhz": CLOCK_THRESHOLD_MHZ, "pstate": ps, "util_pct": util_pct,
                    "throttle_raw": raw, "throttle_reasons": decode_reasons(raw), "problem_throttle": problem, "locked": locked,
                })
            }
            _ => json!({"available": false}),
        };
        let pci = g.pci();
        let mut events = Map::new();
        for (name, seconds) in g.events() {
            events.insert(name.to_string(), json!(seconds));
        }
        let row = json!({
            "index": g.index,
            "name": g.name(),
            "utilization_percent": util_pct,
            "temperature_celsius": g.u32_of(U32::Temperature),
            "power_watts": g.u32_of(U32::PowerUsage).map(|p| round(p as f64 / 1000.0, 3)),
            "is_uma": uma,
            "memory_total_bytes": mem_total,
            "memory_used_bytes": mem_used,
            "throttle": throttle,
            "identity": {
                "uuid": uuid,
                "pci_bus_id": pci.map(|p| unsafe { std::ffi::CStr::from_ptr(p.busId.as_ptr()) }.to_string_lossy().into_owned()),
                "vbios": g.vbios(),
            },
            "clock_event_seconds": events,
            "clock_cap": clock_cap,
        });
        (row, self.processes(g, seen))
    }

    /// One card's processes; `seen` collects every pid of this scrape, so
    /// the snapshot can forget the ones that left once all cards are read.
    fn processes(&self, g: &Gpu, seen_all: &mut std::collections::HashSet<i64>) -> Vec<Value> {
        let mut seen = std::collections::HashSet::new();
        let now = os::mono();
        let mut s = self.state.lock().unwrap();
        let mut rows: Vec<Value> = g
            .processes()
            .into_iter()
            .filter(|(pid, _, _)| seen.insert(*pid as i64))
            .map(|(pid, mem, _)| {
                let pid = pid as i64;
                let p = read_proc(pid);
                // cpu_pct since the last scrape this process was seen in, as
                // psutil's cached Process gives it; 0 the first time.
                let cpu = p.as_ref().map(|p| {
                    let pct = match s.procs.get(&pid) {
                        Some((start, cpu_s, at)) if *start == p.start_ticks && now > *at => round(100.0 * (p.cpu_s - cpu_s).max(0.0) / (now - at), 1),
                        _ => 0.0,
                    };
                    s.procs.insert(pid, (p.start_ticks, p.cpu_s, now));
                    pct
                });
                json!({
                    "pid": pid,
                    "user": p.as_ref().and_then(|p| p.uid).map(os::user_name),
                    "cmd": p.as_ref().map(|p| p.name.clone()),
                    "gpu_mem": mem,
                    "cpu_pct": cpu,
                })
            })
            .collect();
        seen_all.extend(seen);
        rows.sort_by_key(|r| std::cmp::Reverse(r["gpu_mem"].as_i64().unwrap_or(0)));
        rows
    }

    pub fn snapshot(&self) -> Value {
        let started = Instant::now();
        let info = meminfo(None);
        let total = info.get("mem_total_bytes").and_then(|v| v.as_i64()).unwrap_or(0);
        let avail = info.get("mem_available_bytes").and_then(|v| v.as_i64()).unwrap_or(0);
        let gpus = self.hub.nv.gpus();
        let cap = if gpus.is_empty() { Value::Null } else { self.hub.clock_cap.get() };
        let mut rows = Vec::new();
        let mut procs = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for g in &gpus {
            let (row, p) = self.gpu_row(g, total, avail, &cap, &mut seen);
            rows.push(row);
            procs.extend(p);
        }
        self.state.lock().unwrap().procs.retain(|pid, _| seen.contains(pid));
        let up = read_text("/proc/uptime").and_then(|u| u.split_whitespace().next()?.parse::<f64>().ok());
        let hwmon = hwmon_temperatures();
        let (driver, cuda, _) = self.hub.nv.versions();
        let gpu_name = rows.first().and_then(|r| r["name"].as_str()).unwrap_or("").to_string();
        json!({
            "hostname": os::hostname(),
            "exporter_version": VERSION,
            "timestamp": os::now(),
            "uptime_seconds": up,
            "info": {
                "time": local_time(),
                "uptime": format_uptime(up),
                "kernel": os::uname().release,
                "driver": driver.unwrap_or_default(),
                "cuda": cuda.unwrap_or_default(),
                "gpu_name": gpu_name,
            },
            "gpus": rows,
            "memory": Self::memory(&info),
            "pressure": pressure(),
            "cpu": self.cpu(&hwmon),
            "spbm": self.spbm.collect(),
            "thermal_zones": thermal_zones(THERMAL_ROOT),
            "hwmon_temperatures": hwmon,
            "network": network(),
            "rdma": rdma(IB_ROOT, rdma_netlink_counters().as_ref()),
            "processes": procs,
            "scrape_duration_seconds": round(started.elapsed().as_secs_f64(), 4),
        })
    }

    /// The JSON body, shared by requests that land within 250 ms: several
    /// dashboards polling at once then cost one scrape, and the rate-style
    /// readings (CPU, process CPU) keep a sensible interval.
    pub fn body(&self) -> Arc<Vec<u8>> {
        {
            let s = self.state.lock().unwrap();
            if let Some((at, body)) = &s.cached {
                if at.elapsed().as_secs_f64() <= 0.25 {
                    return body.clone();
                }
            }
        }
        let body = Arc::new(serde_json::to_vec(&self.snapshot()).unwrap_or_default());
        self.state.lock().unwrap().cached = Some((Instant::now(), body.clone()));
        body
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn uptime_format() {
        assert_eq!(super::format_uptime(Some(54270.32)), "15h 4m");
        assert_eq!(super::format_uptime(Some(3.0 * 86400.0 + 29.0 * 60.0)), "3d 0h 29m");
    }
}
