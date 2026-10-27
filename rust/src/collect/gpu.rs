//! NVIDIA GPUs through NVML: what each card is, and what it is doing.
//!
//! NVML is the driver's C library, loaded at runtime: a host without it
//! still reports everything else. Every query returns None instead of
//! failing: a card that does not support one (GB10 has no device memory,
//! GeForce no ECC) is a missing reading, not a failed sample.
//!
//! The clock-status logic started from sparkview (github.com/parallelArchitect/
//! sparkview, MIT): the load-gated clock status and its thresholds. Throttle
//! reasons are decoded with NVML's own bit values: sparkview's table maps
//! 0x100 to SW_POWER_CAP, but NVML says 0x100 is the display clock setting
//! and 0x4 is the power cap.

use std::collections::HashMap;
use std::ffi::{c_char, c_uint, CStr};
use std::sync::{Arc, Mutex};

use nvml_wrapper_sys::bindings::*;
use serde_json::{json, Map, Value};

use crate::collect::processes::read_proc;
use crate::os;
use crate::rates::Rates;
use crate::sysfs::{list, read_text, round};

// sparkview's thresholds: a clock under this while loaded is throttled, and
// under this utilization the GPU is idle whatever its clock says.
pub const CLOCK_THRESHOLD_MHZ: f64 = 1400.0;
const LOAD_GATE_PCT: f64 = 10.0;

// NVML clocks-event reason bits. 0x400 is not in NVML's headers, but
// nvidia-smi calls it Reliability, and it is the only bit set when it says
// only Reliability is active. Bits without a confirmed name are reported as hex.
const REASON_BITS: &[(u64, &str)] = &[
    (0x001, "GPU_IDLE"), (0x002, "APPLICATIONS_CLOCKS_SETTING"), (0x004, "SW_POWER_CAP"),
    (0x008, "HW_SLOWDOWN"), (0x010, "SYNC_BOOST"), (0x020, "SW_THERMAL_SLOWDOWN"),
    (0x040, "HW_THERMAL_SLOWDOWN"), (0x080, "HW_POWER_BRAKE_SLOWDOWN"),
    (0x100, "DISPLAY_CLOCK_SETTING"), (0x400, "RELIABILITY"),
];
pub const PROBLEM_BITS: u64 = 0x004 | 0x008 | 0x020 | 0x040 | 0x080;

// The cumulative clocks-event counters, in nanoseconds, by NVML field id
// (nvml.h's NVML_FI_DEV_CLOCKS_EVENT_REASON_*). The thermal and power-brake
// ids are current NVML's (pynvml 13.615: 269-271); nvml-wrapper-sys 0.10's
// header has them as 251-253, which a 615 driver does not answer.
pub const EVENT_FIELDS: &[(&str, u32)] = &[
    ("sw_power_cap", 74),
    ("sw_thermal_slowdown", 269),
    ("hw_thermal_slowdown", 270),
    ("hw_power_brake_slowdown", 271),
    ("sync_boost", 76),
];
const CLOCKS: &[(&str, u32)] = &[("graphics", 0), ("sm", 1), ("memory", 2), ("video", 3)];
const TEMP_THRESHOLDS: &[(&str, u32)] = &[("shutdown", 0), ("slowdown", 1), ("memory_max", 2), ("gpu_max", 3)];
const SYSTEMD_DIRS: &[&str] = &["/etc/systemd/system", "/lib/systemd/system", "/usr/lib/systemd/system"];
const OK: nvmlReturn_t = nvmlReturn_enum_NVML_SUCCESS;
const NOT_AVAILABLE: u64 = u64::MAX; // NVML_VALUE_NOT_AVAILABLE for process memory

// How often the costly or slow-moving readings are asked, whatever the
// sample interval. Measured on an idle RTX 5000: the clocks-event reasons are
// ~25 ms of CPU, each PCIe throughput call samples for 20 ms, energy and each
// clock ~5-9 ms; utilization, memory, temperature and power are well under 1.
pub const REASONS_TTL_S: f64 = 1.0;
pub const SLOW_TTL_S: f64 = 5.0;

fn arch_name(a: u32) -> Option<&'static str> {
    Some(match a {
        2 => "kepler", 3 => "maxwell", 4 => "pascal", 5 => "volta", 6 => "turing",
        7 => "ampere", 8 => "ada", 9 => "hopper", 10 => "blackwell", 0xffff_ffff => "unknown",
        _ => return None,
    })
}

fn brand_name(b: u32) -> Option<&'static str> {
    Some(match b {
        0 => "unknown", 1 => "quadro", 2 => "tesla", 3 => "nvs", 4 => "grid", 5 => "geforce",
        6 => "titan", 7 => "nvidia_vapps", 8 => "nvidia_vpc", 9 => "nvidia_vcs", 10 => "nvidia_vws",
        11 => "nvidia_vgaming", 12 => "quadro_rtx", 13 => "nvidia_rtx", 14 => "nvidia",
        15 => "geforce_rtx", 16 => "titan_rtx",
        _ => return None,
    })
}

fn compute_mode_name(m: u32) -> Option<&'static str> {
    Some(match m {
        0 => "default", 1 => "exclusive_thread", 2 => "prohibited", 3 => "exclusive_process",
        _ => return None,
    })
}

fn topology_name(l: u32) -> Option<&'static str> {
    Some(match l {
        0 => "internal", 10 => "single", 20 => "multiple", 30 => "hostbridge", 40 => "node", 50 => "system",
        _ => return None,
    })
}

fn watts(mw: Option<u32>) -> Option<f64> {
    mw.map(|m| round(m as f64 / 1000.0, 3))
}

fn c_str(buf: &[c_char]) -> String {
    // SAFETY: NVML NUL-terminates within the buffer it was given.
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}

// -- shared with /metrics.json ----------------------------------------------

pub fn decode_reasons(bitmask: u64) -> Vec<String> {
    let mut names: Vec<String> = REASON_BITS.iter().filter(|(b, _)| bitmask & b != 0).map(|(_, n)| n.to_string()).collect();
    let known = REASON_BITS.iter().fold(0, |acc, (b, _)| acc | b);
    let mut unknown = bitmask & !known;
    let mut bit = 1u64;
    while unknown != 0 {
        if unknown & bit != 0 {
            names.push(format!("{bit:#x}"));
            unknown &= !bit;
        }
        bit <<= 1;
    }
    names
}

