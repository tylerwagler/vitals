//! Every hwmon sensor (temperatures, fans, voltages, currents, power, energy),
//! thermal zones, cooling devices, and GB10's spbm power rails.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Map, Value};

use crate::os;
use crate::sysfs::{basename, exists, link_name, list, list_natural, natural_key, read_int, read_text, realpath, round, scaled};

pub const HWMON_ROOT: &str = "/sys/class/hwmon";
pub const THERMAL_ROOT: &str = "/sys/class/thermal";
pub const SPBM_ACPI_DIR: &str = "/sys/bus/platform/devices/NVDA8800:00";

const KINDS: &[&str] = &["temp", "fan", "in", "curr", "power", "energy", "humidity", "freq"];
const THRESHOLDS: &[&str] = &["min", "max", "lcrit", "crit", "emergency", "crit_hyst", "max_hyst", "cap"];

/// kind -> (unit, scale from the sysfs integer)
fn unit(kind: &str) -> (&'static str, f64) {
    match kind {
        "temp" => ("celsius", 1e-3),
        "fan" => ("rpm", 1.0),
        "in" => ("volts", 1e-3),
        "curr" => ("amps", 1e-3),
        "power" => ("watts", 1e-6),
        "energy" => ("joules", 1e-6),
        "humidity" => ("percent", 1e-3),
        _ => ("mhz", 1e-6),
    }
}

/// 'temp12_input' -> ('temp', 'temp12', 'input'), for the kinds hwmon has.
fn sensor_file(name: &str) -> Option<(&'static str, String, &'static str)> {
    let (stem, suffix) = if let Some(s) = name.strip_suffix("_input") {
        (s, "input")
    } else {
        let s = name.strip_suffix("_average")?;
        (s, "average")
    };
    for kind in KINDS {
        if let Some(n) = stem.strip_prefix(kind) {
            if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) {
                return Some((kind, stem.to_string(), suffix));
            }
        }
    }
    None
}

/// (kind, channel prefix, value file) per sensor, preferring _input over _average.
fn channels(hw: &Path) -> Vec<(&'static str, String, PathBuf)> {
    let mut found: BTreeMap<(String, String), (&'static str, PathBuf, &'static str)> = BTreeMap::new();
    for path in list(hw, |n| n.ends_with("_input") || n.ends_with("_average")) {
        let Some((kind, prefix, suffix)) = sensor_file(&basename(&path)) else { continue };
        let key = (kind.to_string(), prefix);
        let keep = match found.get(&key) {
            None => true,
            Some(_) => suffix == "input",
        };
        if keep {
            found.insert(key, (kind, path, suffix));
        }
    }
    let mut out: Vec<_> = found.into_iter().map(|((_, prefix), (kind, path, _))| (kind, prefix, path)).collect();
    out.sort_by(|a, b| a.0.cmp(b.0).then_with(|| natural_key(&a.1).cmp(&natural_key(&b.1))));
    out
}

pub fn hwmon_chips(root: &str) -> Vec<PathBuf> {
    list_natural(root, |n| n.starts_with("hwmon"))
}

