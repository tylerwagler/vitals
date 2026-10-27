//! RAM, swap and the kernel's memory policy; and pressure stall information
//! for every resource the kernel reports it for.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::rates::Rates;
use crate::sysfs::{bracketed, list, read_int, read_text, round};

const VMSTAT_RATES: &[&str] = &["pgfault", "pgmajfault", "pswpin", "pswpout", "pgpgin", "pgpgout",
    "pgsteal_kswapd", "pgsteal_direct", "workingset_refault_file"];
const PSI_RESOURCES: &[&str] = &["cpu", "memory", "io", "irq"];

/// 'SReclaimable' -> 'sreclaimable', 'Active(anon)' -> 'active_anon',
/// 'HugePages_Total' -> 'huge_pages_total'.
pub fn snake(key: &str) -> String {
    let key = key.replace('(', "_").replace(')', "");
    let chars: Vec<char> = key.chars().collect();
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && c.is_ascii_uppercase() {
            let p = chars[i - 1];
            if p.is_ascii_lowercase() || p.is_ascii_digit() {
                out.push('_');
            }
        }
        out.push(c.to_ascii_lowercase());
    }
    let mut collapsed = String::new();
    for c in out.chars() {
        if c == '_' && collapsed.ends_with('_') {
            continue;
        }
        collapsed.push(c);
    }
    collapsed.trim_matches('_').to_string()
}

/// Every /proc/meminfo line: kB fields as *_bytes, counts as they are.
pub fn meminfo(text: Option<&str>) -> Map<String, Value> {
    let owned;
    let text = match text {
        Some(t) => t,
        None => {
            owned = read_text("/proc/meminfo").unwrap_or_default();
            &owned
        }
    };
    let mut out = Map::new();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else { continue };
        let parts: Vec<&str> = rest.split_whitespace().collect();
        let Some(n) = parts.first().filter(|p| p.bytes().all(|b| b.is_ascii_digit())).and_then(|p| p.parse::<i64>().ok()) else {
            continue;
        };
        if parts.get(1) == Some(&"kB") {
            out.insert(format!("{}_bytes", snake(key)), json!(n * 1024));
        } else {
            out.insert(snake(key), json!(n));
        }
    }
    out
}

fn vmstat() -> HashMap<String, i64> {
    read_text("/proc/vmstat")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(' ')?;
            Some((k.to_string(), v.trim().parse().ok()?))
        })
        .collect()
}

fn swaps() -> Value {
    let text = read_text("/proc/swaps").unwrap_or_default();
    Value::Array(
        text.lines()
            .skip(1)
            .filter_map(|l| {
                let p: Vec<&str> = l.split_whitespace().collect();
                if p.len() < 5 {
                    return None;
                }
                Some(json!({
                    "device": p[0], "type": p[1],
                    "size_bytes": p[2].parse::<i64>().ok()? * 1024,
                    "used_bytes": p[3].parse::<i64>().ok()? * 1024,
                    "priority": p[4].parse::<i64>().ok()?,
                }))
            })
            .collect(),
    )
}

pub fn static_() -> Value {
    let info = meminfo(None);
    let mut sizes: Vec<i64> = list("/sys/kernel/mm/hugepages", |n| n.starts_with("hugepages-") && n.ends_with("kB"))
        .iter()
        .filter_map(|p| {
            let n = crate::sysfs::basename(p);
            n["hugepages-".len()..n.len() - 2].parse::<i64>().ok().map(|k| k * 1024)
        })
        .collect();
    sizes.sort();
    json!({
        "total_bytes": info.get("mem_total_bytes"),
        "swap_total_bytes": info.get("swap_total_bytes"),
        "huge_page_sizes_bytes": sizes,
        "default_huge_page_bytes": info.get("hugepagesize_bytes"),
        "transparent_hugepages": bracketed(read_text("/sys/kernel/mm/transparent_hugepage/enabled")),
        "transparent_hugepages_defrag": bracketed(read_text("/sys/kernel/mm/transparent_hugepage/defrag")),
        "swappiness": read_int("/proc/sys/vm/swappiness"),
        "overcommit_memory": read_int("/proc/sys/vm/overcommit_memory"),
        "overcommit_ratio": read_int("/proc/sys/vm/overcommit_ratio"),
        "min_free_bytes": read_int("/proc/sys/vm/min_free_kbytes").map(|k| k * 1024),
        "zswap": read_text("/sys/module/zswap/parameters/enabled"),
        "ksm_running": read_int("/sys/kernel/mm/ksm/run"),
        "swap_devices": swaps(),
    })
}