/// sparkview's four states, load-gated. Returns (status, locked).
pub fn clock_status(clk: f64, clk_max: f64, util: f64, pstate: &str, problem: bool) -> (&'static str, bool) {
    let locked = 0.0 < clk_max && clk_max < 2900.0 && (clk - clk_max).abs() < 50.0;
    if util < LOAD_GATE_PCT || !matches!(pstate, "P0" | "P1" | "P2") {
        return ("IDLE", locked);
    }
    if locked {
        return ("LOCKED", locked);
    }
    if clk < CLOCK_THRESHOLD_MHZ || problem {
        return ("THROTTLED", locked);
    }
    ("PASS", locked)
}

/// (is_uma, total, used) for one GPU.
///
/// GB10's memory is the host's, and NVML says so by not reporting it. That,
/// not a total that happens to be near the host's, is the test: sparkview's
/// size comparison calls a 16 GB card in a 16 GB container unified.
pub fn gpu_memory(mem: Option<(u64, u64)>, host_total: i64, host_available: i64) -> (bool, i64, i64) {
    match mem {
        None => (true, host_total, host_total - host_available),
        Some((total, used)) => (false, total as i64, used as i64),
    }
}

/// The max of ``nvidia-smi -lgc MIN,MAX`` (or --lock-gpu-clocks) in a unit's ExecStart.
pub fn parse_lgc(exec_start: &str) -> Option<f64> {
    for flag in ["--lock-gpu-clocks", "-lgc"] {
        let mut rest = exec_start;
        while let Some(i) = rest.find(flag) {
            let after = rest[i + flag.len()..].trim_start_matches([' ', '=']);
            let (lo, tail) = after.split_once(',').unwrap_or(("", ""));
            let hi: String = tail.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
            if !lo.trim().is_empty() && lo.trim().chars().all(|c| c.is_ascii_digit()) && !hi.is_empty() {
                return hi.parse().ok();
            }
            rest = &rest[i + flag.len()..];
        }
    }
    None
}

/// The variables a unit's ExecStart sees from `Environment=` and
/// `EnvironmentFile=` (a leading `-` marks a file that may be missing).
pub fn unit_environment(unit_text: &str) -> HashMap<String, String> {
    let mut env = HashMap::new();
    let mut assign = |pair: &str| {
        if let Some((k, v)) = pair.trim().split_once('=') {
            if !k.trim().is_empty() {
                env.insert(k.trim().to_string(), v.trim().trim_matches('"').trim_matches('\'').to_string());
            }
        }
    };
    for line in unit_text.lines() {
        let line = line.trim();
        if let Some(path) = line.strip_prefix("EnvironmentFile=") {
            let path = path.trim().trim_start_matches('-');
            for entry in read_text(path).unwrap_or_default().lines() {
                if !entry.trim().is_empty() && !entry.trim_start().starts_with('#') {
                    assign(entry);
                }
            }
        } else if let Some(rest) = line.strip_prefix("Environment=") {
            // "quoted pairs" or bare words, as systemd splits them
            let mut chars = rest.chars().peekable();
            while chars.peek().is_some() {
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
                let mut word = String::new();
                if chars.peek() == Some(&'"') {
                    chars.next();
                    for c in chars.by_ref() {
                        if c == '"' {
                            break;
                        }
                        word.push(c);
                    }
                } else {
                    while let Some(&c) = chars.peek() {
                        if c.is_whitespace() {
                            break;
                        }
                        word.push(c);
                        chars.next();
                    }
                }
                if !word.is_empty() {
                    assign(word.trim_matches('"'));
                }
            }
        }
    }
    env
}

/// `${NAME}` and `$NAME` replaced as systemd does; unknown names are left.
pub fn expand_unit_vars(text: &str, env: &HashMap<String, String>) -> String {
    let b = text.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    while i < b.len() {
        if b[i] == b'$' {
            if b.get(i + 1) == Some(&b'{') {
                if let Some(end) = text[i + 2..].find('}') {
                    let name = &text[i + 2..i + 2 + end];
                    if !name.is_empty() && name.bytes().all(word) {
                        match env.get(name) {
                            Some(v) => out.push_str(v),
                            None => out.push_str(&text[i..i + 3 + end]),
                        }
                        i += 3 + end;
                        continue;
                    }
                }
            } else {
                let n = b[i + 1..].iter().take_while(|&&c| word(c)).count();
                if n > 0 {
                    let name = &text[i + 1..i + 1 + n];
                    match env.get(name) {
                        Some(v) => out.push_str(v),
                        None => out.push_str(&text[i..i + 1 + n]),
                    }
                    i += 1 + n;
                    continue;
                }
            }
        }
        let ch = text[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8().max(1);
    }
    out
}

fn cmdline(pid: i64) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| b.split(|&c| c == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect())
        .unwrap_or_default()
}

/// The port a GPU process serves on: the first `--port` on its own command
/// line or an ancestor's.
///
/// A GPU process is often not the server itself: vLLM's `EngineCore` and
/// `Worker_TP*` are children of the API server that took `--port`. A command
/// line is world-readable where the process's sockets are not (a container's
/// root-owned process, read by this exporter's own account), so this is what
/// lets pulsar-gui tell which service a card belongs to when one exporter
/// answers for several. None when no ancestor says.
pub fn served_port_of(args_of: impl Fn(i64) -> Vec<String>, parent_of: impl Fn(i64) -> Option<i64>, pid: i64) -> Option<i64> {
    let mut p = pid;
    for _ in 0..8 {
        let args = args_of(p);
        for (i, arg) in args.iter().enumerate() {
            let Some(rest) = arg.strip_prefix("--port") else { continue };
            let value = if rest.is_empty() {
                args.get(i + 1).cloned().unwrap_or_default()
            } else if let Some(v) = rest.strip_prefix('=').filter(|v| !v.is_empty() && v.bytes().all(|c| c.is_ascii_digit())) {
                v.to_string()
            } else {
                continue;
            };
            if !value.is_empty() && value.bytes().all(|c| c.is_ascii_digit()) {
                return value.parse().ok();
            }
        }
        p = parent_of(p)?;
        if p <= 1 {
            return None;
        }
    }
    None
}

pub fn served_port(pid: i64) -> Option<i64> {
    served_port_of(cmdline, |p| read_proc(p).map(|p| p.ppid), pid)
}

