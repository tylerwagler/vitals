//! CPUs: what they are (models, clusters, caches, flags, NUMA) and what they
//! are doing (per-core utilization by kind, frequency, temperature).

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use serde_json::{json, Map, Value};

use crate::os;
use crate::rates::Rates;
use crate::sysfs::{basename, cpu_list, list, list_natural, read_int, read_text, round, run};

const CPU_DIR: &str = "/sys/devices/system/cpu";
// Chips whose hottest sensor is "the CPU temperature", most specific first.
const CPU_TEMP_CHIPS: &[&str] = &["coretemp", "k10temp", "zenpower", "cpu_thermal", "acpitz"];
// /proc/stat's columns, as psutil names them.
const TIME_FIELDS: &[&str] = &["user", "nice", "system", "idle", "iowait", "irq", "softirq", "steal", "guest", "guest_nice"];
// Time that is not work. guest/guest_nice are already inside user/nice.
const IDLE_FIELDS: &[&str] = &["idle", "iowait"];
const NOT_SUMMED: &[&str] = &["guest", "guest_nice"];

fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64().map(|x| round(x, 1)),
        Value::String(s) => s.trim().parse::<f64>().ok().map(|x| round(x, 1)),
        _ => None,
    }
}

/// Per-CPU rows from lscpu, which knows every ARM part name.
fn lscpu_cpus() -> Vec<Map<String, Value>> {
    for columns in [
        "CPU,CORE,SOCKET,NODE,CLUSTER,MAXMHZ,MINMHZ,MODELNAME",
        "CPU,CORE,SOCKET,NODE,MAXMHZ,MINMHZ,MODELNAME",
        "CPU,CORE,SOCKET,NODE,MAXMHZ,MINMHZ",
    ] {
        if let Some(out) = run(&["lscpu", "-J", &format!("--extended={columns}")]) {
            return serde_json::from_str::<Value>(&out)
                .ok()
                .and_then(|v| v.get("cpus").and_then(|c| c.as_array()).cloned())
                .unwrap_or_default()
                .into_iter()
                .filter_map(|r| r.as_object().cloned())
                .collect();
        }
    }
    Vec::new()
}

fn caches() -> Value {
    let rows = run(&["lscpu", "-J", "-C"])
        .and_then(|o| serde_json::from_str::<Value>(&o).ok())
        .and_then(|v| v.get("caches").and_then(|c| c.as_array()).cloned())
        .unwrap_or_default();
    Value::Array(
        rows.iter()
            .map(|r| {
                json!({
                    "name": r.get("name"), "level": r.get("level"), "type": r.get("type"),
                    "one_size": r.get("one-size"), "all_size": r.get("all-size"), "ways": r.get("ways"),
                })
            })
            .collect(),
    )
}

/// (model name, feature flags) of the first processor in /proc/cpuinfo.
fn cpuinfo() -> (Option<String>, Vec<String>) {
    let (mut model, mut flags) = (None, Vec::new());
    for line in read_text("/proc/cpuinfo").unwrap_or_default().lines() {
        let (key, value) = line.split_once(':').unwrap_or((line, ""));
        let key = key.trim();
        if key == "model name" && model.is_none() {
            model = Some(value.trim().to_string());
        } else if (key == "flags" || key == "Features") && flags.is_empty() {
            flags = value.split_whitespace().map(String::from).collect();
        }
    }
    (model, flags)
}

/// Physical cores, as psutil counts them: distinct core sibling sets.
fn physical_cores(online: &[i64]) -> Option<i64> {
    let mut sets = BTreeSet::new();
    for n in online {
        let s = read_text(format!("{CPU_DIR}/cpu{n}/topology/core_cpus_list"))
            .or_else(|| read_text(format!("{CPU_DIR}/cpu{n}/topology/thread_siblings_list")));
        if let Some(s) = s {
            sets.insert(s);
        }
    }
    if sets.is_empty() { None } else { Some(sets.len() as i64) }
}

