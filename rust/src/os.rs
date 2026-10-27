//! The few things the standard library does not say about this process and
//! machine: hostname, uname, CPU count, user names, clocks.

use std::ffi::CStr;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length; gethostname NUL-terminates
    // within it on success.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

pub struct Uname {
    pub release: String,
    pub version: String,
    pub machine: String,
}

pub fn uname() -> Uname {
    // SAFETY: utsname is plain data; uname fills it.
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe { libc::uname(&mut u) };
    let field = |f: &[libc::c_char]| {
        // SAFETY: uname NUL-terminates each field.
        unsafe { CStr::from_ptr(f.as_ptr()) }.to_string_lossy().into_owned()
    };
    Uname { release: field(&u.release), version: field(&u.version), machine: field(&u.machine) }
}

/// Online CPUs, as os.cpu_count() and psutil.cpu_count() report them.
pub fn cpu_count() -> i64 {
    // SAFETY: sysconf has no preconditions.
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if n > 0 { n as i64 } else { 1 }
}

pub fn clock_ticks() -> f64 {
    // SAFETY: sysconf has no preconditions.
    let n = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if n > 0 { n as f64 } else { 100.0 }
}

pub fn page_size() -> i64 {
    // SAFETY: sysconf has no preconditions.
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if n > 0 { n as i64 } else { 4096 }
}

pub fn uid() -> u32 {
    // SAFETY: getuid cannot fail.
    unsafe { libc::getuid() }
}

/// The name of a uid, or the uid as text (psutil's username()).
pub fn user_name(uid: u32) -> String {
    static CACHE: OnceLock<std::sync::Mutex<std::collections::HashMap<u32, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(name) = cache.lock().unwrap().get(&uid) {
        return name.clone();
    }
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: all pointers are valid for the call; the result points into buf.
    let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
    let name = if rc == 0 && !result.is_null() {
        unsafe { CStr::from_ptr(pwd.pw_name) }.to_string_lossy().into_owned()
    } else {
        uid.to_string()
    };
    cache.lock().unwrap().insert(uid, name.clone());
    name
}

/// Seconds since the Unix epoch.
pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Monotonic seconds since the process started.
pub fn mono() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// UTC ISO 8601 as Python's datetime.isoformat() writes it.
pub fn iso_utc(epoch: f64) -> String {
    let secs = epoch.floor() as i64;
    let micros = ((epoch - secs as f64) * 1e6).round() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let base = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", y, m, d, rem / 3600, rem % 3600 / 60, rem % 60);
    if micros > 0 { format!("{base}.{micros:06}+00:00") } else { format!("{base}+00:00") }
}

#[cfg(test)]
mod tests {
    #[test]
    fn iso() {
        assert_eq!(super::iso_utc(0.0), "1970-01-01T00:00:00+00:00");
        assert_eq!(super::iso_utc(1_793_054_393.0), "2026-10-26T22:39:53+00:00");
        assert_eq!(super::iso_utc(1_793_054_393.5), "2026-10-26T22:39:53.500000+00:00");
    }
}