/// A process's served port does not change: looked up once per process
/// (pid and start time), for every table that shows it.
pub fn served_port_cached(pid: i64, start_ticks: i64) -> Option<i64> {
    static CACHE: Mutex<Option<HashMap<(i64, i64), Option<i64>>>> = Mutex::new(None);
    if let Some(v) = CACHE.lock().unwrap().get_or_insert_with(HashMap::new).get(&(pid, start_ticks)) {
        return *v;
    }
    let port = served_port(pid);
    let mut c = CACHE.lock().unwrap();
    let cache = c.get_or_insert_with(HashMap::new);
    if cache.len() > 4096 {
        cache.clear(); // processes come and go; a bound, not an eviction policy
    }
    cache.insert((pid, start_ticks), port);
    port
}

/// A lab clamp set by a ``*clock-cap.service`` unit, which NVML cannot see.
/// Reported only while the unit is active: an installed but stopped unit
/// clamps nothing. Checked at most every 10 s, since it costs a subprocess.
pub struct ClockCap {
    state: Mutex<(f64, Value)>,
}

impl Default for ClockCap {
    fn default() -> Self {
        ClockCap { state: Mutex::new((f64::NEG_INFINITY, Value::Null)) }
    }
}

impl ClockCap {
    pub fn get(&self) -> Value {
        let mut s = self.state.lock().unwrap();
        let now = os::mono();
        if now - s.0 >= 10.0 {
            *s = (now, Self::probe());
        }
        s.1.clone()
    }

    fn probe() -> Value {
        let mut seen = std::collections::HashSet::new();
        for dir in SYSTEMD_DIRS {
            for path in list(dir, |n| n.contains("clock-cap") && n.ends_with(".service")) {
                let unit = crate::sysfs::basename(&path);
                if !seen.insert(unit.clone()) {
                    continue;
                }
                let body = read_text(&path).unwrap_or_default();
                let exec: Vec<&str> = body.lines().filter(|l| l.trim().starts_with("ExecStart")).collect();
                // The cap is often a variable (`-lgc 0,${GPU_CLOCK_MAX}`) set
                // in an EnvironmentFile; resolve it the way systemd would.
                let Some(cap) = parse_lgc(&expand_unit_vars(&exec.join(" "), &unit_environment(&body))) else { continue };
                let active = std::process::Command::new("systemctl")
                    .args(["is-active", "--quiet", &unit])
                    .status()
                    .is_ok_and(|s| s.success());
                if active {
                    return json!({"max_mhz": cap, "source": unit});
                }
            }
        }
        Value::Null
    }
}

// -- NVML -------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct Handle(nvmlDevice_t);
// SAFETY: NVML device handles are process-wide and NVML is thread-safe.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

struct Lib(NvmlLib);
// SAFETY: the library's function pointers are immutable after loading, and
// NVML's API is documented thread-safe.
unsafe impl Send for Lib {}
unsafe impl Sync for Lib {}

/// Call an NVML function whose last argument is one out-parameter.
macro_rules! out1 {
    ($lib:expr, $f:ident, $t:ty $(, $arg:expr)*) => {{
        let mut out: $t = unsafe { std::mem::zeroed() };
        match $lib.0.$f.as_ref() {
            // SAFETY: the out-parameter is valid for writing; inputs are
            // handles and plain values NVML defines.
            Ok(f) => (unsafe { f($($arg,)* &mut out) } == OK).then_some(out),
            Err(_) => None,
        }
    }};
}

/// Two out-parameters.
macro_rules! out2 {
    ($lib:expr, $f:ident, $t:ty, $u:ty $(, $arg:expr)*) => {{
        let mut a: $t = unsafe { std::mem::zeroed() };
        let mut b: $u = unsafe { std::mem::zeroed() };
        match $lib.0.$f.as_ref() {
            Ok(f) => (unsafe { f($($arg,)* &mut a, &mut b) } == OK).then_some((a, b)),
            Err(_) => None,
        }
    }};
}

/// A string out-parameter with its length.
macro_rules! out_str {
    ($lib:expr, $f:ident $(, $arg:expr)*) => {{
        let mut buf = [0 as c_char; 256];
        match $lib.0.$f.as_ref() {
            Ok(f) => (unsafe { f($($arg,)* buf.as_mut_ptr(), buf.len() as c_uint) } == OK).then(|| c_str(&buf)),
            Err(_) => None,
        }
    }};
}

/// One card's queries.
#[derive(Clone)]
pub struct Gpu {
    lib: Arc<Lib>,
    pub h: Handle,
    pub index: usize,
}

