//! vitals: one host's hardware, OS and live state as JSON over HTTP, with a
//! Server-Sent Events stream. Serve the API, or print one reading and exit.

mod collect;
mod hub;
mod legacy;
mod os;
mod prometheus;
mod rates;
mod server;
mod sysfs;

use std::net::TcpListener;
use std::process::ExitCode;
use std::time::Duration;

use serde_json::{json, Value};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "usage: vitals [--host HOST] [--port PORT] [--max-streams N] [--once [WHAT]] [-v] [-V]

One host's hardware, OS and live state as JSON over HTTP.

  --host HOST        listen address (default 0.0.0.0)
  --port PORT        listen port (default 9420)
  --max-streams N    concurrent /v1/stream subscribers (default 8)
  --once [WHAT]      print one reading and exit: all (default), static, live, processes, metrics
  -v, --verbose      log every request
  -V, --version      print the version";

struct Args {
    host: String,
    port: u16,
    max_streams: usize,
    keepalive: f64,
    once: Option<String>,
    verbose: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { host: "0.0.0.0".into(), port: 9420, max_streams: 8, keepalive: 15.0, once: None, verbose: false };
    let mut it = std::env::args().skip(1).peekable();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| inline.clone().or_else(|| it.next()).ok_or(format!("{name} needs a value"));
        match flag.as_str() {
            "--host" => a.host = value("--host")?,
            "--port" => a.port = value("--port")?.parse().map_err(|_| "--port must be a number")?,
            "--max-streams" => a.max_streams = value("--max-streams")?.parse().map_err(|_| "--max-streams must be a number")?,
            // tests shorten the keepalive; not in the usage
            "--keepalive" => a.keepalive = value("--keepalive")?.parse().map_err(|_| "--keepalive must be a number")?,
            "--once" => {
                let what = inline.clone().or_else(|| {
                    it.next_if(|n| matches!(n.as_str(), "all" | "static" | "live" | "processes" | "metrics"))
                });
                a.once = Some(what.unwrap_or_else(|| "all".into()));
            }
            // Accepted so a unit written for the reference 0.6.0 starts this one
            // unchanged; every GPU process is reported and every interface is read.
            "--process-limit" | "--net" | "--token" => {
                value(&flag)?;
            }
            "-v" | "--verbose" => a.verbose = true,
            "-V" | "--version" => {
                println!("vitals {VERSION}");
                std::process::exit(0);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if let Some(w) = &a.once {
        if !matches!(w.as_str(), "all" | "static" | "live" | "processes" | "metrics") {
            return Err(format!("--once: unknown reading {w}"));
        }
    }
    Ok(a)
}

fn once(hub: &std::sync::Arc<hub::Hub>, what: &str) -> Value {
    match what {
        "metrics" => {
            let ex = legacy::Exporter::new(hub.clone());
            std::thread::sleep(Duration::from_millis(200)); // give the CPU counters an interval
            ex.snapshot()
        }
        "processes" => hub.process_table(25, "cpu"),
        _ => {
            let mut out = serde_json::Map::new();
            if what == "all" || what == "static" {
                out.insert("static".into(), (*hub.static_doc(false)).clone());
            }
            if what == "all" || what == "live" {
                hub.take(); // the first sample has no rates; the second does
                std::thread::sleep(Duration::from_millis(500));
                out.insert("live".into(), (*hub.take()).clone());
            }
            if what == "all" { Value::Object(out) } else { out.remove(what).unwrap_or(json!(null)) }
        }
    }
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("vitals: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    // A collector bug is reported once, as that section's error, not as a
    // backtrace on every sample.
    std::panic::set_hook(Box::new(|info| {
        if std::env::var_os("VITALS_DEBUG").is_some() {
            eprintln!("PANIC {info}");
        }
    }));
    os::mono();
    let hub = hub::Hub::new();
    if let Some(what) = &args.once {
        println!("{}", serde_json::to_string_pretty(&once(&hub, what)).unwrap_or_default());
        return ExitCode::SUCCESS;
    }
    let listener = match TcpListener::bind((args.host.as_str(), args.port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("vitals: cannot listen on {}:{}: {e}", args.host, args.port);
            return ExitCode::from(1);
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(args.port);
    *hub.listen.lock().unwrap() = Some(format!("{}:{port}", args.host));
    eprintln!("INFO vitals {VERSION} on http://{}:{port}/ (stream: /v1/stream)", args.host);
    let server = server::Server::new(hub, args.max_streams, args.keepalive, args.verbose);
    server.serve(listener);
    ExitCode::SUCCESS
}
