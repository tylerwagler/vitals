//! Network interfaces, sockets and TCP, and RDMA ports.

use std::collections::HashMap;
use std::ffi::CStr;
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::rates::Rates;
use crate::sysfs::{basename, exists, is_dir, link_name, list, names, read_int, read_text, round};

const NET_ROOT: &str = "/sys/class/net";
pub const IB_ROOT: &str = "/sys/class/infiniband";
const NET_COUNTERS: &[&str] = &["rx_bytes", "tx_bytes", "rx_packets", "tx_packets",
    "rx_errors", "tx_errors", "rx_dropped", "tx_dropped", "collisions", "multicast"];
const RATED: &[&str] = &["rx_bytes", "tx_bytes", "rx_packets", "tx_packets", "rx_errors", "tx_errors", "rx_dropped", "tx_dropped"];
// /proc/net/dev's sixteen columns, receive then transmit.
const PROC_NET_DEV: &[&str] = &["rx_bytes", "rx_packets", "rx_errors", "rx_dropped", "rx_fifo", "rx_frame",
    "rx_compressed", "multicast", "tx_bytes", "tx_packets", "tx_errors",
    "tx_dropped", "tx_fifo", "collisions", "tx_carrier", "tx_compressed"];
const TCP_COUNTERS: &[&str] = &["ActiveOpens", "PassiveOpens", "AttemptFails", "EstabResets",
    "InSegs", "OutSegs", "RetransSegs", "InErrs", "OutRsts"];

// The port counters pulsar-gui sums into "link errors", as the reference
// exporter ships them. Counters a port does not have are skipped.
pub const RDMA_ERRORS: &[&str] = &["symbol_error", "link_downed", "link_error_recovery",
    "port_rcv_errors", "port_xmit_discards", "port_xmit_wait"];
pub const RDMA_HW_COUNTERS: &[&str] = &[
    "out_of_sequence", "packet_seq_err", "duplicate_request", "implied_nak_seq_err",
    "local_ack_timeout_err", "req_transport_retries_exceeded", "rnr_nak_retry_err",
    "roce_adp_retrans", "roce_adp_retrans_to", "roce_slow_restart", "out_of_buffer",
    "np_ecn_marked_roce_packets", "np_cnp_sent", "rp_cnp_handled",
];

// -- interfaces -------------------------------------------------------------

fn iface_kind(path: &Path) -> String {
    let name = basename(path);
    if name == "lo" {
        return "loopback".into();
    }
    if is_dir(path.join("bridge")) {
        return "bridge".into();
    }
    if is_dir(path.join("bonding")) {
        return "bond".into();
    }
    if exists(path.join("device")) {
        return if is_dir(path.join("wireless")) { "wireless" } else { "physical" }.into();
    }
    let uevent = read_text(path.join("uevent")).unwrap_or_default();
    uevent
        .lines()
        .find_map(|l| l.strip_prefix("DEVTYPE=").map(|t| t.split_whitespace().next().unwrap_or("").to_string()))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "virtual".into()) // vlan, wireguard, ...; veth, tap
}

fn interfaces() -> Vec<std::path::PathBuf> {
    list(NET_ROOT, |_| true)
}

fn prefix_len(addr: *const libc::sockaddr) -> Option<u32> {
    if addr.is_null() {
        return None;
    }
    // SAFETY: getifaddrs gives sockaddrs of the family they say.
    unsafe {
        match (*addr).sa_family as i32 {
            libc::AF_INET => Some(u32::from_be((*(addr as *const libc::sockaddr_in)).sin_addr.s_addr).count_ones()),
            libc::AF_INET6 => Some((*(addr as *const libc::sockaddr_in6)).sin6_addr.s6_addr.iter().map(|b| b.count_ones()).sum()),
            _ => None,
        }
    }
}