impl Gpu {
    pub fn name(&self) -> Option<String> { out_str!(self.lib, nvmlDeviceGetName, self.h.0) }
    pub fn uuid(&self) -> Option<String> { out_str!(self.lib, nvmlDeviceGetUUID, self.h.0) }
    pub fn serial(&self) -> Option<String> { out_str!(self.lib, nvmlDeviceGetSerial, self.h.0) }
    pub fn board_part(&self) -> Option<String> { out_str!(self.lib, nvmlDeviceGetBoardPartNumber, self.h.0) }
    pub fn vbios(&self) -> Option<String> { out_str!(self.lib, nvmlDeviceGetVbiosVersion, self.h.0) }
    pub fn inforom_image(&self) -> Option<String> { out_str!(self.lib, nvmlDeviceGetInforomImageVersion, self.h.0) }
    pub fn u32_of(&self, which: U32) -> Option<u32> {
        let (l, h) = (&self.lib, self.h.0);
        match which {
            U32::Temperature => out1!(l, nvmlDeviceGetTemperature, c_uint, h, 0),
            U32::PowerUsage => out1!(l, nvmlDeviceGetPowerUsage, c_uint, h),
            U32::EnforcedLimit => out1!(l, nvmlDeviceGetEnforcedPowerLimit, c_uint, h),
            U32::DefaultLimit => out1!(l, nvmlDeviceGetPowerManagementDefaultLimit, c_uint, h),
            U32::PerformanceState => out1!(l, nvmlDeviceGetPerformanceState, nvmlPstates_t, h),
            U32::LinkGen => out1!(l, nvmlDeviceGetCurrPcieLinkGeneration, c_uint, h),
            U32::LinkWidth => out1!(l, nvmlDeviceGetCurrPcieLinkWidth, c_uint, h),
            U32::MaxLinkGen => out1!(l, nvmlDeviceGetMaxPcieLinkGeneration, c_uint, h),
            U32::MaxLinkWidth => out1!(l, nvmlDeviceGetMaxPcieLinkWidth, c_uint, h),
            U32::Replay => out1!(l, nvmlDeviceGetPcieReplayCounter, c_uint, h),
            U32::NumFans => out1!(l, nvmlDeviceGetNumFans, c_uint, h),
            U32::Cores => out1!(l, nvmlDeviceGetNumGpuCores, c_uint, h),
            U32::BusWidth => out1!(l, nvmlDeviceGetMemoryBusWidth, c_uint, h),
            U32::Minor => out1!(l, nvmlDeviceGetMinorNumber, c_uint, h),
            U32::Brand => out1!(l, nvmlDeviceGetBrand, nvmlBrandType_t, h),
            U32::Architecture => out1!(l, nvmlDeviceGetArchitecture, nvmlDeviceArchitecture_t, h),
            U32::ComputeMode => out1!(l, nvmlDeviceGetComputeMode, nvmlComputeMode_t, h),
            U32::Persistence => out1!(l, nvmlDeviceGetPersistenceMode, nvmlEnableState_t, h),
            U32::DisplayActive => out1!(l, nvmlDeviceGetDisplayActive, nvmlEnableState_t, h),
        }
    }
    pub fn clock(&self, kind: u32) -> Option<u32> { out1!(self.lib, nvmlDeviceGetClockInfo, c_uint, self.h.0, kind) }
    pub fn max_clock(&self, kind: u32) -> Option<u32> { out1!(self.lib, nvmlDeviceGetMaxClockInfo, c_uint, self.h.0, kind) }
    pub fn temp_threshold(&self, kind: u32) -> Option<u32> { out1!(self.lib, nvmlDeviceGetTemperatureThreshold, c_uint, self.h.0, kind) }
    pub fn fan(&self, i: u32) -> Option<u32> { out1!(self.lib, nvmlDeviceGetFanSpeed_v2, c_uint, self.h.0, i) }
    pub fn pcie_throughput(&self, counter: u32) -> Option<u32> { out1!(self.lib, nvmlDeviceGetPcieThroughput, c_uint, self.h.0, counter) }
    pub fn energy_mj(&self) -> Option<u64> { out1!(self.lib, nvmlDeviceGetTotalEnergyConsumption, u64, self.h.0) }
    pub fn utilization(&self) -> Option<(u32, u32)> {
        out1!(self.lib, nvmlDeviceGetUtilizationRates, nvmlUtilization_t, self.h.0).map(|u| (u.gpu, u.memory))
    }
    pub fn encoder(&self) -> Option<u32> { out2!(self.lib, nvmlDeviceGetEncoderUtilization, c_uint, c_uint, self.h.0).map(|(u, _)| u) }
    pub fn decoder(&self) -> Option<u32> { out2!(self.lib, nvmlDeviceGetDecoderUtilization, c_uint, c_uint, self.h.0).map(|(u, _)| u) }
    pub fn memory(&self) -> Option<(u64, u64)> {
        out1!(self.lib, nvmlDeviceGetMemoryInfo, nvmlMemory_t, self.h.0).map(|m| (m.total, m.used))
    }
    pub fn bar1(&self) -> Option<(u64, u64)> {
        out1!(self.lib, nvmlDeviceGetBAR1MemoryInfo, nvmlBAR1Memory_t, self.h.0).map(|m| (m.bar1Total, m.bar1Used))
    }
    pub fn pci(&self) -> Option<nvmlPciInfo_t> { out1!(self.lib, nvmlDeviceGetPciInfo_v3, nvmlPciInfo_t, self.h.0) }
    pub fn compute_capability(&self) -> Option<(i32, i32)> {
        out2!(self.lib, nvmlDeviceGetCudaComputeCapability, i32, i32, self.h.0)
    }
    pub fn limit_constraints(&self) -> Option<(u32, u32)> {
        out2!(self.lib, nvmlDeviceGetPowerManagementLimitConstraints, c_uint, c_uint, self.h.0)
    }
    pub fn ecc_mode(&self) -> Option<(u32, u32)> { out2!(self.lib, nvmlDeviceGetEccMode, nvmlEnableState_t, nvmlEnableState_t, self.h.0) }
    pub fn mig_mode(&self) -> Option<(u32, u32)> { out2!(self.lib, nvmlDeviceGetMigMode, c_uint, c_uint, self.h.0) }
    pub fn ecc_errors(&self, kind: u32, counter: u32) -> Option<u64> {
        out1!(self.lib, nvmlDeviceGetTotalEccErrors, u64, self.h.0, kind, counter)
    }
    pub fn remapped_rows(&self) -> Option<(u32, u32, u32, u32)> {
        let (mut a, mut b, mut c, mut d) = (0, 0, 0, 0);
        let f = self.lib.0.nvmlDeviceGetRemappedRows.as_ref().ok()?;
        // SAFETY: four valid out-parameters.
        (unsafe { f(self.h.0, &mut a, &mut b, &mut c, &mut d) } == OK).then_some((a, b, c, d))
    }
    pub fn reasons_raw(&self) -> Option<u64> {
        out1!(self.lib, nvmlDeviceGetCurrentClocksEventReasons, u64, self.h.0)
            .or_else(|| out1!(self.lib, nvmlDeviceGetCurrentClocksThrottleReasons, u64, self.h.0))
    }
    pub fn nvlink_state(&self, link: u32) -> Option<u32> { out1!(self.lib, nvmlDeviceGetNvLinkState, nvmlEnableState_t, self.h.0, link) }
    pub fn nvlink_version(&self, link: u32) -> Option<u32> { out1!(self.lib, nvmlDeviceGetNvLinkVersion, c_uint, self.h.0, link) }
    pub fn nvlink_remote(&self, link: u32) -> Option<String> {
        out1!(self.lib, nvmlDeviceGetNvLinkRemotePciInfo_v2, nvmlPciInfo_t, self.h.0, link).map(|p| c_str(&p.busId))
    }
    pub fn topology(&self, other: &Gpu) -> Option<u32> {
        out1!(self.lib, nvmlDeviceGetTopologyCommonAncestor, nvmlGpuTopologyLevel_t, self.h.0, other.h.0)
    }
    pub fn p2p(&self, other: &Gpu, cap: u32) -> Option<u32> {
        out1!(self.lib, nvmlDeviceGetP2PStatus, nvmlGpuP2PStatus_t, self.h.0, other.h.0, cap)
    }