pub fn static_() -> Value {
    let online = cpu_list(read_text(format!("{CPU_DIR}/online")).as_deref());
    let rows: HashMap<i64, Map<String, Value>> = lscpu_cpus()
        .into_iter()
        .filter_map(|r| {
            let cpu = match r.get("cpu")? {
                Value::Number(n) => n.as_i64(),
                Value::String(s) => s.parse().ok(),
                _ => None,
            }?;
            Some((cpu, r))
        })
        .collect();
    let (model_name, flags) = cpuinfo();

    let mut clusters: Vec<((Option<String>, Option<f64>, Option<f64>), Vec<i64>)> = Vec::new();
    for &n in &online {
        let row = rows.get(&n);
        let model = row
            .and_then(|r| r.get("modelname"))
            .and_then(|m| m.as_str())
            .map(String::from)
            .or_else(|| model_name.clone());
        let khz = |f: &str| read_int(format!("{CPU_DIR}/cpu{n}/cpufreq/{f}")).map(|k| round(k as f64 / 1000.0, 1)).filter(|v| *v != 0.0);
        let top = num(row.and_then(|r| r.get("maxmhz"))).filter(|v| *v != 0.0).or_else(|| khz("cpuinfo_max_freq"));
        let low = num(row.and_then(|r| r.get("minmhz"))).filter(|v| *v != 0.0).or_else(|| khz("cpuinfo_min_freq"));
        let key = (model, top, low);
        match clusters.iter_mut().find(|(k, _)| *k == key) {
            Some((_, cpus)) => cpus.push(n),
            None => clusters.push((key, vec![n])),
        }
    }

    let packages: BTreeSet<i64> = online
        .iter()
        .filter_map(|n| read_int(format!("{CPU_DIR}/cpu{n}/topology/physical_package_id")))
        .collect();
    let nodes: Vec<Value> = list_natural("/sys/devices/system/node", |n| {
        n.starts_with("node") && n[4..].chars().next().is_some_and(|c| c.is_ascii_digit())
    })
    .iter()
    .map(|node| {
        let total = read_text(node.join("meminfo")).and_then(|m| {
            m.lines().find(|l| l.contains("MemTotal:")).and_then(|l| {
                let parts: Vec<&str> = l.split_whitespace().collect();
                parts.get(parts.len().wrapping_sub(2)).and_then(|v| v.parse::<i64>().ok()).map(|k| k * 1024)
            })
        });
        json!({
            "node": basename(node)[4..].parse::<i64>().unwrap_or(0),
            "cpus": cpu_list(read_text(node.join("cpulist")).as_deref()),
            "memory_total_bytes": total,
        })
    })
    .collect();
    let mut vulns = Map::new();
    for p in list(format!("{CPU_DIR}/vulnerabilities"), |_| true) {
        vulns.insert(basename(&p), json!(read_text(&p)));
    }
    let model = model_name.clone().or_else(|| clusters.first().and_then(|((m, _, _), _)| m.clone()));
    json!({
        "model": model,
        "logical_cpus": os::cpu_count(),
        "physical_cores": physical_cores(&online),
        "sockets": if packages.is_empty() { Value::Null } else { json!(packages.len()) },
        "online": online,
        "isolated": cpu_list(read_text(format!("{CPU_DIR}/isolated")).as_deref()),
        "smt": {"active": read_int(format!("{CPU_DIR}/smt/active")), "control": read_text(format!("{CPU_DIR}/smt/control"))},
        "clusters": clusters.iter().map(|((m, top, low), cpus)| json!({
            "model": m, "max_mhz": top, "min_mhz": low, "count": cpus.len(), "cpus": cpus,
        })).collect::<Vec<_>>(),
        "caches": caches(),
        "numa_nodes": nodes,
        "frequency_driver": read_text(format!("{CPU_DIR}/cpu0/cpufreq/scaling_driver")),
        "governor": read_text(format!("{CPU_DIR}/cpu0/cpufreq/scaling_governor")),
        "energy_performance_preference": read_text(format!("{CPU_DIR}/cpu0/cpufreq/energy_performance_preference")),
        "boost": read_int(format!("{CPU_DIR}/cpufreq/boost")),
        "flags": flags,
        "vulnerabilities": vulns,
    })
}