type Channel = (&'static str, PathBuf, String, Option<PathBuf>);

/// Which sensors each chip has, their labels and alarm files, re-read once
/// a minute. A sample then opens only the value files: labels are half the
/// files and never change.
pub struct HwmonLayout {
    root: String,
    state: Mutex<(f64, Vec<(String, Option<String>, Vec<Channel>)>)>,
}

impl HwmonLayout {
    const TTL_S: f64 = 60.0;

    pub fn new(root: &str) -> Self {
        HwmonLayout { root: root.to_string(), state: Mutex::new((f64::NEG_INFINITY, Vec::new())) }
    }

    fn get(&self) -> Vec<(String, Option<String>, Vec<Channel>)> {
        let mut s = self.state.lock().unwrap();
        let now = os::mono();
        if now - s.0 >= Self::TTL_S {
            s.0 = now;
            s.1 = hwmon_chips(&self.root)
                .iter()
                .map(|hw| {
                    let sensors = channels(hw)
                        .into_iter()
                        .map(|(kind, prefix, path)| {
                            let alarm = hw.join(format!("{prefix}_alarm"));
                            let label = read_text(hw.join(format!("{prefix}_label"))).filter(|l| !l.is_empty()).unwrap_or(prefix);
                            (kind, path, label, exists(&alarm).then_some(alarm))
                        })
                        .collect();
                    (basename(hw), read_text(hw.join("name")), sensors)
                })
                .collect();
        }
        s.1.clone()
    }
}

pub fn hwmon_live(layout: &HwmonLayout) -> Vec<Value> {
    let mut out = Vec::new();
    for (name, chip, sensors) in layout.get() {
        for (kind, path, label, alarm) in sensors {
            let (unit, scale) = unit(kind);
            let mut row = Map::new();
            row.insert("hwmon".into(), json!(name));
            row.insert("chip".into(), json!(chip));
            row.insert("sensor".into(), json!(label));
            row.insert("kind".into(), json!(kind));
            row.insert("value".into(), json!(scaled(&path, scale, 3)));
            row.insert("unit".into(), json!(unit));
            if let Some(a) = alarm.and_then(read_int) {
                row.insert("alarm".into(), json!(a != 0));
            }
            out.push(Value::Object(row));
        }
    }
    out
}

pub fn hwmon_static(root: &str) -> Value {
    Value::Array(
        hwmon_chips(root)
            .iter()
            .map(|hw| {
                let device = exists(hw.join("device")).then(|| realpath(hw.join("device"))).flatten();
                let sensors: Vec<Value> = channels(hw)
                    .into_iter()
                    .map(|(kind, prefix, _)| {
                        let (unit, scale) = unit(kind);
                        let mut limits = Map::new();
                        for t in THRESHOLDS {
                            if let Some(v) = scaled(hw.join(format!("{prefix}_{t}")), scale, 3) {
                                limits.insert(t.to_string(), json!(v));
                            }
                        }
                        let label = read_text(hw.join(format!("{prefix}_label"))).filter(|l| !l.is_empty()).unwrap_or(prefix);
                        let mut row = Map::new();
                        row.insert("sensor".into(), json!(label));
                        row.insert("kind".into(), json!(kind));
                        row.insert("unit".into(), json!(unit));
                        if !limits.is_empty() {
                            row.insert("limits".into(), Value::Object(limits));
                        }
                        Value::Object(row)
                    })
                    .collect();
                json!({
                    "hwmon": basename(hw), "chip": read_text(hw.join("name")),
                    "device": device.map(basename), "driver": link_name(hw.join("device/driver")),
                    "sensors": sensors,
                })
            })
            .collect(),
    )
}

fn zones(root: &str) -> Vec<PathBuf> {
    list_natural(root, |n| n.starts_with("thermal_zone"))
}

pub fn thermal_zones(root: &str) -> Value {
    Value::Array(
        zones(root)
            .iter()
            .map(|z| json!({"zone": basename(z), "type": read_text(z.join("type")), "celsius": scaled(z.join("temp"), 1e-3, 2)}))
            .collect(),
    )
}

pub fn thermal_static(root: &str) -> Value {
    Value::Array(
        zones(root)
            .iter()
            .map(|z| {
                let trips: Vec<Value> = list_natural(z, |n| n.starts_with("trip_point_") && n.ends_with("_temp"))
                    .iter()
                    .map(|t| {
                        let base = t.to_string_lossy();
                        let base = &base[..base.len() - "_temp".len()];
                        json!({"type": read_text(format!("{base}_type")), "celsius": scaled(t, 1e-3, 2)})
                    })
                    .collect();
                json!({"zone": basename(z), "type": read_text(z.join("type")), "policy": read_text(z.join("policy")), "trip_points": trips})
            })
            .collect(),
    )
}

pub fn cooling_devices(root: &str) -> Value {
    Value::Array(
        list_natural(root, |n| n.starts_with("cooling_device"))
            .iter()
            .map(|d| json!({"device": basename(d), "type": read_text(d.join("type")),
                            "state": read_int(d.join("cur_state")), "max_state": read_int(d.join("max_state"))}))
            .collect(),
    )
}

/// Cooling device states every 10 s: on ACPI hosts each cur_state read can be
/// a firmware call, and a host has one per CPU.
pub struct CoolingDevices {
    state: Mutex<(f64, Value)>,
}

impl Default for CoolingDevices {
    fn default() -> Self {
        CoolingDevices { state: Mutex::new((f64::NEG_INFINITY, Value::Array(vec![]))) }
    }
}

impl CoolingDevices {
    pub fn get(&self) -> Value {
        let mut s = self.state.lock().unwrap();
        let now = os::mono();
        if now - s.0 >= 10.0 {
            *s = (now, cooling_devices(THERMAL_ROOT));
        }
        s.1.clone()
    }
}

pub fn find_hwmon(name: &str, root: &str) -> Option<PathBuf> {
    list(root, |n| n.starts_with("hwmon")).into_iter().find(|p| read_text(p.join("name")).unwrap_or_default().starts_with(name))
}

/// GB10 power rails from the spark_hwmon driver, every channel it labels.
pub struct Spbm {
    root: String,
    acpi_dir: String,
    peak_gpu_w: Mutex<Option<f64>>,
}

impl Spbm {
    pub fn new(root: &str, acpi_dir: &str) -> Self {
        Spbm { root: root.into(), acpi_dir: acpi_dir.into(), peak_gpu_w: Mutex::new(None) }
    }

    fn status(&self, hw: &Path, name: &str) -> PathBuf {
        let p = hw.join(name);
        if exists(&p) { p } else { Path::new(&self.acpi_dir).join(name) }
    }

    pub fn collect(&self) -> Value {
        let Some(hw) = find_hwmon("spbm", &self.root) else { return Value::Null };
        let mut power: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
        for label_path in list(&hw, |n| n.starts_with("power") && n.ends_with("_label")) {
            let Some(label) = read_text(&label_path).filter(|l| !l.is_empty()) else { continue };
            let base = label_path.to_string_lossy();
            let base = &base[..base.len() - "_label".len()];
            let mut ch = Map::new();
            for k in ["input", "cap", "max", "min"] {
                if let Some(v) = scaled(format!("{base}_{k}"), 1e-6, 3) {
                    ch.insert(k.to_string(), json!(v));
                }
            }
            power.insert(label, ch);
        }
        let labelled = |prefix: &str, scale: f64| {
            let mut out = Map::new();
            for label_path in list(&hw, |n| n.starts_with(prefix) && n.ends_with("_label")) {
                let base = label_path.to_string_lossy();
                let base = &base[..base.len() - "_label".len()];
                if let (Some(label), Some(v)) = (read_text(&label_path).filter(|l| !l.is_empty()), scaled(format!("{base}_input"), scale, 3)) {
                    out.insert(label, json!({"input": v}));
                }
            }
            out
        };
        let gpu_w = power.get("gpu").and_then(|c| c.get("input")).and_then(|v| v.as_f64());
        let peak = {
            let mut p = self.peak_gpu_w.lock().unwrap();
            if let Some(w) = gpu_w {
                if p.is_none_or(|x| w > x) {
                    *p = Some(w);
                }
            }
            *p
        };
        let mut cap_exceeded = Map::new();
        for (label, ch) in &power {
            if let (Some(i), Some(c)) = (ch.get("input").and_then(|v| v.as_f64()), ch.get("cap").and_then(|v| v.as_f64())) {
                cap_exceeded.insert(label.clone(), json!(i > c));
            }
        }
        let tj = read_int(self.status(&hw, "tj_max_c"));
        json!({
            "hwmon": hw.to_string_lossy(),
            "power_watts": power,
            "temperatures_celsius": labelled("temp", 1e-3),
            "energy_joules": labelled("energy", 1e-6),
            "peak_gpu_watts": peak,
            "cap_exceeded": cap_exceeded,
            // decidegrees of rise over the idle point, per spbm.c
            "tj_rise_celsius": tj.map(|t| round(t as f64 * 0.1, 1)),
            "prochot": read_int(self.status(&hw, "prochot")),
            "pl_level": read_int(self.status(&hw, "pl_level")),
        })
    }
}

pub fn static_() -> Value {
    json!({"hwmon": hwmon_static(HWMON_ROOT), "thermal_zones": thermal_static(THERMAL_ROOT)})
}

pub fn live(hwmon: &[Value], spbm: &Spbm, cooling: &CoolingDevices) -> Value {
    let hottest = hwmon
        .iter()
        .filter(|s| s["kind"] == "temp")
        .filter_map(|s| s["value"].as_f64())
        .reduce(f64::max);
    json!({
        "hottest_celsius": hottest,
        "hwmon": hwmon,
        "thermal_zones": thermal_zones(THERMAL_ROOT),
        "cooling_devices": cooling.get(),
        "spbm": spbm.collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vitals-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn hwmon_reads_every_kind_with_units_and_limits() {
        let root = tmp("hwmon");
        let hw = root.join("hwmon3");
        fs::create_dir_all(&hw).unwrap();
        for (f, v) in [("name", "nct6798"), ("temp1_input", "45500"), ("temp1_label", "CPUTIN"), ("temp1_crit", "100000"),
                       ("fan2_input", "1200"), ("fan2_alarm", "1"), ("in0_input", "1104"), ("power1_average", "12500000"),
                       ("curr1_input", "2500")] {
            fs::write(hw.join(f), format!("{v}\n")).unwrap();
        }
        let live = hwmon_live(&HwmonLayout::new(root.to_str().unwrap()));
        let find = |kind: &str, sensor: &str| live.iter().find(|s| s["kind"] == kind && s["sensor"] == sensor).cloned().unwrap();
        assert_eq!(find("temp", "CPUTIN")["value"], 45.5);
        assert_eq!(find("fan", "fan2"), json!({"hwmon": "hwmon3", "chip": "nct6798", "sensor": "fan2", "kind": "fan",
                                                "value": 1200.0, "unit": "rpm", "alarm": true}));
        assert_eq!(find("in", "in0")["value"], 1.104);
        assert_eq!(find("power", "power1")["value"], 12.5);
        assert_eq!(find("curr", "curr1")["value"], 2.5);
        let chips = hwmon_static(root.to_str().unwrap());
        let temp = chips[0]["sensors"].as_array().unwrap().iter().find(|s| s["kind"] == "temp").unwrap().clone();
        assert_eq!(temp["limits"], json!({"crit": 100.0}));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn spbm_status_falls_back_to_the_acpi_device() {
        let root = tmp("spbm");
        let hw = root.join("hwmon/hwmon6");
        fs::create_dir_all(&hw).unwrap();
        fs::write(hw.join("name"), "spbm\n").unwrap();
        fs::write(hw.join("power1_label"), "pl1\n").unwrap();
        fs::write(hw.join("power1_input"), "18762000\n").unwrap();
        fs::write(hw.join("power1_cap"), "140000000\n").unwrap();
        fs::write(hw.join("prochot"), "1\n").unwrap();
        let acpi = root.join("acpi");
        fs::create_dir_all(&acpi).unwrap();
        fs::write(acpi.join("tj_max_c"), "36\n").unwrap();
        let out = Spbm::new(root.join("hwmon").to_str().unwrap(), acpi.to_str().unwrap()).collect();
        assert_eq!(out["power_watts"]["pl1"], json!({"input": 18.762, "cap": 140.0}));
        assert_eq!(out["cap_exceeded"], json!({"pl1": false}));
        assert_eq!(out["prochot"], 1);
        assert_eq!(out["pl_level"], Value::Null);
        assert_eq!(out["tj_rise_celsius"], 3.6);
        let _ = fs::remove_dir_all(&root);
    }
}