    /// The cumulative clocks-event counters, in seconds.
    pub fn events(&self) -> Vec<(&'static str, f64)> {
        let Ok(f) = self.lib.0.nvmlDeviceGetFieldValues.as_ref() else { return vec![] };
        let mut values: Vec<nvmlFieldValue_t> = EVENT_FIELDS
            .iter()
            .map(|(_, id)| {
                let mut v: nvmlFieldValue_t = unsafe { std::mem::zeroed() };
                v.fieldId = *id;
                v
            })
            .collect();
        // SAFETY: values holds as many entries as the count says.
        if unsafe { f(self.h.0, values.len() as i32, values.as_mut_ptr()) } != OK {
            return vec![];
        }
        EVENT_FIELDS
            .iter()
            .zip(values)
            .filter(|(_, v)| v.nvmlReturn == OK)
            // SAFETY: these fields are unsigned long long counters.
            .map(|((name, _), v)| (*name, round(unsafe { v.value.ullVal } as f64 / 1e9, 6)))
            .collect()
    }

    /// (pid, used memory) of compute and graphics processes, each with its kind.
    pub fn processes(&self) -> Vec<(u32, Option<i64>, &'static str)> {
        let mut out = Vec::new();
        for (kind, f) in [
            ("compute", self.lib.0.nvmlDeviceGetComputeRunningProcesses_v3.as_ref()),
            ("graphics", self.lib.0.nvmlDeviceGetGraphicsRunningProcesses_v3.as_ref()),
        ] {
            let Ok(f) = f else { continue };
            let mut count: c_uint = 0;
            // SAFETY: a zero count with a null buffer asks for the count.
            let rc = unsafe { f(self.h.0, &mut count, std::ptr::null_mut()) };
            if rc == OK || rc != nvmlReturn_enum_NVML_ERROR_INSUFFICIENT_SIZE {
                continue;
            }
            count += 5; // processes may start between the two calls
            let mut infos: Vec<nvmlProcessInfo_t> = vec![unsafe { std::mem::zeroed() }; count as usize];
            // SAFETY: infos holds count entries.
            if unsafe { f(self.h.0, &mut count, infos.as_mut_ptr()) } != OK {
                continue;
            }
            for p in &infos[..count as usize] {
                let mem = (p.usedGpuMemory != NOT_AVAILABLE).then_some(p.usedGpuMemory as i64);
                out.push((p.pid, mem, kind));
            }
        }
        out
    }
}

pub enum U32 {
    Temperature, PowerUsage, EnforcedLimit, DefaultLimit, PerformanceState, LinkGen, LinkWidth,
    MaxLinkGen, MaxLinkWidth, Replay, NumFans, Cores, BusWidth, Minor, Brand, Architecture,
    ComputeMode, Persistence, DisplayActive,
}

struct State {
    lib: Option<Arc<Lib>>,
    error: Option<String>,
    tried: f64,
}