/// psutil.net_if_addrs(): each interface's IPv4 and IPv6 addresses.
fn addresses() -> HashMap<String, Vec<Value>> {
    let mut out: HashMap<String, Vec<Value>> = HashMap::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs allocates a list we walk and then free.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return out;
    }
    let mut cur = head;
    while !cur.is_null() {
        // SAFETY: cur is a node of the list getifaddrs returned.
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        if ifa.ifa_addr.is_null() {
            continue;
        }
        let name = unsafe { CStr::from_ptr(ifa.ifa_name) }.to_string_lossy().into_owned();
        let family = unsafe { (*ifa.ifa_addr).sa_family } as i32;
        let (fam, addr) = match family {
            libc::AF_INET => {
                let a = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
                ("ipv4", std::net::Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr)).to_string())
            }
            libc::AF_INET6 => {
                let a = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in6) };
                ("ipv6", std::net::Ipv6Addr::from(a.sin6_addr.s6_addr).to_string())
            }
            _ => continue,
        };
        out.entry(name).or_default().push(json!({"family": fam, "address": addr, "prefix": prefix_len(ifa.ifa_netmask)}));
    }
    // SAFETY: head came from getifaddrs.
    unsafe { libc::freeifaddrs(head) };
    out
}

fn iface_static() -> Value {
    let addrs = addresses();
    Value::Array(
        interfaces()
            .iter()
            .map(|path| {
                let name = basename(path);
                let speed = read_int(path.join("speed"));
                json!({
                    "iface": name,
                    "kind": iface_kind(path),
                    "mac": read_text(path.join("address")),
                    "mtu": read_int(path.join("mtu")),
                    "driver": link_name(path.join("device/driver")),
                    "pci": if exists(path.join("device")) { link_name(path.join("device")) } else { None },
                    "speed_gbps": speed.filter(|s| *s > 0).map(|s| s as f64 / 1000.0),
                    "duplex": read_text(path.join("duplex")),
                    "master": link_name(path.join("master")),
                    "members": if is_dir(path.join("brif")) { Some(names(path.join("brif"))) } else { None },
                    "addresses": addrs.get(&name).cloned().unwrap_or_default(),
                })
            })
            .collect(),
    )
}

/// Every interface's counters from one file, instead of ten sysfs files each.
pub fn proc_net_dev(text: Option<&str>) -> HashMap<String, HashMap<&'static str, i64>> {
    let owned;
    let text = match text {
        Some(t) => t,
        None => {
            owned = read_text("/proc/net/dev").unwrap_or_default();
            &owned
        }
    };
    let mut out = HashMap::new();
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else { continue };
        let fields: Vec<i64> = rest.split_whitespace().filter_map(|f| f.parse().ok()).collect();
        if fields.len() >= PROC_NET_DEV.len() {
            out.insert(name.trim().to_string(), PROC_NET_DEV.iter().copied().zip(fields).collect());
        }
    }
    out
}

fn iface_live(rates: &Rates) -> Value {
    let counters = proc_net_dev(None);
    let empty = HashMap::new();
    Value::Array(
        interfaces()
            .iter()
            .map(|path| {
                let name = basename(path);
                let mut row = Map::new();
                row.insert("iface".into(), json!(name));
                row.insert("state".into(), json!(read_text(path.join("operstate")).unwrap_or_else(|| "unknown".into()).to_uppercase()));
                let stats = counters.get(&name).unwrap_or(&empty);
                for c in NET_COUNTERS {
                    let value = stats.get(c).copied();
                    row.insert(c.to_string(), json!(value));
                    if RATED.contains(c) {
                        row.insert(format!("{c}_per_s"), json!(rates.rate_i(&format!("net/{name}/{c}"), value, 1)));
                    }
                }
                let bits = |k: &str| row.get(k).and_then(|v| v.as_f64()).map(|b| b * 8.0);
                let (rx, tx) = (bits("rx_bytes_per_s"), bits("tx_bytes_per_s"));
                row.insert("rx_bits_per_s".into(), json!(rx));
                row.insert("tx_bits_per_s".into(), json!(tx));
                Value::Object(row)
            })
            .collect(),
    )
}

