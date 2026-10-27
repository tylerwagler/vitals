//! The machine itself: what it is (bare metal, VM, LXC), its OS, kernel and
//! firmware, and its load, processes and systemd state.

use std::sync::Mutex;

use serde_json::{json, Map, Value};

use crate::os;
use crate::rates::Rates;
use crate::sysfs::{exists, list, parse_kv, read_int, read_text, round, run, run_any};

const DMI_DIR: &str = "/sys/class/dmi/id";
// World-readable DMI fields; the serials and UUID are root-only.
const DMI_FIELDS: &[&str] = &[
    "sys_vendor", "product_name", "product_version", "product_family", "product_sku",
    "board_vendor", "board_name", "board_version",
    "bios_vendor", "bios_version", "bios_date", "bios_release",
    "chassis_vendor", "chassis_version",
];
// Words in DMI or the device tree that only a hypervisor writes.
const VM_VENDORS: &[(&str, &str)] = &[
    ("qemu", "qemu"), ("kvm", "kvm"), ("vmware", "vmware"), ("virtualbox", "oracle"),
    ("innotek", "oracle"), ("xen", "xen"), ("bochs", "bochs"), ("parallels", "parallels"),
    ("amazon ec2", "amazon"), ("google compute engine", "google"),
    ("microsoft corporation virtual machine", "microsoft"), ("linux,dummy-virt", "qemu"),
];

fn chassis_type(n: i64) -> String {
    match n {
        1 => "other", 2 => "unknown", 3 => "desktop", 4 => "low-profile desktop", 6 => "mini tower",
        7 => "tower", 8 => "portable", 9 => "laptop", 10 => "notebook", 13 => "all-in-one",
        17 => "main server chassis", 23 => "rack mount chassis", 24 => "sealed-case pc",
        28 => "blade", 30 => "tablet", 31 => "convertible", 35 => "mini pc", 36 => "stick pc",
        _ => return n.to_string(),
    }
    .to_string()
}

// -- machine type -----------------------------------------------------------

/// Proxmox (and any LXD/LXC) container: lxcfs serves its /proc files.
fn lxcfs_mounted() -> bool {
    read_text("/proc/mounts").unwrap_or_default().lines().any(|l| l.starts_with("lxcfs ") && l.contains(" /proc/"))
}

pub fn detect_container() -> Option<String> {
    // LXC's own marks first: an OCI image run as a Proxmox app container
    // carries the /run/systemd/container its build left ("docker"), which
    // systemd-detect-virt then repeats.
    if exists("/dev/.lxc-boot-id") || lxcfs_mounted() {
        return Some("lxc".into());
    }
    let answer = run_any(&["systemd-detect-virt", "--container"]).unwrap_or_default();
    let answer = answer.trim();
    if !answer.is_empty() && answer != "none" {
        return Some(answer.to_string());
    }
    // systemd-detect-virt reads /proc/1/environ, which an unprivileged
    // exporter cannot, and may then say "none" inside a container.
    if let Some(marker) = read_text("/run/systemd/container").filter(|m| !m.is_empty()) {
        return Some(marker);
    }
    if exists("/run/.containerenv") {
        return Some("podman".into());
    }
    if exists("/.dockerenv") {
        return Some("docker".into());
    }
    None
}