#[derive(Clone)]
pub struct Slow {
    enc: Option<u32>,
    dec: Option<u32>,
    energy_mj: Option<u64>,
    clocks: Vec<(&'static str, Option<u32>)>,
    events: Vec<(&'static str, f64)>,
    tx: Option<u32>,
    rx: Option<u32>,
    replay: Option<u32>,
    ecc: Value,
    remapped: Option<(u32, u32, u32, u32)>,
    fans: Vec<Option<u32>>,
}

/// NVML, loaded and initialised lazily and retried every 30 s until the
/// driver is ready.
pub struct Nvml {
    state: Mutex<State>,
    reasons: Mutex<HashMap<usize, (f64, Option<u64>)>>,
    slow: Mutex<HashMap<usize, (f64, Slow)>>,
    windows: Mutex<HashMap<String, (f64, f64, Option<f64>)>>,
    max_clock: Mutex<HashMap<String, Option<u32>>>,
}

impl Default for Nvml {
    fn default() -> Self {
        Nvml {
            state: Mutex::new(State { lib: None, error: None, tried: f64::NEG_INFINITY }),
            reasons: Mutex::default(),
            slow: Mutex::default(),
            windows: Mutex::default(),
            max_clock: Mutex::default(),
        }
    }
}

impl Nvml {
    const RETRY_S: f64 = 30.0;

    fn lib(&self) -> Option<Arc<Lib>> {
        let mut s = self.state.lock().unwrap();
        if s.lib.is_some() {
            return s.lib.clone();
        }
        let now = os::mono();
        if now - s.tried < Self::RETRY_S {
            return None;
        }
        s.tried = now;
        // SAFETY: loading the driver's NVML library runs its constructors,
        // which is what every NVML client does.
        let loaded = unsafe { NvmlLib::new("libnvidia-ml.so.1") }.or_else(|_| unsafe { NvmlLib::new("libnvidia-ml.so") });
        let lib = match loaded {
            Ok(l) => Lib(l),
            Err(_) => {
                s.error = Some("NVML Shared Library Not Found".into());
                return None;
            }
        };
        let rc = match lib.0.nvmlInit_v2.as_ref() {
            // SAFETY: nvmlInit_v2 takes no arguments.
            Ok(f) => unsafe { f() },
            Err(_) => nvmlReturn_enum_NVML_ERROR_FUNCTION_NOT_FOUND,
        };
        if rc != OK {
            let msg = lib.0.nvmlErrorString.as_ref().ok()
                // SAFETY: nvmlErrorString returns a static string.
                .map(|f| unsafe { CStr::from_ptr(f(rc)) }.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("NVML error {rc}"));
            if s.error.as_deref() != Some(&msg) {
                eprintln!("WARNING NVML unavailable: {msg}");
            }
            s.error = Some(msg);
            return None;
        }
        s.error = None;
        s.lib = Some(Arc::new(lib));
        s.lib.clone()
    }

    pub fn ready(&self) -> bool {
        self.lib().is_some()
    }

    pub fn error(&self) -> Option<String> {
        self.state.lock().unwrap().error.clone()
    }

    pub fn gpus(&self) -> Vec<Gpu> {
        let Some(lib) = self.lib() else { return vec![] };
        let count = out1!(lib, nvmlDeviceGetCount_v2, c_uint).unwrap_or(0);
        (0..count)
            .filter_map(|i| out1!(lib, nvmlDeviceGetHandleByIndex_v2, nvmlDevice_t, i).map(|h| Gpu { lib: lib.clone(), h: Handle(h), index: i as usize }))
            .collect()
    }

    /// (driver, cuda, nvml) versions.
    pub fn versions(&self) -> (Option<String>, Option<String>, Option<String>) {
        let Some(lib) = self.lib() else { return (None, None, None) };
        let cuda = out1!(lib, nvmlSystemGetCudaDriverVersion_v2, i32).or_else(|| out1!(lib, nvmlSystemGetCudaDriverVersion, i32));
        (
            out_str!(lib, nvmlSystemGetDriverVersion),
            cuda.map(|c| format!("{}.{}", c / 1000, c % 1000 / 10)),
            out_str!(lib, nvmlSystemGetNVMLVersion),
        )
    }

    /// The clocks-event reason bits, shared with /metrics.json, at most once a second.
    pub fn reasons(&self, g: &Gpu) -> Option<u64> {
        let now = os::mono();
        if let Some((t, v)) = self.reasons.lock().unwrap().get(&g.index) {
            if now - t < REASONS_TTL_S {
                return *v;
            }
        }
        let v = g.reasons_raw();
        self.reasons.lock().unwrap().insert(g.index, (now, v));
        v
    }

    fn slow(&self, g: &Gpu) -> (Slow, f64) {
        let now = os::mono();
        if let Some((t, v)) = self.slow.lock().unwrap().get(&g.index) {
            if now - t < SLOW_TTL_S {
                return (v.clone(), now - t);
            }
        }
        let ecc = match g.ecc_mode() {
            Some((cur, _)) if cur != 0 => json!({
                "volatile_corrected": g.ecc_errors(0, 0), "volatile_uncorrected": g.ecc_errors(1, 0),
                "aggregate_corrected": g.ecc_errors(0, 1), "aggregate_uncorrected": g.ecc_errors(1, 1),
            }),
            _ => Value::Null,
        };
        let fans = g.u32_of(U32::NumFans).unwrap_or(0);
        let v = Slow {
            enc: g.encoder(),
            dec: g.decoder(),
            energy_mj: g.energy_mj(),
            clocks: CLOCKS.iter().filter(|(k, _)| *k != "graphics").map(|(k, c)| (*k, g.clock(*c))).collect(),
            events: g.events(),
            tx: g.pcie_throughput(0),
            rx: g.pcie_throughput(1),
            replay: g.u32_of(U32::Replay),
            ecc,
            remapped: g.remapped_rows(),
            fans: (0..fans).map(|i| g.fan(i)).collect(),
        };
        self.slow.lock().unwrap().insert(g.index, (now, v.clone()));
        (v, 0.0)
    }

    /// Per-second rate of a tiered counter over its own read interval. A
    /// reused reading keeps the rate of the last real one, rather than reading
    /// as a zero change against the sample's interval.
    fn window_rate(&self, key: &str, value: Option<f64>, fresh: bool) -> Option<f64> {
        let value = value?;
        let now = os::mono();
        let mut w = self.windows.lock().unwrap();
        let prev = w.get(key).copied();
        if !fresh {
            return prev.and_then(|p| p.2);
        }
        let rate = prev.and_then(|(t, v, _)| (value >= v && now > t).then(|| (value - v) / (now - t)));
        w.insert(key.to_string(), (now, value, rate));
        rate
    }

    /// A card's max graphics clock does not change, and each query is an
    /// ioctl (~5 ms of CPU on a Turing card), so it is asked once per card.
    pub fn max_graphics_clock(&self, g: &Gpu, uuid: &str) -> Option<u32> {
        if let Some(v) = self.max_clock.lock().unwrap().get(uuid) {
            if v.is_some() {
                return *v;
            }
        }
        let v = g.max_clock(0);
        self.max_clock.lock().unwrap().insert(uuid.to_string(), v);
        v
    }
}

fn pair(v: Option<(u32, u32)>) -> Value {
    v.map(|(c, p)| json!({"current": c, "pending": p})).unwrap_or(Value::Null)
}

fn device_static(g: &Gpu, host_total: Option<i64>) -> Value {
    let pci = g.pci();
    let mem = g.memory();
    let limits = g.limit_constraints();
    let max_links = NVML_NVLINK_MAX_LINKS;
    let nvlinks: Vec<Value> = (0..max_links)
        .filter_map(|link| {
            let state = g.nvlink_state(link)?;
            Some(json!({"link": link, "active": state != 0, "version": g.nvlink_version(link), "remote_bus_id": g.nvlink_remote(link)}))
        })
        .collect();
    let mut thresholds = Map::new();
    for (k, t) in TEMP_THRESHOLDS {
        thresholds.insert(k.to_string(), json!(g.temp_threshold(*t)));
    }
    let mut max_clocks = Map::new();
    for (k, c) in CLOCKS {
        max_clocks.insert(k.to_string(), json!(g.max_clock(*c)));
    }
    json!({
        "index": g.index,
        "name": g.name(),
        "uuid": g.uuid(),
        "serial": g.serial(),
        "board_part_number": g.board_part(),
        "brand": g.u32_of(U32::Brand).and_then(brand_name),
        "architecture": g.u32_of(U32::Architecture).and_then(arch_name),
        "compute_capability": g.compute_capability().map(|(a, b)| format!("{a}.{b}")),
        "cores": g.u32_of(U32::Cores),
        "vbios": g.vbios(),
        "inforom_image": g.inforom_image(),
        "minor_number": g.u32_of(U32::Minor),
        "pci": {
            "bus_id": pci.map(|p| c_str(&p.busId)),
            "device_id": pci.map(|p| format!("{:04x}", (p.pciDeviceId >> 16) & 0xffff)),
            "subsystem_id": pci.map(|p| format!("{:08x}", p.pciSubSystemId)),
            "max_link_generation": g.u32_of(U32::MaxLinkGen),
            "max_link_width": g.u32_of(U32::MaxLinkWidth),
        },
        "is_uma": mem.is_none(),
        "memory_total_bytes": mem.map(|m| m.0 as i64).or(host_total),
        "memory_bus_width_bits": g.u32_of(U32::BusWidth),
        "max_clocks_mhz": max_clocks,
        "power_limits_watts": {
            "default": watts(g.u32_of(U32::DefaultLimit)),
            "min": watts(limits.map(|l| l.0)),
            "max": watts(limits.map(|l| l.1)),
            "enforced": watts(g.u32_of(U32::EnforcedLimit)),
        },
        "temperature_thresholds_celsius": thresholds,
        "persistence_mode": g.u32_of(U32::Persistence),
        "compute_mode": g.u32_of(U32::ComputeMode).and_then(compute_mode_name),
        "ecc_mode": pair(g.ecc_mode()),
        "mig_mode": pair(g.mig_mode()),
        "display_active": g.u32_of(U32::DisplayActive),
        "fans": g.u32_of(U32::NumFans),
        "nvlink": nvlinks,
    })
}

/// How each pair of GPUs is connected, and what peer-to-peer it allows.
fn topology(gpus: &[Gpu]) -> Value {
    let caps = [("read", 0u32), ("write", 1), ("nvlink", 2), ("atomics", 3)];
    let mut out = Vec::new();
    for (i, a) in gpus.iter().enumerate() {
        for b in &gpus[i + 1..] {
            let mut p2p = Map::new();
            for (name, idx) in caps {
                p2p.insert(name.into(), json!(a.p2p(b, idx).map(|s| s == nvmlGpuP2PStatus_enum_NVML_P2P_STATUS_OK)));
            }
            out.push(json!({"a": a.index, "b": b.index, "level": a.topology(b).and_then(topology_name), "p2p": p2p}));
        }
    }
    Value::Array(out)
}

pub fn static_(nv: &Nvml, host_total: Option<i64>) -> Value {
    let gpus = nv.gpus();
    if !nv.ready() {
        return json!({"available": false, "error": nv.error(), "devices": []});
    }
    let (driver, cuda, nvml) = nv.versions();
    json!({
        "available": true,
        "driver": driver,
        "cuda": cuda,
        "nvml": nvml,
        "count": gpus.len(),
        "devices": gpus.iter().map(|g| device_static(g, host_total)).collect::<Vec<_>>(),
        "topology": topology(&gpus),
    })
}

// -- live -------------------------------------------------------------------

/// Who is on a GPU: names and owners, never command lines, which can carry
/// secrets (an --api-key).
fn process_rows(g: &Gpu) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut rows: Vec<Value> = g
        .processes()
        .into_iter()
        .filter(|(pid, _, _)| seen.insert(*pid))
        .map(|(pid, mem, kind)| {
            // Another PID namespace: a container cannot see the host's PIDs.
            let p = read_proc(pid as i64);
            json!({
                "pid": pid, "type": kind,
                "name": p.as_ref().map(|p| p.name.clone()),
                "user": p.as_ref().and_then(|p| p.uid).map(os::user_name),
                "ppid": p.as_ref().map(|p| p.ppid),
                "port": p.as_ref().and_then(|p| served_port_cached(p.pid, p.start_ticks)),
                "gpu_memory_bytes": mem,
            })
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r["gpu_memory_bytes"].as_i64().unwrap_or(0)));
    rows
}

fn device_live(nv: &Nvml, g: &Gpu, host_total: i64, host_available: i64, clock_cap: &Value) -> Value {
    let uuid = g.uuid();
    let key = format!("gpu/{}", uuid.clone().unwrap_or_else(|| g.index.to_string()));
    let util = g.utilization();
    let (uma, total, used) = gpu_memory(g.memory(), host_total, host_available);
    let bar1 = g.bar1();
    let graphics = g.clock(0);
    let clk_max = nv.max_graphics_clock(g, uuid.as_deref().unwrap_or(""));
    let pstate = g.u32_of(U32::PerformanceState).map(|p| format!("P{p}"));
    let raw = nv.reasons(g);
    let util_pct = util.map(|u| u.0);

    let (slow, slow_age) = nv.slow(g);
    let fresh = slow_age == 0.0;

    let throttle = match (raw, graphics, clk_max, &pstate) {
        (Some(raw), Some(clk), Some(max), Some(ps)) => {
            let problem = raw & PROBLEM_BITS != 0;
            let (status, locked) = clock_status(clk as f64, max as f64, util_pct.unwrap_or(0) as f64, ps, problem);
            json!({"available": true, "status": status, "reasons": decode_reasons(raw), "raw": raw,
                   "problem": problem, "locked": locked, "threshold_mhz": CLOCK_THRESHOLD_MHZ})
        }
        _ => json!({"available": false}),
    };

    let mut events = Map::new();
    let mut event_pct = Map::new();
    for (name, seconds) in &slow.events {
        events.insert(name.to_string(), json!(seconds));
        let r = nv.window_rate(&format!("{key}/event/{name}"), Some(*seconds), fresh);
        event_pct.insert(name.to_string(), json!(r.map(|r| round(100.0 * r, 2))));
    }
    let energy_rate = nv.window_rate(&format!("{key}/energy"), slow.energy_mj.map(|e| e as f64), fresh);
    let kib = 1024i64; // NVML's PCIe throughput is in KB/s, 1024-byte
    let mut clocks = Map::new();
    clocks.insert("graphics".into(), json!(graphics));
    for (k, v) in &slow.clocks {
        clocks.insert(k.to_string(), json!(v));
    }

    json!({
        "index": g.index,
        "uuid": uuid,
        "name": g.name(),
        "utilization": {
            "gpu_percent": util_pct,
            "memory_percent": util.map(|u| u.1),
            "encoder_percent": slow.enc,
            "decoder_percent": slow.dec,
        },
        "memory": {
            "is_uma": uma, "total_bytes": total, "used_bytes": used,
            "free_bytes": total - used,
            "used_percent": if total != 0 { json!(round(100.0 * used as f64 / total as f64, 2)) } else { Value::Null },
            "bar1_total_bytes": bar1.map(|b| b.0),
            "bar1_used_bytes": bar1.map(|b| b.1),
        },
        "temperature_celsius": g.u32_of(U32::Temperature),
        "fan_speeds_percent": slow.fans,
        "power": {
            "draw_watts": watts(g.u32_of(U32::PowerUsage)),
            "limit_watts": watts(g.u32_of(U32::EnforcedLimit)),
            "energy_joules": slow.energy_mj.map(|e| round(e as f64 / 1000.0, 3)),
            // mJ per second is mW: the mean draw over the energy counter's window
            "average_watts": energy_rate.map(|r| round(r / 1000.0, 3)),
        },
        "clocks_mhz": clocks,
        "max_graphics_clock_mhz": clk_max,
        "pstate": pstate,
        "throttle": throttle,
        "clock_event_seconds": events,
        "clock_event_percent": event_pct,
        "clock_cap": clock_cap,
        "pcie": {
            "link_generation": g.u32_of(U32::LinkGen),
            "link_width": g.u32_of(U32::LinkWidth),
            "tx_bytes_per_s": slow.tx.map(|t| t as i64 * kib),
            "rx_bytes_per_s": slow.rx.map(|r| r as i64 * kib),
            "replay_count": slow.replay,
        },
        "ecc_errors": slow.ecc,
        "remapped_rows": slow.remapped.map(|(c, u, p, f)| json!({"corrected": c, "uncorrected": u, "pending": p != 0, "failed": f != 0})),
        // How old the 5 s tier (PCIe throughput, encoder/decoder, the other
        // clocks, energy, event counters, ECC, fans) is in this sample.
        "slow_readings_age_s": round(slow_age, 2),
        "processes": process_rows(g),
    })
}

/// Every GPU, the cards in parallel: a card's queries take ~100 ms (the PCIe
/// throughput counters sample for 20 ms each), and NVML is thread-safe, so N
/// cards cost one card's time.
pub fn live(nv: &Nvml, _rates: &Rates, host_total: i64, host_available: i64, clock_cap: &ClockCap) -> Value {
    let gpus = nv.gpus();
    if gpus.is_empty() {
        return json!([]);
    }
    let cap = clock_cap.get();
    if gpus.len() == 1 {
        return json!([device_live(nv, &gpus[0], host_total, host_available, &cap)]);
    }
    let rows: Vec<Value> = std::thread::scope(|s| {
        let handles: Vec<_> = gpus.iter().map(|g| s.spawn(|| device_live(nv, g, host_total, host_available, &cap))).collect();
        handles.into_iter().map(|h| h.join().unwrap_or(Value::Null)).collect()
    });
    Value::Array(rows)
}

/// pid -> the GPUs it holds memory on, for the process table.
pub fn process_memory(nv: &Nvml) -> HashMap<i64, Vec<(i64, Option<i64>)>> {
    let mut out: HashMap<i64, Vec<(i64, Option<i64>)>> = HashMap::new();
    for g in nv.gpus() {
        for (pid, mem, _) in g.processes() {
            let rows = out.entry(pid as i64).or_default();
            if !rows.iter().any(|(i, _)| *i == g.index as i64) {
                rows.push((g.index as i64, mem));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_use_nvml_bits() {
        assert_eq!(decode_reasons(0x400), vec!["RELIABILITY"]);
        assert_eq!(decode_reasons(0x004), vec!["SW_POWER_CAP"]);
        assert_eq!(decode_reasons(0x100), vec!["DISPLAY_CLOCK_SETTING"]);
        assert_eq!(decode_reasons(0x201), vec!["GPU_IDLE", "0x200"]);
    }

    #[test]
    fn clock_states() {
        for (clk, max, util, ps, problem, status) in [
            (208.0, 3003.0, 0.0, "P8", false, "IDLE"),
            (2400.0, 3003.0, 90.0, "P0", false, "PASS"),
            (1200.0, 3003.0, 90.0, "P0", false, "THROTTLED"),
            (2400.0, 3003.0, 90.0, "P0", true, "THROTTLED"),
            (2195.0, 2200.0, 90.0, "P0", false, "LOCKED"),
        ] {
            assert_eq!(clock_status(clk, max, util, ps, problem).0, status);
        }
    }

    #[test]
    fn lgc_cap_is_read_from_exec_start() {
        assert_eq!(parse_lgc("ExecStart=/usr/bin/nvidia-smi -lgc 300,2200"), Some(2200.0));
        assert_eq!(parse_lgc("ExecStart=nvidia-smi --lock-gpu-clocks=300,2300"), Some(2300.0));
        assert_eq!(parse_lgc("ExecStart=/usr/bin/true"), None);
    }

    #[test]
    fn served_port_is_inherited_from_the_server_that_took_it() {
        let procs: HashMap<i64, (Vec<&str>, i64)> = [
            (1, (vec!["/sbin/init"], 0)),
            (40, (vec!["python3", "-m", "vllm.entrypoints.openai.api_server", "--port", "8001"], 1)),
            (41, (vec!["VLLM::EngineCore"], 40)),
            (42, (vec!["VLLM::Worker_TP0"], 41)),
            (7, (vec!["pulsar-server", "-m", "m", "--port=8000"], 0)),
            (8, (vec!["pulsar-server", "--tp-rank", "1"], 1)),
        ].into_iter().collect();
        let args = |p: i64| procs.get(&p).map(|(a, _)| a.iter().map(|s| s.to_string()).collect()).unwrap_or_default();
        let parent = |p: i64| procs.get(&p).map(|(_, pp)| *pp).filter(|pp| *pp != 0);
        assert_eq!(served_port_of(args, parent, 42), Some(8001));
        assert_eq!(served_port_of(args, parent, 7), Some(8000));
        assert_eq!(served_port_of(args, parent, 8), None);
    }

    #[test]
    fn a_clock_cap_given_as_a_variable_is_resolved() {
        let dir = std::env::temp_dir().join(format!("vitals-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let env_file = dir.join("gpu-clock-cap");
        std::fs::write(&env_file, "# thermal mitigation\nGPU_CLOCK_MAX=2300\n").unwrap();
        let unit = format!(
            "[Service]\nEnvironmentFile=-{}\nEnvironment=\"OTHER=1\"\nExecStart=/bin/sh -c \"for i in $(seq 10); do /usr/bin/nvidia-smi -lgc 0,${{GPU_CLOCK_MAX}} && exit 0; done\"\n",
            env_file.display()
        );
        let env = unit_environment(&unit);
        assert_eq!(env["GPU_CLOCK_MAX"], "2300");
        assert_eq!(env["OTHER"], "1");
        let exec: Vec<&str> = unit.lines().filter(|l| l.starts_with("ExecStart")).collect();
        assert_eq!(parse_lgc(&expand_unit_vars(&exec.join(" "), &env)), Some(2300.0));
        assert_eq!(expand_unit_vars("a $UNKNOWN ${ALSO} b", &env), "a $UNKNOWN ${ALSO} b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uma_means_nvml_cannot_report_the_memory() {
        let g = 1i64 << 30;
        assert_eq!(gpu_memory(Some((16 << 30, 13 << 30)), 16 * g, 6 * g), (false, 16 * g, 13 * g));
        assert_eq!(gpu_memory(None, 16 * g, 6 * g), (true, 16 * g, 10 * g));
    }
}
