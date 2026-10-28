//! Disks and filesystems: what is attached and mounted, how full it is, and
//! how busy.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::rates::Rates;
use crate::sysfs::{basename, bracketed, exists, is_dir, list, names, read_int, read_text, realpath, round};

const BLOCK_ROOT: &str = "/sys/block";
const SKIP_DISKS: &[&str] = &["loop", "ram", "fd", "sr"];
// Mounts that are not storage. Everything else, network filesystems included.
const PSEUDO_FS: &[&str] = &[
    "proc", "sysfs", "devtmpfs", "devpts", "tmpfs", "cgroup", "cgroup2", "securityfs",
    "pstore", "bpf", "tracefs", "debugfs", "mqueue", "hugetlbfs", "configfs", "fusectl",
    "autofs", "binfmt_misc", "rpc_pipefs", "efivarfs", "nsfs", "ramfs", "squashfs",
    "fuse.lxcfs", "fuse.portal", "fuse.gvfsd-fuse", "nfsd", "selinuxfs", "overlay",
];
const SKIP_MOUNT_PREFIXES: &[&str] = &["/proc", "/sys", "/run/credentials", "/snap/", "/var/lib/docker/"];
const NETWORK_FS: &[&str] = &["nfs", "nfs4", "cifs", "smb3", "ceph", "glusterfs", "9p", "fuse.sshfs", "fuse.rclone"];
const SECTOR: i64 = 512; // /proc/diskstats counts 512-byte sectors whatever the disk's

fn transport(name: &str) -> Option<&'static str> {
    let path = realpath(format!("{BLOCK_ROOT}/{name}")).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    for (needle, kind) in [("/nvme", "nvme"), ("/virtio", "virtio"), ("/usb", "usb"), ("/ata", "sata"), ("/host", "scsi"), ("/mmc", "mmc")] {
        if path.contains(needle) {
            return Some(kind);
        }
    }
    if name.starts_with("dm-") {
        Some("device-mapper")
    } else if name.starts_with("md") {
        Some("md-raid")
    } else if name.starts_with("zd") {
        Some("zvol")
    } else if name.starts_with("zram") {
        Some("zram")
    } else {
        None
    }
}

pub fn disks() -> Vec<String> {
    names(BLOCK_ROOT).into_iter().filter(|n| !SKIP_DISKS.iter().any(|s| n.starts_with(s))).collect()
}

fn block_static() -> Value {
    Value::Array(
        disks()
            .iter()
            .map(|name| {
                let base = format!("{BLOCK_ROOT}/{name}");
                let dev = format!("{base}/device");
                let size = read_int(format!("{base}/size"));
                let parts: Vec<Value> = list(&base, |n| n.starts_with(name.as_str()))
                    .iter()
                    .filter(|p| exists(p.join("partition")))
                    .map(|p| json!({"name": basename(p), "size_bytes": read_int(p.join("size")).map(|s| s * 512)}))
                    .collect();
                let flag = |f: &str| read_int(format!("{base}/{f}")).is_some_and(|v| v != 0);
                json!({
                    "name": name,
                    "dm_name": read_text(format!("{base}/dm/name")),
                    "model": read_text(format!("{dev}/model")),
                    "vendor": read_text(format!("{dev}/vendor")),
                    "serial": read_text(format!("{dev}/serial")).filter(|s| !s.is_empty()).or_else(|| read_text(format!("{base}/serial"))),
                    "firmware": read_text(format!("{dev}/firmware_rev")).filter(|s| !s.is_empty()).or_else(|| read_text(format!("{dev}/rev"))),
                    "transport": transport(name),
                    "size_bytes": size.map(|s| s * 512),
                    "rotational": flag("queue/rotational"),
                    "removable": flag("removable"),
                    "read_only": flag("ro"),
                    "logical_block_bytes": read_int(format!("{base}/queue/logical_block_size")),
                    "physical_block_bytes": read_int(format!("{base}/queue/physical_block_size")),
                    "scheduler": bracketed(read_text(format!("{base}/queue/scheduler"))),
                    "holders": if is_dir(format!("{base}/holders")) { names(format!("{base}/holders")) } else { vec![] },
                    "partitions": parts,
                })
            })
            .collect(),
    )
}

pub struct Mount {
    pub device: String,
    pub mountpoint: String,
    pub fstype: String,
    pub opts: String,
}

/// getmntent's unescaping of /proc/mounts: \040 is a space.
fn unescape(field: &str) -> String {
    let b = field.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// psutil.disk_partitions(all=True), less what is not storage.
pub fn mounts() -> Vec<Mount> {
    let text = read_text("/proc/self/mounts").or_else(|| read_text("/etc/mtab")).unwrap_or_default();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 4 {
            continue;
        }
        let device = unescape(f[0]);
        let m = Mount {
            device: if device == "none" { String::new() } else { device },
            mountpoint: unescape(f[1]),
            fstype: f[2].to_string(),
            opts: f[3].to_string(),
        };
        // An overlay is a container's root (Docker, OCI); anywhere else it is
        // a layer of someone else's.
        if (PSEUDO_FS.contains(&m.fstype.as_str()) && m.mountpoint != "/")
            || SKIP_MOUNT_PREFIXES.iter().any(|p| m.mountpoint.starts_with(p))
        {
            continue;
        }
        if !seen.insert(m.mountpoint.clone()) {
            continue;
        }
        out.push(m);
    }
    out
}

fn fs_static() -> Value {
    Value::Array(
        mounts()
            .iter()
            .map(|p| json!({"device": p.device, "mountpoint": p.mountpoint, "fstype": p.fstype,
                            "read_only": p.opts.split(',').any(|o| o == "ro"),
                            "network": NETWORK_FS.contains(&p.fstype.as_str())}))
            .collect(),
    )
}

