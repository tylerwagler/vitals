//! Where readings come from: one live sampler and one static inventory,
//! shared by every request and stream.
//!
//! Live samples are taken on demand, or by a sampler thread that runs only
//! while a stream is subscribed, at the fastest interval any subscriber asked
//! for. Either way every sample goes through one `Rates`, so each rate is over
//! the interval since the sample before it, and the sample says how long that
//! was (`interval_s`).
//!
//! The static inventory (what the machine is and has) is collected at most
//! once a minute. Its `generation` is a hash of the contents, so it changes
//! only when something does: a card swapped, a link renegotiated, a disk added.

use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::collect::{cpu, gpu, host, memory, network, pci, processes, sensors, storage};
use crate::os;
use crate::rates::Rates;
use crate::sysfs::round;
use crate::VERSION;

pub const LIVE_SECTIONS: &[&str] = &["host", "cpu", "memory", "pressure", "gpus", "sensors", "storage", "network", "rdma"];
pub const STATIC_SECTIONS: &[&str] = &["host", "exporter", "cpu", "memory", "gpus", "pci", "storage", "network", "sensors"];
pub const ENVELOPE: &[&str] = &["hostname", "seq", "time", "interval_s", "duration_s", "static_generation"];
pub const STATIC_ENVELOPE: &[&str] = &["hostname", "generation", "collected_at", "collection_seconds"];
// Readings that move on their own and are not a change of hardware: a GPU's
// PCIe link downtrains when idle. Left out of the static generation.
const VOLATILE_STATIC: &[&str] = &["current_speed", "current_width", "collected_at", "collection_seconds"];
const STATIC_TTL_S: f64 = 60.0;