/// (cpu number or None for the total, seconds per field) from /proc/stat.
fn cpu_times() -> Vec<(Option<i64>, Vec<f64>)> {
    let hz = os::clock_ticks();
    let mut out = Vec::new();
    for line in read_text("/proc/stat").unwrap_or_default().lines() {
        let Some(rest) = line.strip_prefix("cpu") else { continue };
        let mut parts = rest.split_whitespace();
        let label = if rest.starts_with(' ') { "" } else { parts.next().unwrap_or("") };
        let values: Vec<f64> = parts.take(TIME_FIELDS.len()).filter_map(|v| v.parse::<f64>().ok()).map(|t| t / hz).collect();
        out.push((if label.is_empty() { None } else { label.parse().ok() }, values));
    }
    out
}

/// Percent of the interval in each kind of time, from cumulative seconds.
fn breakdown(rates: &Rates, key: &str, times: &[f64]) -> Option<Map<String, Value>> {
    let fields = &TIME_FIELDS[..times.len()];
    let deltas: Vec<Option<f64>> = fields.iter().zip(times).map(|(f, v)| rates.delta(&format!("{key}/{f}"), Some(*v))).collect();
    if deltas.iter().any(|d| d.is_none()) {
        return None;
    }
    let deltas: Vec<f64> = deltas.into_iter().flatten().collect();
    let total: f64 = fields.iter().zip(&deltas).filter(|(f, _)| !NOT_SUMMED.contains(f)).map(|(_, d)| d).sum();
    if total <= 0.0 {
        return None;
    }
    let mut out = Map::new();
    let mut idle = 0.0;
    for (f, d) in fields.iter().zip(&deltas) {
        let pct = round(100.0 * d / total, 2);
        if IDLE_FIELDS.contains(f) {
            idle += pct;
        }
        out.insert(f.to_string(), json!(pct));
    }
    out.insert("utilization".into(), json!(round(100.0 - idle, 2)));
    Some(out)
}

/// The hottest sensor of the first CPU chip present.
pub fn temperature(hwmon: &[Value]) -> Option<f64> {
    for chip in CPU_TEMP_CHIPS {
        let readings: Vec<f64> = hwmon
            .iter()
            .filter(|s| s["chip"] == *chip && s["kind"] == "temp")
            .filter_map(|s| s["value"].as_f64())
            .collect();
        if !readings.is_empty() {
            return readings.into_iter().reduce(f64::max);
        }
    }
    None
}

/// x86 thermal-throttle event counters, at most every 10 s: each of those
/// sysfs files is an MSR read on its CPU, 50+ ms for a 24-core host. The
/// rate is over the counters' own interval, not the sample's.
pub struct ThrottleCounts {
    state: Mutex<(f64, Option<(f64, HashMap<String, i64>)>, Value)>,
}

impl Default for ThrottleCounts {
    fn default() -> Self {
        ThrottleCounts { state: Mutex::new((f64::NEG_INFINITY, None, Value::Null)) }
    }
}

impl ThrottleCounts {
    const TTL_S: f64 = 10.0;

