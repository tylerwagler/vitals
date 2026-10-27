//! The process table, on request: the top N by CPU, memory or GPU memory.
//!
//! Names and owners only, never command lines: those can carry secrets (an
//! --api-key, a token in a URL), and this is served to the whole LAN.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

use crate::os;
use crate::sysfs::{list, read_text, round};

pub const SORTS: &[&str] = &["cpu", "memory", "gpu"];
// A first table has no CPU interval to measure over; it waits this long for one.
const PRIME_S: f64 = 0.25;
// Past this the previous CPU times are stale, so the table re-primes.
const STALE_S: f64 = 60.0;

/// psutil's names for /proc/<pid>/stat's state letter.
fn status(c: char) -> &'static str {
    match c {
        'R' => "running", 'S' => "sleeping", 'D' => "disk-sleep", 'T' => "stopped",
        't' => "tracing-stop", 'Z' => "zombie", 'X' | 'x' => "dead", 'K' => "wake-kill",
        'W' => "waking", 'I' => "idle", 'P' => "parked", _ => "unknown",
    }
}

pub struct Proc {
    pub pid: i64,
    pub ppid: i64,
    pub name: String,
    pub state: char,
    pub cpu_s: f64,
    pub nice: i64,
    pub threads: i64,
    pub start_ticks: i64,
    pub rss: i64,
    pub uid: Option<u32>,
}

/// One /proc/<pid>/stat, parsed (the comm may hold spaces and parentheses).
pub fn read_proc(pid: i64) -> Option<Proc> {
    let stat = read_text(format!("/proc/{pid}/stat"))?;
    let (open, close) = (stat.find('(')?, stat.rfind(')')?);
    let mut name = stat[open + 1..close].to_string();
    let f: Vec<&str> = stat[close + 2..].split_whitespace().collect();
    if f.len() < 22 {
        return None;
    }
    let n = |i: usize| f[i].parse::<i64>().unwrap_or(0);
    let hz = os::clock_ticks();
    // The kernel keeps 15 characters of a name; psutil then takes the
    // executable's from the command line when it starts the same way.
    if name.len() >= 15 {
        if let Ok(cmd) = std::fs::read(format!("/proc/{pid}/cmdline")) {
            if let Some(first) = cmd.split(|&b| b == 0).next() {
                let exe = String::from_utf8_lossy(first);
                let base = exe.rsplit('/').next().unwrap_or("");
                if base.starts_with(&name) {
                    name = base.to_string();
                }
            }
        }
    }
    let uid = read_text(format!("/proc/{pid}/status")).and_then(|s| {
        s.lines().find_map(|l| l.strip_prefix("Uid:").and_then(|r| r.split_whitespace().next()?.parse().ok()))
    });
    Some(Proc {
        pid,
        ppid: n(1),
        name,
        state: f[0].chars().next().unwrap_or('?'),
        cpu_s: (n(11) + n(12)) as f64 / hz,
        nice: n(16),
        threads: n(17),
        start_ticks: n(19),
        rss: n(21) * os::page_size(),
        uid,
    })
}

pub fn pids() -> Vec<i64> {
    list("/proc", |n| n.bytes().all(|b| b.is_ascii_digit()))
        .iter()
        .filter_map(|p| crate::sysfs::basename(p).parse().ok())
        .collect()
}

#[derive(Default)]
pub struct Processes {
    // (pid, start ticks) -> cumulative CPU seconds at the last table
    state: Mutex<(f64, HashMap<(i64, i64), f64>)>,
}

impl Processes {
    fn measure() -> Vec<Proc> {
        pids().into_iter().filter_map(read_proc).collect()
    }

    pub fn table(&self, top: usize, sort: &str, gpu_mem: &HashMap<i64, Vec<(i64, Option<i64>)>>) -> Value {
        let mut s = self.state.lock().unwrap();
        let mut now = os::mono();
        if s.1.is_empty() || now - s.0 > STALE_S {
            s.1 = Self::measure().iter().map(|p| ((p.pid, p.start_ticks), p.cpu_s)).collect();
            s.0 = now;
            std::thread::sleep(Duration::from_secs_f64(PRIME_S));
            now = os::mono();
        }
        let interval = now - s.0;
        let procs = Self::measure();
        let mut cpu = HashMap::new();
        for p in &procs {
            let key = (p.pid, p.start_ticks);
            // psutil's cpu_percent: of one CPU, so a busy multithreaded
            // process can pass 100. A process new since the last table has 0.
            let pct = match s.1.get(&key) {
                Some(prev) if interval > 0.0 => round(100.0 * (p.cpu_s - prev).max(0.0) / interval, 1),
                _ => 0.0,
            };
            cpu.insert(key, pct);
        }
        s.1 = procs.iter().map(|p| ((p.pid, p.start_ticks), p.cpu_s)).collect();
        s.0 = now;
        drop(s);

        let total_mem = crate::collect::memory::meminfo(None).get("mem_total_bytes").and_then(|v| v.as_i64()).unwrap_or(0);
        let boot = crate::collect::host::boot_time().unwrap_or(0.0);
        let hz = os::clock_ticks();
        let mut states: BTreeMap<&str, i64> = BTreeMap::new();
        let mut threads = 0;
        let mut rows: Vec<Value> = procs
            .iter()
            .map(|p| {
                *states.entry(status(p.state)).or_default() += 1;
                threads += p.threads;
                let gpus = gpu_mem.get(&p.pid);
                json!({
                    "pid": p.pid, "ppid": p.ppid, "name": p.name,
                    "user": p.uid.map(os::user_name), "state": status(p.state), "nice": p.nice,
                    "threads": p.threads, "started_at": boot + p.start_ticks as f64 / hz,
                    "cpu_percent": cpu.get(&(p.pid, p.start_ticks)),
                    "memory_rss_bytes": p.rss,
                    "memory_percent": if total_mem > 0 { json!(round(100.0 * p.rss as f64 / total_mem as f64, 3)) } else { Value::Null },
                    "gpu_memory_bytes": gpus.map(|g| g.iter().map(|(_, b)| b.unwrap_or(0)).sum::<i64>()),
                    "gpus": gpus.map(|g| g.iter().map(|(i, _)| *i).collect::<Vec<_>>()).unwrap_or_default(),
                })
            })
            .collect();
        let field = match sort {
            "memory" => "memory_rss_bytes",
            "gpu" => "gpu_memory_bytes",
            _ => "cpu_percent",
        };
        rows.sort_by(|a, b| {
            let (x, y) = (a[field].as_f64().unwrap_or(0.0), b[field].as_f64().unwrap_or(0.0));
            y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal)
        });
        let count = rows.len();
        rows.truncate(top);
        json!({
            "interval_s": round(interval, 3),
            "sort": sort,
            "totals": {"processes": count, "threads": threads, "by_state": states},
            "processes": rows,
        })
    }
}
