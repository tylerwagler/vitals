//! Every PCI device, named from pci.ids when the host has it, with its driver,
//! NUMA node, IOMMU group and link; and the version of each driver in use.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, BufReader};
use std::sync::Mutex;

use serde_json::{json, Map, Value};

use crate::sysfs::{basename, exists, link_name, list, read_int, read_text, realpath};

const PCI_ROOT: &str = "/sys/bus/pci/devices";
const PCI_IDS: &[&str] = &["/usr/share/misc/pci.ids", "/usr/share/hwdata/pci.ids", "/usr/share/pci.ids"];

fn class_name(base: i64) -> Option<&'static str> {
    Some(match base {
        0x00 => "unclassified", 0x01 => "storage", 0x02 => "network", 0x03 => "display",
        0x04 => "multimedia", 0x05 => "memory", 0x06 => "bridge", 0x07 => "communication",
        0x08 => "system", 0x09 => "input", 0x0a => "docking", 0x0b => "processor",
        0x0c => "serial bus", 0x0d => "wireless", 0x0e => "intelligent", 0x0f => "satellite",
        0x10 => "encryption", 0x11 => "signal processing", 0x12 => "accelerator",
        0x13 => "instrumentation", 0x40 => "coprocessor",
        _ => return None,
    })
}

fn is_pci_addr(s: &str) -> bool {
    // 0000:01:00.0
    let b = s.as_bytes();
    b.len() == 12
        && b[4] == b':'
        && b[7] == b':'
        && b[10] == b'.'
        && (b'0'..=b'7').contains(&b[11])
        && [0, 1, 2, 3, 5, 6, 8, 9].iter().all(|&i| b[i].is_ascii_hexdigit() && !b[i].is_ascii_uppercase())
}

type Names = HashMap<(String, Option<String>), String>;

/// Names for the (vendor, device) ids present, from one pass over pci.ids.
/// Keyed (vendor, None) for a vendor and (vendor, Some(device)) for a device.
/// The file is ~1.5 MB; only what this host has is kept, and the answer is
/// remembered for that set of ids.
fn pci_names(wanted: &BTreeSet<(String, String)>) -> Names {
    static MEMO: Mutex<Option<(BTreeSet<(String, String)>, Names)>> = Mutex::new(None);
    if let Some((ids, names)) = MEMO.lock().unwrap().as_ref() {
        if ids == wanted {
            return names.clone();
        }
    }
    let mut out = Names::new();
    if let Some(path) = PCI_IDS.iter().find(|p| exists(p)) {
        if let Ok(f) = std::fs::File::open(path) {
            let vendors: BTreeSet<&str> = wanted.iter().map(|(v, _)| v.as_str()).collect();
            let mut vendor: Option<String> = None;
            for line in BufReader::new(f).split(b'\n').map_while(Result::ok) {
                let line = String::from_utf8_lossy(&line);
                if line.starts_with("C ") {
                    break; // the class section; no more vendors
                }
                if line.trim().is_empty() || line.starts_with('#') {
                    continue;
                }
                if !line.starts_with('\t') {
                    let (vid, name) = line.split_once("  ").unwrap_or((&line, ""));
                    vendor = vendors.contains(vid).then(|| vid.to_string());
                    if let Some(v) = &vendor {
                        out.insert((v.clone(), None), name.trim().to_string());
                    }
                } else if let (Some(v), false) = (&vendor, line.starts_with("\t\t")) {
                    let (did, name) = line.trim().split_once("  ").unwrap_or((line.trim(), ""));
                    if wanted.contains(&(v.clone(), did.to_string())) {
                        out.insert((v.clone(), Some(did.to_string())), name.trim().to_string());
                    }
                }
            }
        }
    }
    *MEMO.lock().unwrap() = Some((wanted.clone(), out.clone()));
    out
}

fn hex4(path: std::path::PathBuf) -> Option<String> {
    let t = read_text(path)?;
    t.strip_prefix("0x").map(|h| h.to_lowercase())
}

pub fn devices() -> Vec<Map<String, Value>> {
    let mut rows = Vec::new();
    for dev in list(PCI_ROOT, |_| true) {
        let cls = read_int(dev.join("class"));
        let parent = realpath(&dev).and_then(|p| p.parent().map(basename)).unwrap_or_default();
        let link = if exists(dev.join("max_link_speed")) {
            json!({
                "current_speed": read_text(dev.join("current_link_speed")),
                "current_width": read_int(dev.join("current_link_width")),
                "max_speed": read_text(dev.join("max_link_speed")),
                "max_width": read_int(dev.join("max_link_width")),
            })
        } else {
            Value::Null
        };
        let mut row = Map::new();
        row.insert("address".into(), json!(basename(&dev)));
        row.insert("vendor_id".into(), json!(hex4(dev.join("vendor"))));
        row.insert("device_id".into(), json!(hex4(dev.join("device"))));
        row.insert("subsystem_vendor_id".into(), json!(hex4(dev.join("subsystem_vendor"))));
        row.insert("subsystem_device_id".into(), json!(hex4(dev.join("subsystem_device"))));
        row.insert("revision".into(), json!(hex4(dev.join("revision"))));
        row.insert("class_id".into(), json!(cls.map(|c| format!("{c:06x}"))));
        row.insert("class".into(), json!(cls.and_then(|c| class_name(c >> 16))));
        row.insert("driver".into(), json!(link_name(dev.join("driver"))));
        row.insert("numa_node".into(), json!(read_int(dev.join("numa_node"))));
        row.insert("iommu_group".into(), json!(link_name(dev.join("iommu_group"))));
        row.insert("parent".into(), json!(is_pci_addr(&parent).then_some(parent)));
        row.insert("link".into(), link);
        rows.push(row);
    }
    let wanted: BTreeSet<(String, String)> = rows
        .iter()
        .filter_map(|r| Some((r["vendor_id"].as_str()?.to_string(), r["device_id"].as_str()?.to_string())))
        .collect();
    let names = pci_names(&wanted);
    for r in rows.iter_mut() {
        let v = r["vendor_id"].as_str().map(String::from);
        let d = r["device_id"].as_str().map(String::from);
        let vendor = v.clone().and_then(|v| names.get(&(v, None)).cloned());
        let name = v.zip(d).and_then(|(v, d)| names.get(&(v, Some(d))).cloned());
        r.insert("vendor".into(), json!(vendor));
        r.insert("name".into(), json!(name));
    }
    rows
}

/// Each driver bound to a device, with its module's version where the module
/// has one (out-of-tree modules do; in-tree ones go with the kernel).
fn drivers(rows: &[Map<String, Value>]) -> Value {
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    for r in rows {
        if let Some(d) = r["driver"].as_str() {
            *counts.entry(d.to_string()).or_default() += 1;
        }
    }
    let mut out = Map::new();
    for (name, n) in counts {
        let module = link_name(format!("/sys/bus/pci/drivers/{name}/module")).unwrap_or_else(|| name.clone());
        out.insert(name, json!({
            "devices": n,
            "module": module,
            "version": read_text(format!("/sys/module/{module}/version")),
            "srcversion": read_text(format!("/sys/module/{module}/srcversion")),
        }));
    }
    Value::Object(out)
}

pub fn static_() -> Value {
    let rows = devices();
    let d = drivers(&rows);
    json!({"devices": rows, "drivers": d})
}