/// Canonical JSON (sorted keys) without the volatile fields, for hashing.
fn canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().filter(|k| !VOLATILE_STATIC.contains(&k.as_str())).collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String((*k).clone()).to_string());
                out.push(':');
                canonical(&m[*k], out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(x, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

pub fn generation_of(doc: &Value) -> String {
    let mut s = String::new();
    canonical(doc, &mut s);
    sha1_smol::Sha1::from(s.as_bytes()).digest().to_string()[..12].to_string()
}

/// The envelope plus the requested sections, or the whole document.
pub fn pick(doc: &Value, sections: Option<&[String]>, envelope: &[&str]) -> Value {
    let Some(sections) = sections else { return doc.clone() };
    let mut out = Map::new();
    for k in envelope.iter().map(|s| s.to_string()).chain(sections.iter().cloned()) {
        if let Some(v) = doc.get(&k) {
            out.insert(k, v.clone());
        }
    }
    Value::Object(out)
}

#[derive(Default)]
struct Shared {
    latest: Option<Arc<Value>>,
    latest_at: f64,
    seq: u64,
    static_generation: Option<String>,
    subscribers: HashMap<u64, f64>,
    next_id: u64,
    sampling: bool,
    stopping: bool,
    encoded: HashMap<(u64, Option<Vec<String>>), Arc<Vec<u8>>>,
}

/// What a stream subscriber sees when it wakes.
pub struct View {
    pub latest: Option<Arc<Value>>,
    pub seq: u64,
    pub static_generation: Option<String>,
    pub stopping: bool,
}

pub struct Hub {
    pub nv: gpu::Nvml,
    pub clock_cap: gpu::ClockCap,
    systemd: host::Systemd,
    cpu_throttle: cpu::ThrottleCounts,
    hwmon_layout: sensors::HwmonLayout,
    cooling: sensors::CoolingDevices,
    spbm: sensors::Spbm,
    statvfs: storage::Statvfs,
    processes: processes::Processes,
    rates: Rates,
    pub started_at: f64,
    pub listen: Mutex<Option<String>>,
    failed: Mutex<HashSet<String>>,
    sample_lock: Mutex<()>,
    shared: Mutex<Shared>,
    cond: Condvar,
    static_doc: Mutex<Option<(f64, Arc<Value>)>>,
    static_refreshing: AtomicBool,
}

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Hub {
            nv: gpu::Nvml::default(),
            clock_cap: gpu::ClockCap::default(),
            systemd: host::Systemd::default(),
            cpu_throttle: cpu::ThrottleCounts::default(),
            hwmon_layout: sensors::HwmonLayout::new(sensors::HWMON_ROOT),
            cooling: sensors::CoolingDevices::default(),
            spbm: sensors::Spbm::new(sensors::HWMON_ROOT, sensors::SPBM_ACPI_DIR),
            statvfs: storage::Statvfs::default(),
            processes: processes::Processes::default(),
            rates: Rates::default(),
            started_at: os::now(),
            listen: Mutex::new(None),
            failed: Mutex::new(HashSet::new()),
            sample_lock: Mutex::new(()),
            shared: Mutex::new(Shared::default()),
            cond: Condvar::new(),
            static_doc: Mutex::new(None),
            static_refreshing: AtomicBool::new(false),
        })
    }

    // -- sections -----------------------------------------------------------

    /// One section failing is that section's error, not a failed sample.
    fn safe(&self, name: &str, f: impl FnOnce() -> Value) -> Value {
        match catch_unwind(AssertUnwindSafe(f)) {
            Ok(v) => {
                self.failed.lock().unwrap().remove(name);
                v
            }
            Err(panic) => {
                let msg = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "panic".into());
                if self.failed.lock().unwrap().insert(name.to_string()) {
                    eprintln!("ERROR collecting {name} failed: {msg}");
                }
                json!({"error": format!("panic: {msg}")})
            }
        }
    }

    /// A fresh live sample of every section.
    pub fn take(self: &Arc<Self>) -> Arc<Value> {
        let _guard = self.sample_lock.lock().unwrap_or_else(|e| e.into_inner());
        let started = Instant::now();
        let rates = &self.rates;
        rates.begin(os::mono());
        let hw = self.safe("sensors.hwmon", || Value::Array(sensors::hwmon_live(&self.hwmon_layout)));
        let hw_list: Vec<Value> = hw.as_array().cloned().unwrap_or_default();
        let mem = self.safe("memory", || memory::live(rates));
        let total = mem.get("total_bytes").and_then(|v| v.as_i64()).unwrap_or(0);
        let avail = mem.get("available_bytes").and_then(|v| v.as_i64()).unwrap_or(0);
        let generation = self.shared.lock().unwrap().static_generation.clone();
        let mut doc = Map::new();
        doc.insert("hostname".into(), json!(os::hostname()));
        doc.insert("seq".into(), json!(0));
        doc.insert("time".into(), json!(os::now()));
        doc.insert("interval_s".into(), json!(rates.dt().map(|d| round(d, 4))));
        doc.insert("static_generation".into(), json!(generation));
        doc.insert("host".into(), self.safe("host", || host::live(rates, &self.systemd)));
        doc.insert("cpu".into(), self.safe("cpu", || cpu::live(rates, &hw_list, &self.cpu_throttle)));
        doc.insert("memory".into(), mem);
        doc.insert("pressure".into(), self.safe("pressure", || memory::pressure(rates, "/proc/pressure")));
        doc.insert("gpus".into(), self.safe("gpus", || gpu::live(&self.nv, rates, total, avail, &self.clock_cap)));
        doc.insert("sensors".into(), self.safe("sensors", || sensors::live(&hw_list, &self.spbm, &self.cooling)));
        doc.insert("storage".into(), self.safe("storage", || storage::live(rates, &self.statvfs)));
        doc.insert("network".into(), self.safe("network", || network::live(rates)));
        doc.insert("rdma".into(), self.safe("rdma", || network::rdma_live(rates)));
        rates.end();
        if generation.is_none() {
            self.refresh_static_in_background();
        }
        doc.insert("duration_s".into(), json!(round(started.elapsed().as_secs_f64(), 4)));
        let mut s = self.shared.lock().unwrap();
        s.seq += 1;
        doc.insert("seq".into(), json!(s.seq));
        let doc = Arc::new(Value::Object(doc));
        s.latest = Some(doc.clone());
        s.latest_at = os::mono();
        s.encoded.clear();
        self.cond.notify_all();
        doc
    }

    /// The latest sample if it is fresh enough, else a new one. While a stream
    /// runs, its samples are fresh for one of its intervals, so polling
    /// alongside a stream costs nothing and does not shorten its rate intervals.
    pub fn live(self: &Arc<Self>) -> Arc<Value> {
        {
            let s = self.shared.lock().unwrap();
            let mut max_age = 0.25;
            if let Some(min) = s.subscribers.values().cloned().reduce(f64::min) {
                max_age = f64::max(max_age, min * 1.5);
            }
            if let Some(latest) = &s.latest {
                if os::mono() - s.latest_at <= max_age {
                    return latest.clone();
                }
            }
        }
        self.take()
    }

    /// A sample's JSON, encoded once per section set however many streams want it.
    pub fn encoded(&self, doc: &Arc<Value>, sections: Option<&[String]>) -> Arc<Vec<u8>> {
        let seq = doc["seq"].as_u64().unwrap_or(0);
        let key = (seq, sections.map(|s| s.to_vec()));
        let is_latest = |s: &Shared| s.latest.as_ref().is_some_and(|l| Arc::ptr_eq(l, doc));
        {
            let s = self.shared.lock().unwrap();
            if is_latest(&s) {
                if let Some(b) = s.encoded.get(&key) {
                    return b.clone();
                }
            }
        }
        let body = Arc::new(serde_json::to_vec(&pick(doc, sections, ENVELOPE)).unwrap_or_default());
        let mut s = self.shared.lock().unwrap();
        if is_latest(&s) {
            s.encoded.insert(key, body.clone());
        }
        body
    }

    // -- static -------------------------------------------------------------

    pub fn exporter_static(&self) -> Value {
        let archive = std::env::current_exe().ok().map(|p| p.to_string_lossy().into_owned());
        json!({
            "name": "vitals",
            "version": VERSION,
            "implementation": "rust",
            "pid": std::process::id(),
            "user": os::user_name(os::uid()),
            "started_at": self.started_at,
            "listen": *self.listen.lock().unwrap(),
            "binary": archive,
            "nvml_ready": self.nv.ready(),
            "nvml_error": self.nv.error(),
            "live_sections": LIVE_SECTIONS,
            "static_sections": STATIC_SECTIONS,
        })
    }

    fn collect_static(&self) -> Value {
        let started = Instant::now();
        let mem = self.safe("static.memory", memory::static_);
        let host_total = mem.get("total_bytes").and_then(|v| v.as_i64());
        // gpus before exporter: the exporter reports how NVML fared
        let gpus = self.safe("static.gpus", || gpu::static_(&self.nv, host_total));
        let mut sections = Map::new();
        sections.insert("host".into(), self.safe("static.host", host::static_));
        sections.insert("exporter".into(), self.exporter_static());
        sections.insert("cpu".into(), self.safe("static.cpu", cpu::static_));
        sections.insert("memory".into(), mem);
        sections.insert("gpus".into(), gpus);
        sections.insert("pci".into(), self.safe("static.pci", pci::static_));
        sections.insert("storage".into(), self.safe("static.storage", storage::static_));
        sections.insert("network".into(), self.safe("static.network", network::static_));
        sections.insert("sensors".into(), self.safe("static.sensors", sensors::static_));
        let sections = Value::Object(sections);
        let generation = generation_of(&sections);
        let mut doc = Map::new();
        doc.insert("hostname".into(), json!(os::hostname()));
        doc.insert("generation".into(), json!(generation));
        doc.insert("collected_at".into(), json!(os::now()));
        doc.insert("collection_seconds".into(), json!(round(started.elapsed().as_secs_f64(), 4)));
        if let Value::Object(m) = sections {
            doc.extend(m);
        }
        Value::Object(doc)
    }

    pub fn static_doc(&self, refresh: bool) -> Arc<Value> {
        let mut st = self.static_doc.lock().unwrap_or_else(|e| e.into_inner());
        let now = os::mono();
        let stale = match &*st {
            None => true,
            Some((at, _)) => now - at > STATIC_TTL_S,
        };
        if refresh || stale {
            let doc = Arc::new(self.collect_static());
            let generation = doc["generation"].as_str().unwrap_or("").to_string();
            *st = Some((os::mono(), doc));
            let mut s = self.shared.lock().unwrap();
            if s.static_generation.as_deref() != Some(&generation) {
                if s.static_generation.is_some() {
                    eprintln!("INFO static inventory changed: generation {generation}");
                }
                s.static_generation = Some(generation);
                self.cond.notify_all();
            }
        }
        st.as_ref().unwrap().1.clone()
    }

    fn refresh_static_in_background(self: &Arc<Self>) {
        // Not under the static lock: a collection in progress holds it, and
        // the sampler must not wait a whole collection to find that out.
        let fresh = self.static_doc.try_lock().ok().is_some_and(|d| d.as_ref().is_some_and(|(at, _)| os::mono() - at <= STATIC_TTL_S));
        if fresh || self.static_refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let hub = self.clone();
        let spawned = std::thread::Builder::new().name("vitals-static".into()).spawn(move || {
            hub.static_doc(false);
            hub.static_refreshing.store(false, Ordering::Release);
        });
        if spawned.is_err() {
            self.static_refreshing.store(false, Ordering::Release);
        }
    }

    // -- processes ----------------------------------------------------------

    pub fn process_table(&self, top: usize, sort: &str) -> Value {
        let table = self.processes.table(top, sort, &gpu::process_memory(&self.nv));
        let mut out = Map::new();
        out.insert("hostname".into(), json!(os::hostname()));
        out.insert("time".into(), json!(os::now()));
        if let Value::Object(m) = table {
            out.extend(m);
        }
        Value::Object(out)
    }

    // -- streams ------------------------------------------------------------

    pub fn subscribe(self: &Arc<Self>, interval: f64) -> u64 {
        let mut s = self.shared.lock().unwrap();
        s.next_id += 1;
        let id = s.next_id;
        s.subscribers.insert(id, interval);
        if !s.sampling {
            s.sampling = true;
            let hub = self.clone();
            std::thread::Builder::new().name("vitals-sampler".into()).spawn(move || hub.run()).ok();
        }
        self.cond.notify_all();
        id
    }

    pub fn unsubscribe(&self, id: u64) {
        let mut s = self.shared.lock().unwrap();
        s.subscribers.remove(&id);
        self.cond.notify_all();
    }

    pub fn sampling(&self) -> bool {
        let s = self.shared.lock().unwrap();
        s.sampling && !s.subscribers.is_empty()
    }

    pub fn seq(&self) -> u64 {
        self.shared.lock().unwrap().seq
    }

    pub fn static_generation(&self) -> Option<String> {
        self.shared.lock().unwrap().static_generation.clone()
    }

    /// Sample at the fastest subscribed interval while anyone listens.
    fn run(self: Arc<Self>) {
        loop {
            {
                let mut s = self.shared.lock().unwrap();
                if s.subscribers.is_empty() || s.stopping {
                    s.sampling = false;
                    return;
                }
            }
            let tick = os::mono();
            self.take();
            self.refresh_static_in_background();
            let mut s = self.shared.lock().unwrap();
            while !s.subscribers.is_empty() && !s.stopping {
                let interval = s.subscribers.values().cloned().fold(f64::INFINITY, f64::min);
                let remaining = tick + interval - os::mono();
                if remaining <= 0.0 {
                    break;
                }
                s = self.cond.wait_timeout(s, Duration::from_secs_f64(remaining)).unwrap().0;
            }
        }
    }

    /// Wait until there is something for a subscriber, or `until` passes.
    /// `want(view)` says whether what is there now is worth waking for.
    pub fn wait(&self, until: f64, want: impl Fn(&View) -> Option<f64>) -> View {
        let mut s = self.shared.lock().unwrap();
        loop {
            let view = View {
                latest: s.latest.clone(),
                seq: s.seq,
                static_generation: s.static_generation.clone(),
                stopping: s.stopping,
            };
            if view.stopping {
                return view;
            }
            // want returns None when it is satisfied, else the time it wants
            // to be woken at regardless.
            let wake_at = match want(&view) {
                None => return view,
                Some(t) => t.min(until),
            };
            let remaining = wake_at - os::mono();
            if remaining <= 0.0 {
                return view;
            }
            s = self.cond.wait_timeout(s, Duration::from_secs_f64(remaining)).unwrap().0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_ignores_a_downtrained_link() {
        let a = json!({"pci": {"devices": [{"address": "0000:01:00.0", "link": {"current_speed": "2.5 GT/s", "max_speed": "16 GT/s"}}]}});
        let b = json!({"pci": {"devices": [{"address": "0000:01:00.0", "link": {"current_speed": "16 GT/s", "max_speed": "16 GT/s"}}]}});
        let c = json!({"pci": {"devices": [{"address": "0000:01:00.0", "link": {"current_speed": "16 GT/s", "max_speed": "8 GT/s"}}]}});
        assert_eq!(generation_of(&a), generation_of(&b));
        assert_ne!(generation_of(&b), generation_of(&c));
    }
}