#[derive(Clone, Copy)]
pub struct StatFs {
    blocks: u64,
    bfree: u64,
    bavail: u64,
    frsize: u64,
    files: u64,
    ffree: u64,
}

fn statvfs(path: &str) -> Option<StatFs> {
    let c = CString::new(path).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c is a valid C string and s is valid for writing.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    Some(StatFs {
        blocks: s.f_blocks as u64, bfree: s.f_bfree as u64, bavail: s.f_bavail as u64,
        frsize: s.f_frsize as u64, files: s.f_files as u64, ffree: s.f_ffree as u64,
    })
}

/// statvfs that cannot hang a sample.
///
/// A dead NFS server blocks statvfs indefinitely. Network mounts are asked on
/// a thread of their own and given half a second; one that does not answer
/// is reported unresponsive, and not asked again until that call returns.
#[derive(Default)]
pub struct Statvfs {
    stuck: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl Statvfs {
    const TIMEOUT: Duration = Duration::from_millis(500);

    /// (result, responsive)
    pub fn get(&self, path: &str, network: bool) -> (Option<StatFs>, bool) {
        if !network {
            return (statvfs(path), true);
        }
        {
            let mut stuck = self.stuck.lock().unwrap();
            if let Some(done) = stuck.get(path) {
                if !done.load(Ordering::Acquire) {
                    return (None, false);
                }
                stuck.remove(path);
            }
        }
        let (tx, rx) = mpsc::channel();
        let done = Arc::new(AtomicBool::new(false));
        let (p, d) = (path.to_string(), done.clone());
        let spawned = std::thread::Builder::new().name("vitals-statvfs".into()).spawn(move || {
            let _ = tx.send(statvfs(&p));
            d.store(true, Ordering::Release);
        });
        if spawned.is_err() {
            return (None, true);
        }
        match rx.recv_timeout(Self::TIMEOUT) {
            Ok(r) => (r, true),
            Err(_) => {
                self.stuck.lock().unwrap().insert(path.to_string(), done);
                (None, false)
            }
        }
    }
}

fn filesystems(statvfs: &Statvfs) -> Value {
    Value::Array(
        mounts()
            .iter()
            .map(|p| {
                let network = NETWORK_FS.contains(&p.fstype.as_str());
                let (st, responsive) = statvfs.get(&p.mountpoint, network);
                let mut row = Map::new();
                row.insert("mountpoint".into(), json!(p.mountpoint));
                row.insert("device".into(), json!(p.device));
                row.insert("fstype".into(), json!(p.fstype));
                if !responsive {
                    row.insert("responsive".into(), json!(false));
                }
                if let Some(st) = st.filter(|s| s.blocks != 0) {
                    let total = st.blocks * st.frsize;
                    let free = st.bavail * st.frsize;
                    let used = (st.blocks - st.bfree) * st.frsize;
                    row.insert("total_bytes".into(), json!(total));
                    row.insert("used_bytes".into(), json!(used));
                    row.insert("free_bytes".into(), json!(free));
                    // as df counts it: used over what a user could fill
                    row.insert("used_percent".into(),
                        if used + free != 0 { json!(round(100.0 * used as f64 / (used + free) as f64, 2)) } else { Value::Null });
                    row.insert("inodes_total".into(), if st.files != 0 { json!(st.files) } else { Value::Null });
                    row.insert("inodes_used".into(), if st.files != 0 { json!(st.files - st.ffree) } else { Value::Null });
                }
                Value::Object(row)
            })
            .collect(),
    )
}

fn disk_io(rates: &Rates) -> Value {
    let wanted: HashSet<String> = disks().into_iter().collect();
    let mut rows: Vec<(String, Vec<i64>)> = read_text("/proc/diskstats")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 14 || !wanted.contains(f[2]) {
                return None;
            }
            Some((f[2].to_string(), f[3..].iter().filter_map(|v| v.parse().ok()).collect()))
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    Value::Array(
        rows.iter()
            .filter(|(_, f)| f.len() >= 10)
            .map(|(name, f)| {
                let key = format!("disk/{name}");
                let (reads, rsec, writes, wsec, busy) = (f[0], f[2], f[4], f[6], f[9]);
                let (rb, wb) = (rsec * SECTOR, wsec * SECTOR);
                let busy = rates.rate_i(&format!("{key}/busy"), Some(busy), 3);
                json!({
                    "disk": name,
                    "read_bytes": rb, "write_bytes": wb, "reads": reads, "writes": writes,
                    "read_bytes_per_s": rates.rate_i(&format!("{key}/rb"), Some(rb), 1),
                    "write_bytes_per_s": rates.rate_i(&format!("{key}/wb"), Some(wb), 1),
                    "reads_per_s": rates.rate_i(&format!("{key}/r"), Some(reads), 2),
                    "writes_per_s": rates.rate_i(&format!("{key}/w"), Some(writes), 2),
                    // busy_time is milliseconds: per second of wall time, as a percent
                    "busy_percent": busy.map(|b| round((b / 10.0).min(100.0), 2)),
                })
            })
            .collect(),
    )
}

pub fn static_() -> Value {
    json!({"block_devices": block_static(), "filesystems": fs_static()})
}

pub fn live(rates: &Rates, statvfs: &Statvfs) -> Value {
    json!({"filesystems": filesystems(statvfs), "disks": disk_io(rates)})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_mount_fields() {
        assert_eq!(unescape("/mnt/my\\040disk"), "/mnt/my disk");
        assert_eq!(unescape("/plain"), "/plain");
    }

    #[test]
    fn local_statvfs_is_direct() {
        let (r, responsive) = Statvfs::default().get("/", false);
        assert!(responsive && r.unwrap().blocks > 0);
    }
}