pub fn detect_hypervisor() -> Option<String> {
    let answer = run_any(&["systemd-detect-virt", "--vm"]).unwrap_or_default();
    let answer = answer.trim();
    if !answer.is_empty() && answer != "none" {
        return Some(answer.to_string());
    }
    if answer == "none" {
        return None;
    }
    if let Some(hv) = read_text("/sys/hypervisor/type").filter(|h| !h.is_empty()) {
        return Some(hv);
    }
    let words = [
        read_text(format!("{DMI_DIR}/sys_vendor")),
        read_text(format!("{DMI_DIR}/product_name")),
        read_text("/sys/firmware/devicetree/base/compatible"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
    .to_lowercase()
    .replace('\0', " ");
    for (needle, name) in VM_VENDORS {
        if words.contains(needle) {
            return Some(name.to_string());
        }
    }
    let cpuinfo = read_text("/proc/cpuinfo").unwrap_or_default();
    if cpuinfo.lines().any(|l| l.starts_with("flags") && l.contains(" hypervisor")) {
        return Some("unknown".into());
    }
    None
}

/// bare-metal, vm, lxc (or another container technology), and what runs it.
/// A container in a VM is reported as the container, with the hypervisor
/// alongside: the container is what the readings describe.
pub fn machine_type() -> Value {
    let (container, hypervisor) = (detect_container(), detect_hypervisor());
    let kind = match (&container, &hypervisor) {
        (Some(c), _) if c == "lxc" || c == "lxc-libvirt" => "lxc",
        (Some(_), _) => "container",
        (None, Some(_)) => "vm",
        (None, None) => "bare-metal",
    };
    json!({"type": kind, "container": container, "hypervisor": hypervisor})
}

// -- static -----------------------------------------------------------------

pub fn os_release() -> Value {
    let text = read_text("/etc/os-release").or_else(|| read_text("/usr/lib/os-release"));
    let info = parse_kv(text.as_deref());
    let get = |k: &str| info.get(k).cloned().unwrap_or(Value::Null);
    let name = info.get("PRETTY_NAME").or(info.get("NAME")).cloned().unwrap_or(Value::Null);
    let mut out = Map::new();
    out.insert("name".into(), name);
    out.insert("id".into(), get("ID"));
    out.insert("version_id".into(), get("VERSION_ID"));
    out.insert("codename".into(), get("VERSION_CODENAME"));
    let dgx = parse_kv(read_text("/etc/dgx-release").as_deref());
    if !dgx.is_empty() {
        out.insert("dgx".into(), Value::Object(dgx));
    }
    if let Some(pve) = run(&["pveversion"]) {
        out.insert("proxmox".into(), json!(pve.trim()));
    }
    Value::Object(out)
}

pub fn dmi() -> Value {
    let mut out = Map::new();
    for k in DMI_FIELDS {
        if let Some(v) = read_text(format!("{DMI_DIR}/{k}")).filter(|v| !v.is_empty()) {
            out.insert(k.to_string(), json!(v));
        }
    }
    if let Some(n) = read_int(format!("{DMI_DIR}/chassis_type")) {
        out.insert("chassis_type".into(), json!(chassis_type(n)));
    }
    if let Some(model) = read_text("/sys/firmware/devicetree/base/model").filter(|m| !m.is_empty()) {
        out.insert("devicetree_model".into(), json!(model.trim_matches('\0')));
    }
    if out.is_empty() { Value::Null } else { Value::Object(out) }
}

fn local_timezone() -> Option<String> {
    match std::fs::read_link("/etc/localtime") {
        Ok(target) => {
            let t = target.to_string_lossy().into_owned();
            Some(t.split_once("zoneinfo/").map(|(_, z)| z.to_string()).unwrap_or(t))
        }
        Err(_) => read_text("/etc/timezone"),
    }
}

pub fn boot_time() -> Option<f64> {
    read_text("/proc/stat")?
        .lines()
        .find_map(|l| l.strip_prefix("btime ").and_then(|v| v.trim().parse::<f64>().ok()))
}

pub fn static_() -> Value {
    let u = os::uname();
    let boot = boot_time();
    json!({
        "hostname": os::hostname(),
        "machine": machine_type(),
        "os": os_release(),
        "kernel": {
            "release": u.release,
            "version": u.version,
            "arch": u.machine,
            "cmdline": read_text("/proc/cmdline"),
            "tainted": read_int("/proc/sys/kernel/tainted"),
        },
        "firmware": dmi(),
        "boot_time": boot,
        "boot_time_iso": boot.map(os::iso_utc),
        "timezone": local_timezone(),
    })
}

// -- live -------------------------------------------------------------------

/// System state and failed units, at most every 10 s (two subprocesses).
pub struct Systemd {
    state: Mutex<(f64, Value)>,
}

impl Default for Systemd {
    fn default() -> Self {
        Systemd { state: Mutex::new((f64::NEG_INFINITY, Value::Null)) }
    }
}

impl Systemd {
    const TTL_S: f64 = 10.0;

    pub fn get(&self) -> Value {
        let mut s = self.state.lock().unwrap();
        let now = os::mono();
        if now - s.0 >= Self::TTL_S {
            *s = (now, Self::probe());
        }
        s.1.clone()
    }

    fn probe() -> Value {
        let state = run_any(&["systemctl", "is-system-running"]).unwrap_or_default();
        let state = state.trim();
        if state.is_empty() {
            return Value::Null;
        }
        let failed = run(&["systemctl", "list-units", "--state=failed", "--plain", "--no-legend", "--no-pager"])
            .unwrap_or_default();
        let synced = run(&["timedatectl", "show", "-p", "NTPSynchronized", "--value"]).unwrap_or_default();
        let units: Vec<&str> = failed.lines().filter_map(|l| l.split_whitespace().next()).collect();
        json!({
            "state": state,
            "failed_units": units,
            "time_synchronized": match synced.trim() { "yes" => json!(true), "no" => json!(false), _ => Value::Null },
        })
    }
}

fn proc_stat() -> std::collections::HashMap<String, i64> {
    let mut out = std::collections::HashMap::new();
    for line in read_text("/proc/stat").unwrap_or_default().lines() {
        let (key, rest) = line.split_once(' ').unwrap_or((line, ""));
        if matches!(key, "ctxt" | "intr" | "processes" | "procs_running" | "procs_blocked") {
            if let Some(v) = rest.split_whitespace().next().and_then(|v| v.parse().ok()) {
                out.insert(key.to_string(), v);
            }
        }
    }
    out
}

pub fn pid_count() -> usize {
    list("/proc", |n| n.bytes().all(|b| b.is_ascii_digit())).len()
}

/// Logged-in sessions, as psutil.users() counts them: utmp USER_PROCESS
/// records. A host without utmp (Debian 13 no longer writes it) has none,
/// as psutil reports it.
fn users() -> Option<i64> {
    const RECORD: usize = 384; // struct utmp on x86_64 and aarch64 glibc
    const USER_PROCESS: i16 = 7;
    let Ok(data) = std::fs::read("/var/run/utmp").or_else(|_| std::fs::read("/run/utmp")) else { return Some(0) };
    Some(
        data.chunks_exact(RECORD)
            .filter(|r| i16::from_ne_bytes([r[0], r[1]]) == USER_PROCESS && r[44] != 0)
            .count() as i64,
    )
}

pub fn live(rates: &Rates, systemd: &Systemd) -> Value {
    let stat = proc_stat();
    let load = read_text("/proc/loadavg").unwrap_or_default();
    let fields: Vec<&str> = load.split_whitespace().collect();
    let threads = fields.get(3).and_then(|f| f.split_once('/')).and_then(|(_, t)| t.parse::<i64>().ok());
    let file_nr_text = read_text("/proc/sys/fs/file-nr").unwrap_or_default();
    let file_nr: Vec<i64> = file_nr_text.split_whitespace().filter_map(|v| v.parse().ok()).collect();
    let cpus = os::cpu_count() as f64;
    let uptime = read_text("/proc/uptime").and_then(|u| u.split_whitespace().next()?.parse::<f64>().ok());
    let loads: Option<Vec<f64>> = if fields.len() >= 3 {
        fields[..3].iter().map(|f| f.parse::<f64>().ok()).collect()
    } else {
        None
    };
    let g = |k: &str| stat.get(k).copied();
    json!({
        "hostname": os::hostname(),
        "time": os::now(),
        "uptime_seconds": uptime,
        "load_average": loads,
        "load_per_cpu": loads.as_ref().map(|l| l.iter().map(|x| round(x / cpus, 3)).collect::<Vec<_>>()),
        "processes": {
            "total": pid_count(),
            "threads": threads,
            "running": g("procs_running"),
            "blocked": g("procs_blocked"),
        },
        "users": users(),
        "file_handles": {
            "allocated": file_nr.first(),
            "max": file_nr.get(2),
        },
        "context_switches_per_s": rates.rate_i("host/ctxt", g("ctxt"), 1),
        "interrupts_per_s": rates.rate_i("host/intr", g("intr"), 1),
        "forks_per_s": rates.rate_i("host/processes", g("processes"), 2),
        "systemd": systemd.get(),
    })
}