/// /proc/net/sockstat: 'TCP: inuse 5 orphan 0 tw 2 alloc 7 mem 1'.
fn sockstat() -> Value {
    let mut out = Map::new();
    for path in ["/proc/net/sockstat", "/proc/net/sockstat6"] {
        for line in read_text(path).unwrap_or_default().lines() {
            let (proto, rest) = line.split_once(':').unwrap_or((line, ""));
            let f: Vec<&str> = rest.split_whitespace().collect();
            let mut m = Map::new();
            for pair in f.chunks(2) {
                if let [k, v] = pair {
                    if let Ok(n) = v.parse::<i64>() {
                        m.insert(k.to_string(), json!(n));
                    }
                }
            }
            out.insert(proto.trim().to_lowercase(), Value::Object(m));
        }
    }
    Value::Object(out)
}

fn snake(name: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = name.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && c.is_ascii_uppercase() && chars[i - 1].is_ascii_lowercase() {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

fn tcp(rates: &Rates) -> Value {
    let lines: Vec<Vec<String>> = read_text("/proc/net/snmp")
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("Tcp:"))
        .map(|l| l.split_whitespace().map(String::from).collect())
        .collect();
    if lines.len() < 2 {
        return Value::Null;
    }
    let values: HashMap<&str, i64> = lines[0][1..].iter().map(|s| s.as_str()).zip(lines[1][1..].iter().filter_map(|v| v.parse().ok())).collect();
    let mut out = Map::new();
    out.insert("established".into(), json!(values.get("CurrEstab")));
    for c in TCP_COUNTERS {
        if let Some(&v) = values.get(c) {
            out.insert(format!("{}_per_s", snake(c)), json!(rates.rate_i(&format!("tcp/{c}"), Some(v), 2)));
        }
    }
    let segs = out.get("out_segs_per_s").and_then(|v| v.as_f64());
    let retrans = out.get("retrans_segs_per_s").and_then(|v| v.as_f64());
    out.insert("retransmit_percent".into(), match (segs, retrans) {
        (Some(s), Some(r)) if s != 0.0 => json!(round(100.0 * r / s, 3)),
        _ => Value::Null,
    });
    Value::Object(out)
}

// -- RDMA -------------------------------------------------------------------

/// '4: ACTIVE' -> 'ACTIVE'.
fn after_colon(text: Option<String>) -> Option<String> {
    text.map(|t| t.split_once(':').map(|(_, r)| r).unwrap_or(&t).trim().to_string())
}

pub type NetlinkCounters = HashMap<(String, i64), HashMap<String, i64>>;

/// Every port's hw counters from one ``rdma statistic`` netlink query.
///
/// Each read of a sysfs hw_counters file is its own firmware query, 2-3 ms
/// apiece, which made the RDMA section most of a scrape. None when the tool
/// is missing or fails, and the sysfs files are read instead.
pub fn rdma_netlink_counters() -> Option<NetlinkCounters> {
    if names(IB_ROOT).is_empty() {
        return None;
    }
    let out = crate::sysfs::run(&["rdma", "-j", "statistic", "show"])?;
    let rows: Value = serde_json::from_str(&out).ok()?;
    let rows = rows.as_array()?;
    Some(
        rows.iter()
            .filter_map(|r| {
                let r = r.as_object()?;
                let port = r.get("port")?.as_i64()?;
                let name = r.get("ifname")?.as_str()?.to_string();
                Some(((name, port), r.iter().filter_map(|(k, v)| Some((k.clone(), v.as_i64()?))).collect()))
            })
            .collect(),
    )
}

fn hw_counters(sysfs_dir: &Path, netlink: Option<&HashMap<String, i64>>) -> Map<String, Value> {
    let mut out = Map::new();
    for k in RDMA_HW_COUNTERS {
        let v = match netlink {
            Some(n) => n.get(*k).copied(),
            None => read_int(sysfs_dir.join(k)),
        };
        if let Some(v) = v {
            out.insert(k.to_string(), json!(v));
        }
    }
    out
}

pub fn rdma(root: &str, hw: Option<&NetlinkCounters>) -> Vec<Value> {
    let mut out = Vec::new();
    for dev in list(root, |_| true) {
        let dev_name = basename(&dev);
        let nets = if is_dir(dev.join("device/net")) { names(dev.join("device/net")) } else { vec![] };
        for port_dir in list(dev.join("ports"), |_| true) {
            let Ok(port_no) = basename(&port_dir).parse::<i64>() else { continue };
            let ndev = read_text(port_dir.join("gid_attrs/ndevs/0")).filter(|n| !n.is_empty());
            let rate = read_text(port_dir.join("rate")).unwrap_or_default();
            let rate_gbps = rate.split_whitespace().next().filter(|_| rate.contains("Gb/sec")).and_then(|r| r.parse::<f64>().ok());
            let counters = port_dir.join("counters");
            let words = |n: &str| read_int(counters.join(n)).map(|v| v * 4);
            let mut errors = Map::new();
            for k in RDMA_ERRORS {
                if let Some(v) = read_int(counters.join(k)) {
                    errors.insert(k.to_string(), json!(v));
                }
            }
            let netdev = ndev.or_else(|| usize::try_from(port_no - 1).ok().and_then(|i| nets.get(i).cloned()).filter(|_| port_no > 0));
            out.push(json!({
                "device": dev_name,
                "port": port_no,
                "netdev": netdev,
                "state": after_colon(read_text(port_dir.join("state"))),
                "phys_state": after_colon(read_text(port_dir.join("phys_state"))),
                "rate_gbps": rate_gbps,
                "link_layer": read_text(port_dir.join("link_layer")),
                "tx_bytes": words("port_xmit_data"),
                "rx_bytes": words("port_rcv_data"),
                "tx_packets": read_int(counters.join("port_xmit_packets")),
                "rx_packets": read_int(counters.join("port_rcv_packets")),
                "errors": errors,
                "hw_counters": hw_counters(&port_dir.join("hw_counters"), hw.and_then(|h| h.get(&(dev_name.clone(), port_no)))),
            }));
        }
    }
    out
}

fn rdma_static() -> Value {
    Value::Array(
        list(IB_ROOT, |_| true)
            .iter()
            .map(|dev| {
                let ports: Vec<Value> = list(dev.join("ports"), |_| true)
                    .iter()
                    .map(|p| json!({"port": basename(p), "link_layer": read_text(p.join("link_layer")), "gid0": read_text(p.join("gids/0"))}))
                    .collect();
                json!({
                    "device": basename(dev),
                    "node_type": after_colon(read_text(dev.join("node_type"))),
                    "hca_type": read_text(dev.join("hca_type")),
                    "board_id": read_text(dev.join("board_id")),
                    "firmware": read_text(dev.join("fw_ver")),
                    "node_guid": read_text(dev.join("node_guid")),
                    "pci": link_name(dev.join("device")),
                    "driver": link_name(dev.join("device/driver")),
                    "netdevs": if is_dir(dev.join("device/net")) { names(dev.join("device/net")) } else { vec![] },
                    "ports": ports,
                })
            })
            .collect(),
    )
}

pub fn rdma_live(rates: &Rates) -> Value {
    let mut ports = rdma(IB_ROOT, rdma_netlink_counters().as_ref());
    for p in ports.iter_mut() {
        let key = format!("rdma/{}/{}", p["device"].as_str().unwrap_or(""), p["port"]);
        let Value::Object(m) = p else { continue };
        for c in ["tx_bytes", "rx_bytes", "tx_packets", "rx_packets"] {
            let r = rates.rate_i(&format!("{key}/{c}"), m.get(c).and_then(|v| v.as_i64()), 1);
            m.insert(format!("{c}_per_s"), json!(r));
        }
        let bits = |k: &str| m.get(k).and_then(|v| v.as_f64()).map(|b| b * 8.0);
        let (tx, rx) = (bits("tx_bytes_per_s"), bits("rx_bytes_per_s"));
        m.insert("tx_bits_per_s".into(), json!(tx));
        m.insert("rx_bits_per_s".into(), json!(rx));
    }
    Value::Array(ports)
}

pub fn static_() -> Value {
    json!({"interfaces": iface_static(), "rdma": rdma_static()})
}

pub fn live(rates: &Rates) -> Value {
    json!({"interfaces": iface_live(rates), "sockets": sockstat(), "tcp": tcp(rates)})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn proc_net_dev_reads_every_interface_from_one_file() {
        let text = "Inter-|   Receive |  Transmit\n face |bytes packets errs drop fifo frame compressed multicast|...\n  eth0: 1000 10 1 2 0 0 0 3 2000 20 4 5 0 6 0 0\n    lo: 5 1 0 0 0 0 0 0 5 1 0 0 0 0 0 0\n";
        let out = proc_net_dev(Some(text));
        assert_eq!(out["eth0"]["rx_bytes"], 1000);
        assert_eq!(out["eth0"]["tx_bytes"], 2000);
        assert_eq!(out["eth0"]["multicast"], 3);
        assert_eq!(out["eth0"]["collisions"], 6);
        assert_eq!(out["eth0"]["tx_dropped"], 5);
        assert_eq!(out.len(), 2);
    }

    fn port(root: &Path) -> std::path::PathBuf {
        let p = root.join("rocex/ports/1");
        fs::create_dir_all(p.join("counters")).unwrap();
        fs::create_dir_all(p.join("hw_counters")).unwrap();
        fs::create_dir_all(p.join("gid_attrs/ndevs")).unwrap();
        p
    }

    #[test]
    fn rdma_reads_words_and_states() {
        let root = std::env::temp_dir().join(format!("vitals-rdma-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let p = port(&root);
        for (f, v) in [("state", "4: ACTIVE"), ("phys_state", "5: LinkUp"), ("rate", "200 Gb/sec (4X HDR)"),
                       ("link_layer", "Ethernet"), ("gid_attrs/ndevs/0", "eth7"), ("counters/port_xmit_data", "100"),
                       ("counters/symbol_error", "2"), ("hw_counters/out_of_sequence", "3")] {
            fs::write(p.join(f), format!("{v}\n")).unwrap();
        }
        let rows = rdma(root.to_str().unwrap(), None);
        let row = &rows[0];
        assert_eq!((row["device"].as_str(), row["port"].as_i64(), row["netdev"].as_str()), (Some("rocex"), Some(1), Some("eth7")));
        assert_eq!((row["state"].as_str(), row["phys_state"].as_str(), row["rate_gbps"].as_f64()), (Some("ACTIVE"), Some("LinkUp"), Some(200.0)));
        assert_eq!(row["tx_bytes"], 400);
        assert_eq!(row["rx_bytes"], Value::Null);
        assert_eq!(row["errors"], json!({"symbol_error": 2}));
        assert_eq!(row["hw_counters"], json!({"out_of_sequence": 3}));
        let mut netlink = NetlinkCounters::new();
        netlink.insert(("rocex".into(), 1), [("out_of_sequence".to_string(), 9), ("rx_write_requests".to_string(), 5)].into_iter().collect());
        assert_eq!(rdma(root.to_str().unwrap(), Some(&netlink))[0]["hw_counters"], json!({"out_of_sequence": 9}));
        // netlink knew nothing of this port: the sysfs files are read instead
        assert_eq!(rdma(root.to_str().unwrap(), Some(&NetlinkCounters::new()))[0]["hw_counters"], json!({"out_of_sequence": 3}));
        let _ = fs::remove_dir_all(&root);
    }
}
