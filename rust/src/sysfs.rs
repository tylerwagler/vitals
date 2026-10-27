//! Reading /sys, /proc and small tools without letting any of them take a
//! reading down with it: every helper returns None for a value it cannot get.

use std::cmp::Ordering;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

pub fn read_text(path: impl AsRef<Path>) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    Some(text.trim().to_string())
}

/// Decimal, as sysfs writes nearly everything ("08" too); hex only when it says so.
pub fn parse_int(text: &str) -> Option<i64> {
    let t = text.trim();
    if t.len() > 2 && t[..2].eq_ignore_ascii_case("0x") {
        i64::from_str_radix(&t[2..], 16).ok()
    } else {
        t.parse::<i64>().ok()
    }
}

pub fn read_int(path: impl AsRef<Path>) -> Option<i64> {
    parse_int(&read_text(path)?)
}

/// Python's round(x, digits): half to even on the decimal value.
pub fn round(x: f64, digits: i32) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let s = format!("{:.*}", digits.max(0) as usize, x);
    s.parse().unwrap_or(x)
}

pub fn scaled(path: impl AsRef<Path>, scale: f64, digits: i32) -> Option<f64> {
    read_int(path).map(|v| round(v as f64 * scale, digits))
}

/// The basename a symlink points at: a device's driver, its IOMMU group.
pub fn link_name(path: impl AsRef<Path>) -> Option<String> {
    let target = fs::read_link(path).ok()?;
    target.file_name().map(|n| n.to_string_lossy().into_owned())
}

pub fn basename(path: impl AsRef<Path>) -> String {
    path.as_ref().file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

pub fn realpath(path: impl AsRef<Path>) -> Option<PathBuf> {
    fs::canonicalize(path).ok()
}

pub fn exists(path: impl AsRef<Path>) -> bool {
    fs::symlink_metadata(path.as_ref()).is_ok() && path.as_ref().exists()
}

pub fn is_dir(path: impl AsRef<Path>) -> bool {
    path.as_ref().is_dir()
}

/// 'always [madvise] never' -> 'madvise', the kernel's way of marking a choice.
pub fn bracketed(text: Option<String>) -> Option<String> {
    let text = text?;
    match (text.find('['), text.find(']')) {
        (Some(a), Some(b)) if b > a => Some(text[a + 1..b].to_string()),
        _ => Some(text),
    }
}

/// Sort key that puts hwmon2 before hwmon10.
#[derive(Debug, PartialEq, Eq)]
pub enum Part {
    Num(u64),
    Text(String),
}

impl PartialOrd for Part {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Part {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Part::Num(a), Part::Num(b)) => a.cmp(b),
            (Part::Text(a), Part::Text(b)) => a.cmp(b),
            // Python compares the split parts position by position; at a given
            // position both are the same kind, since re.split alternates.
            (Part::Num(_), Part::Text(_)) => Ordering::Less,
            (Part::Text(_), Part::Num(_)) => Ordering::Greater,
        }
    }
}

pub fn natural_key(name: &str) -> Vec<Part> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut digits = false;
    for ch in name.chars() {
        let d = ch.is_ascii_digit();
        if !cur.is_empty() && d != digits {
            out.push(if digits { Part::Num(cur.parse().unwrap_or(0)) } else { Part::Text(cur.clone()) });
            cur.clear();
        }
        digits = d;
        cur.push(ch);
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(if digits { Part::Num(cur.parse().unwrap_or(0)) } else { Part::Text(cur) });
    }
    // re.split(r"(\d+)") always starts with a text part, possibly empty.
    if matches!(out.first(), Some(Part::Num(_))) {
        out.insert(0, Part::Text(String::new()));
    }
    out
}

/// Entries of a directory, as full paths, filtered, in byte order (as Python's
/// sorted(glob(...)) gives).
pub fn list(dir: impl AsRef<Path>, keep: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = match fs::read_dir(dir.as_ref()) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| keep(&e.file_name().to_string_lossy()))
            .map(|e| e.path())
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

/// Like list, ordered by natural_key of the basename.
pub fn list_natural(dir: impl AsRef<Path>, keep: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let mut out = list(dir, keep);
    out.sort_by(|a, b| natural_key(&basename(a)).cmp(&natural_key(&basename(b))));
    out
}

pub fn names(dir: impl AsRef<Path>) -> Vec<String> {
    list(dir, |_| true).iter().map(basename).collect()
}

/// KEY=value lines (os-release, uevent), quotes stripped, in file order.
pub fn parse_kv(text: Option<&str>) -> Map<String, Value> {
    let mut out = Map::new();
    for line in text.unwrap_or("").lines() {
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            if !key.is_empty() && !key.starts_with('#') {
                let v = value.trim().trim_matches('"').trim_matches('\'');
                out.insert(key.to_string(), Value::String(v.to_string()));
            }
        }
    }
    out
}

/// '0-3,8,10-11' -> [0, 1, 2, 3, 8, 10, 11].
pub fn cpu_list(text: Option<&str>) -> Vec<i64> {
    let mut out = Vec::new();
    for part in text.unwrap_or("").split(',') {
        let part = part.trim();
        let (lo, hi) = part.split_once('-').unwrap_or((part, ""));
        if let Ok(lo) = lo.parse::<i64>() {
            let hi = if hi.is_empty() { lo } else { hi.parse().unwrap_or(lo) };
            out.extend(lo..=hi);
        }
    }
    out
}

pub fn which(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        return Some(PathBuf::from(name)).filter(|p| p.exists());
    }
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".into());
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

/// (exit success, stdout) of a tool, or None if it is missing or hangs.
fn run_inner(argv: &[&str], timeout: Duration) -> Option<(bool, String)> {
    let exe = which(argv[0])?;
    let mut child = Command::new(exe)
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let out = reader.join().ok()?;
    Some((status.success(), String::from_utf8_lossy(&out).into_owned()))
}

/// A tool's stdout, or None if it is missing, fails or hangs.
pub fn run(argv: &[&str]) -> Option<String> {
    run_inner(argv, Duration::from_secs(3)).filter(|(ok, _)| *ok).map(|(_, s)| s)
}

/// Like run, but keeps stdout whatever the exit code: ``systemctl
/// is-system-running`` says "degraded" and exits 1.
pub fn run_any(argv: &[&str]) -> Option<String> {
    run_inner(argv, Duration::from_secs(3)).map(|(_, s)| s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(bracketed(Some("always [madvise] never".into())), Some("madvise".into()));
        assert_eq!(cpu_list(Some("0-3,8,10-11")), vec![0, 1, 2, 3, 8, 10, 11]);
        assert!(cpu_list(Some("")).is_empty());
        assert_eq!(parse_int("08"), Some(8));
        assert_eq!(parse_int("0x10de"), Some(0x10de));
        assert_eq!(parse_int("-5"), Some(-5));
        assert_eq!(round(2.675, 2), 2.67); // as Python: the double is below .675
        assert_eq!(round(18.762, 3), 18.762);
        let mut v = vec!["hwmon10", "hwmon2", "hwmon1"];
        v.sort_by(|a, b| natural_key(a).cmp(&natural_key(b)));
        assert_eq!(v, vec!["hwmon1", "hwmon2", "hwmon10"]);
    }
}