pub fn live(rates: &Rates) -> Value {
    let info = meminfo(None);
    let g = |k: &str| info.get(k).and_then(|v| v.as_i64());
    let (total, avail) = (g("mem_total_bytes"), g("mem_available_bytes"));
    let (swap_total, swap_free) = (g("swap_total_bytes"), g("swap_free_bytes"));
    let vm = vmstat();
    // total - available, not total - free: page cache the kernel (or, on a
    // UMA GPU, the driver) can reclaim is not memory in use.
    let used = total.zip(avail).map(|(t, a)| t - a);
    let swap_used = swap_total.zip(swap_free).map(|(t, f)| t - f);
    let pct = |u: Option<i64>, t: Option<i64>| match (u, t) {
        (Some(u), Some(t)) if t != 0 => json!(round(100.0 * u as f64 / t as f64, 2)),
        _ => Value::Null,
    };
    let mut per_s = Map::new();
    for k in VMSTAT_RATES {
        if let Some(&v) = vm.get(*k) {
            per_s.insert(k.to_string(), json!(rates.rate_i(&format!("vm/{k}"), Some(v), 1)));
        }
    }
    json!({
        "total_bytes": total,
        "used_bytes": used,
        "available_bytes": avail,
        "free_bytes": g("mem_free_bytes"),
        "cached_bytes": g("cached_bytes"),
        "buffers_bytes": g("buffers_bytes"),
        "used_percent": pct(used, total),
        "swap_total_bytes": swap_total,
        "swap_used_bytes": swap_used,
        "swap_used_percent": pct(swap_used, swap_total),
        "huge_pages": {"total": g("huge_pages_total"), "free": g("huge_pages_free"), "reserved": g("huge_pages_rsvd")},
        "rates_per_s": per_s,
        "oom_kills": vm.get("oom_kill"),
        "meminfo": info,
    })
}

// -- pressure ---------------------------------------------------------------

/// sparkview's memory pressure levels, from avg10 percentages.
pub fn psi_level(some: f64, full: f64) -> &'static str {
    if full > 0.10 || some > 0.30 {
        "CRITICAL"
    } else if full > 0.05 || some > 0.15 {
        "HIGH"
    } else if some > 0.05 {
        "MOD"
    } else {
        "LOW"
    }
}

/// 'some avg10=0.00 avg60=0.00 avg300=0.00 total=123' lines, parsed.
pub fn psi(text: &str) -> Map<String, Value> {
    let mut out = Map::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(kind) = fields.next() else { continue };
        let mut values = Map::new();
        for f in fields {
            if let Some((k, v)) = f.split_once('=') {
                let value = if k == "total" { v.parse::<i64>().ok().map(Value::from) } else { v.parse::<f64>().ok().map(Value::from) };
                values.insert(k.to_string(), value.unwrap_or(Value::Null));
            }
        }
        out.insert(kind.to_string(), Value::Object(values));
    }
    out
}

pub fn pressure(rates: &Rates, root: &str) -> Value {
    let mut out = Map::new();
    for resource in PSI_RESOURCES {
        let Some(text) = read_text(format!("{root}/{resource}")) else { continue };
        let mut parsed = psi(&text);
        for (kind, values) in parsed.iter_mut() {
            let total = values.get("total").and_then(|t| t.as_i64());
            // total is cumulative stall in microseconds: as a rate, the share
            // of the interval spent stalled, which avg10 smooths away.
            let stalled = rates.rate_i(&format!("psi/{resource}/{kind}"), total, 1);
            if let Value::Object(v) = values {
                v.insert("stalled_percent".into(), json!(stalled.map(|s| round(s / 1e4, 3))));
            }
        }
        if *resource == "memory" {
            let avg10 = |k: &str| parsed.get(k).and_then(|v| v.get("avg10")).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let level = psi_level(avg10("some"), avg10("full"));
            parsed.insert("level".into(), json!(level));
        }
        out.insert(resource.to_string(), Value::Object(parsed));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_names_and_units() {
        let info = meminfo(Some("MemTotal:  16 kB\nActive(anon):  2 kB\nHugePages_Total:  4\nSReclaimable: 1 kB\n"));
        assert_eq!(Value::Object(info), json!({"mem_total_bytes": 16384, "active_anon_bytes": 2048,
            "huge_pages_total": 4, "sreclaimable_bytes": 1024}));
    }

    #[test]
    fn psi_levels() {
        assert_eq!(psi_level(0.0, 0.0), "LOW");
        assert_eq!(psi_level(0.10, 0.0), "MOD");
        assert_eq!(psi_level(0.20, 0.0), "HIGH");
        assert_eq!(psi_level(0.0, 0.2), "CRITICAL");
    }
}