    pub fn get(&self) -> Value {
        let mut s = self.state.lock().unwrap();
        let now = os::mono();
        if now - s.0 < Self::TTL_S {
            return s.2.clone();
        }
        s.0 = now;
        let mut counts = HashMap::new();
        for kind in ["core", "package"] {
            let values: Vec<i64> = list(CPU_DIR, |n| n.starts_with("cpu"))
                .iter()
                .filter_map(|c| read_int(c.join(format!("thermal_throttle/{kind}_throttle_count"))))
                .collect();
            if !values.is_empty() {
                counts.insert(kind.to_string(), values.iter().sum::<i64>());
            }
        }
        if counts.is_empty() {
            s.2 = Value::Null;
            return Value::Null;
        }
        let mut out = Map::new();
        for kind in ["core", "package"] {
            let Some(&total) = counts.get(kind) else { continue };
            out.insert(format!("{kind}_throttle_events"), json!(total));
            let rate = s.1.as_ref().and_then(|(t, prev)| {
                let p = *prev.get(kind)?;
                (total >= p).then(|| round((total - p) as f64 / (now - t), 3))
            });
            out.insert(format!("{kind}_throttle_events_per_s"), json!(rate));
        }
        s.1 = Some((now, counts));
        s.2 = Value::Object(out);
        s.2.clone()
    }
}

pub fn live(rates: &Rates, hwmon: &[Value], throttle: &ThrottleCounts) -> Value {
    let times = cpu_times();
    let total = times.iter().find(|(n, _)| n.is_none()).and_then(|(_, t)| breakdown(rates, "cpu/all", t));
    let mut cores = Vec::new();
    let mut freqs = Vec::new();
    for (n, t) in times.iter().filter_map(|(n, t)| n.map(|n| (n, t))) {
        let b = breakdown(rates, &format!("cpu/{n}"), t);
        let mhz = read_int(format!("{CPU_DIR}/cpu{n}/cpufreq/scaling_cur_freq")).map(|k| round(k as f64 / 1000.0, 1));
        if let Some(f) = mhz {
            freqs.push(f);
        }
        let pick = |k: &str| b.as_ref().and_then(|b| b.get(k).cloned()).unwrap_or(Value::Null);
        cores.push(json!({
            "cpu": n,
            "utilization_percent": pick("utilization"),
            "user_percent": pick("user"),
            "system_percent": pick("system"),
            "iowait_percent": pick("iowait"),
            "steal_percent": pick("steal"),
            "frequency_mhz": mhz,
        }));
    }
    let frequency = if freqs.is_empty() {
        Value::Null
    } else {
        let min = freqs.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = freqs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        json!({"min": min, "max": max, "mean": round(freqs.iter().sum::<f64>() / freqs.len() as f64, 1)})
    };
    json!({
        "utilization_percent": total.as_ref().and_then(|t| t.get("utilization").cloned()),
        "times_percent": total,
        "frequency_mhz": frequency,
        "temperature_celsius": temperature(hwmon),
        "thermal_throttle": throttle.get(),
        "cores": cores,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breakdown_is_percent_of_the_interval() {
        let r = Rates::default();
        r.begin(0.0);
        assert!(breakdown(&r, "c", &[10.0, 0.0, 10.0, 100.0, 0.0, 0.0, 0.0, 0.0, 5.0, 0.0]).is_none());
        r.end();
        r.begin(1.0);
        let b = breakdown(&r, "c", &[10.5, 0.0, 10.25, 100.25, 0.0, 0.0, 0.0, 0.0, 5.2, 0.0]).unwrap();
        assert_eq!(b["user"], 50.0);
        assert_eq!(b["system"], 25.0);
        assert_eq!(b["idle"], 25.0);
        assert_eq!(b["utilization"], 75.0);
    }

    #[test]
    fn temperature_is_the_hottest_cpu_sensor() {
        let hw = vec![
            json!({"chip": "mlx5", "kind": "temp", "value": 70.0}),
            json!({"chip": "acpitz", "kind": "temp", "value": 35.8}),
            json!({"chip": "acpitz", "kind": "temp", "value": 41.2}),
            json!({"chip": "acpitz", "kind": "fan", "value": 900.0}),
        ];
        assert_eq!(temperature(&hw), Some(41.2));
        assert_eq!(temperature(&[]), None);
    }
}
