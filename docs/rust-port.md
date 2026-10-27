# Scope: porting vitals to Rust

Status: **done 2026-10-27** (branch `rust-port`). `rust/` is at parity with the Python reference
on the VM, Sparky and CT 124: `scripts/compare.py` finds no differences, and the API suite passes
against the binary on all three. **Deployed 2026-10-27** (see *Deployment*). The scope as written
on 2026-10-26 follows the results.

## Deployment (2026-10-27)

The exporter the hosts ran was not this repo's original but pulsar-gui's own copy
(`exporter/sparkview_exporter.py`, 051526e) on Sparky and pve1 CT 130, and the older original on
CT 124 and CT 131. pulsar-gui's copy had grown the GPU processes' `gpu`, `ppid` and served `port`
(how pulsar-gui attributes a card to a service), a Prometheus `/metrics` in the 0.6.0 series (the
Switchyard dashboard), the 0.6.0 unit flags, and clock caps given as unit variables. All of that
was carried into both implementations first (`2bf0f6f`), then checked against the running
exporter on each host with `scripts/compare.py http://localhost:9420 ./vitals`. Sparky: no
differences, 271 of 271 series. CT 130: none, 73 of 73. CT 124 and CT 131: none in
`/metrics.json`, and the old copy there has no `/metrics`.

| Host | How it runs | Binary |
|---|---|---|
| Sparky | `vitals.service`, `/opt/vitals/vitals`, user `vitals` | aarch64 `9a4909da` |
| pve2 CT 124 `turing` | `vitals.service` + `vitals.service.d/nvml.conf` (`LD_LIBRARY_PATH=/usr/local/nvidia/lib64`) | x86_64 `b813facf` |
| pve2 CT 131 `retriever` | the same | x86_64 `b813facf` |
| pve1 CT 130 `gemma` | the container's running entrypoint (`/opt/sparkview-exporter/entrypoint.sh`) starts `sparkview_exporter.py`, now a shim that execs `/opt/vitals/vitals`, as nobody | x86_64 `b813facf` |

On each host the old `sparkview-exporter.service` is disabled and stopped, and its files are kept
for rollback (`systemctl disable --now vitals && systemctl enable --now sparkview-exporter`). On
CT 130 the original is `sparkview_exporter.py.bak-2026-10-27`: copy it back over the shim and stop
the running `vitals` to roll back. CT 130's entrypoint still names the old path. `pct set 130
--entrypoint /opt/vitals/entrypoint.sh` takes effect at its next start, and then the shim and
`/opt/sparkview-exporter` can go. pulsar-gui (CT 104) reads all four hosts as `ok` on 0.3.0.

## Results

Serving a 1 Hz stream, whole process:

| Host | Rust: CPU per sample | Rust: peak RSS | Python: CPU per sample | Python: peak RSS |
|---|---|---|---|---|
| the VM (no GPU) | 7.5 ms | 4 MB | 11.5 ms | 27 MB |
| Sparky (GB10) | 8.5 ms | 35 MB | 10.0 ms | 63 MB |
| CT 124 (RTX 5000) | 50.5 ms | 30 MB | 56.0 ms | 52 MB |

- **CPU** came out as predicted: −10 to −35%. The rest is kernel and NVIDIA driver time.
- **Memory** is lower than Python's, but not the ~5–8 MB predicted on GPU hosts. The binary
  alone is 4 MB; loading NVML adds ~20 MB of `libnvidia-ml`/`libcuda` allocations, which every
  NVML client carries.
- **Size:** 1.9 MB (x86_64) and 1.6 MB (aarch64), built from the VM with `cargo zigbuild` against
  glibc 2.39.

What the port changed or found on the way:

- **NVML** goes through `nvml-wrapper-sys`'s raw bindings, not the safe wrapper. The calls are the
  ones pynvml makes, so outputs match one for one. A function the driver lacks is a missing
  symbol, not a panic. No typed enum can reject a value the crate does not know.
- **Field-ID drift:** the bindings' header numbers the clocks-event thermal and power-brake
  fields 251–253. Current NVML numbers them 269–271, and a 615 driver answers only those. Found
  by the diff on both GPU hosts.
- **Physical cores in a container:** psutil counts every core of the machine (48 in CT 124, which
  has 24 threads). Both implementations now count the cores among the CPUs the host or container
  has (20).
- **psutil, replaced:** `/proc/stat` CPU times, `/proc/self/mounts` with getmntent's unescaping,
  `/proc/diskstats` in 512-byte sectors, `/proc/<pid>/stat` with psutil's state names and name
  fix, `getifaddrs`, and utmp, with no utmp counting as 0 users as psutil does.
- **NVIDIA's own Rust** (`NVIDIA/cuda-rust`, early research) covers the CUDA driver, runtime and
  kernels, not NVML. Nothing NVIDIA publishes covers monitoring.

## Why, and why not

**CPU is not the reason.** After the tiering work (`49378aa`), one 1 Hz sample costs, for the
whole process:

| Host | Python's own code (user) | Kernel and NVIDIA driver | Total | Share of a core |
|---|---|---|---|---|
| CT 124 (RTX 5000) | 9.5 ms | 47 ms | 57 ms | 5.7% |
| Sparky (GB10) | 3.4 ms | 6.7 ms | 10 ms | 1.0% |

The kernel column is `/sys`/`/proc` reads and NVML ioctls. A Rust binary makes the same reads and
the same NVML calls, so it pays that column too. A port removes most of the user column:
**about 57 → 48 ms on CT 124 and 10 → 7 ms on Sparky.** On CT 124 the bigger lever is the
clocks-event reasons query (~25 ms of driver CPU), which at a 1 Hz stream still runs every
sample. Raising `REASONS_TTL_S` from 1 s to 5 s gets CT 124 to ~35 ms in Python.

**What a port does buy:**

- **Deployment.** One static-ish binary per architecture. No Python, no psutil, and no vendored
  `pynvml.py` in containers. CT 131/124 and the OCI app containers get one file.
- **Memory.** The Python process is ~53 MB RSS on CT 124. Expect ~5–8 MB.
- **Startup and robustness.** No interpreter-version drift: DGX OS ships Python 3.14, Debian 13
  ships 3.13.
- **Safety under load.** Real threads, with no GIL serialising a slow NVML call against the HTTP
  handlers.

**What it costs:** a second implementation of 3,345 lines of collectors and server, kept in parity
with the JSON contract, plus cross-builds for x86_64 and aarch64.

## Shape of the port

Same API, same JSON, same behaviour. The contract is the existing Python test suite and
`/metrics.json`'s reference fixtures, not this document.

| Python module | Rust module | Notes |
|---|---|---|
| `sysfs.py` | `sysfs.rs` | `read_text`/`read_int`/`scaled`, `cpu_list`, `bracketed`; std only |
| `rates.py` | `rates.rs` | Same semantics: no rate on first sight or a reset |
| `collect/host.py` | `collect/host.rs` | machine type (systemd-detect-virt, lxcfs, DMI), os-release, DMI, systemd TTL probe |
| `collect/cpu.py` | `collect/cpu.rs` | `/proc/stat` per-CPU times parsed directly (psutil's `cpu_times`), lscpu JSON, throttle TTL |
| `collect/memory.py` | `collect/memory.rs` | meminfo, vmstat, PSI: plain parsing |
| `collect/sensors.py` | `collect/sensors.rs` | hwmon layout cache, thermal, cooling TTL, spbm |
| `collect/storage.py` | `collect/storage.rs` | `/proc/mounts`, `statvfs` (rustix), the NFS-timeout pool, `/proc/diskstats` |
| `collect/network.py` | `collect/network.rs` | `/proc/net/dev`, sockstat, snmp, RDMA sysfs, `rdma -j statistic` subprocess |
| `collect/pci.py` | `collect/pci.rs` | sysfs walk, `pci.ids` scan |
| `collect/gpu.py` | `collect/gpu.rs` | `nvml-wrapper`; tiers, window rates, topology, process table |
| `collect/processes.py` | `collect/processes.rs` | `/proc/<pid>/{stat,status}` (the `procfs` crate, or hand-rolled) |
| `hub.py` | `hub.rs` | sampler thread, static cache and generation (sha1 of canonical JSON), latest-sample encode cache |
| `server.py` | `server.rs` | HTTP/1.1 keep-alive, gzip, CORS, SSE with cap and keepalive |
| `legacy.py` | `legacy.rs` | `/metrics.json` exactly as today |

### Crates

Keep the list short. This runs as a locked-down system user on every host.

| Need | Crate | Why |
|---|---|---|
| NVML | `nvml-wrapper` 0.13 (+ `nvml-wrapper-sys` for gaps) | Covers the calls vitals makes: throttle reasons, PCIe throughput, energy, field values, topology, P2P, encoder/decoder, BAR1, ECC, fans, limits, thresholds, processes, MIG. Loads `libnvidia-ml.so` at runtime via `libloading`, so a host without the driver still runs. **Check before relying on it:** NVLink state/remote/version and remapped rows were not under the names I looked for in its docs. Call them through `nvml-wrapper-sys` if absent. |
| JSON | `serde`, `serde_json` | `serde_json::Value` with `preserve_order`, so output key order matches Python's |
| HTTP | none: std `TcpListener` + a thread per connection, like `ThreadingHTTPServer` | The API is GETs, SSE and keep-alive. A framework (axum + tokio) adds a runtime for nothing. `tiny_http` is the fallback if hand-rolled parsing gets awkward |
| gzip | `flate2` (rust backend) | |
| sha1 | `sha1_smol` | Static generation; must match Python's for the same content only if both run side by side (they need not) |
| statvfs, getrusage | `rustix` | |
| CLI | `lexopt` or hand-rolled | Five flags |

No psutil equivalent (`sysinfo`): it reads the same files with its own semantics. Parsing them
directly keeps the numbers identical to the Python version, which the parity check needs.

### Builds

- **glibc, not musl.** `nvml-wrapper` dlopens the driver's `libnvidia-ml.so`, a glibc library,
  and a fully static musl binary cannot load it. Build `x86_64-unknown-linux-gnu` and
  `aarch64-unknown-linux-gnu` against the oldest glibc in the fleet, Debian 13's 2.41.
  Sparky has 2.43, so a 2.41 build runs there.
- **Where:** the VM has Rust 1.97. Sparky and CT 124 have no toolchain, and none is needed there.
  Cross-build aarch64 from the VM with `cargo zigbuild --target aarch64-unknown-linux-gnu.2.41`,
  or `cross`.
- **Containers:** still need the NVML bind mount and the `LD_LIBRARY_PATH=/usr/local/nvidia/lib64`
  drop-in, so dlopen finds the host's library. They no longer need `pynvml.py` or psutil.
- **Size:** expect 2–4 MB stripped per architecture.

## Plan

Each phase ends green on the shared contract tests before the next starts.

1. **Make the tests the contract (in Python first).** Let `tests/test_api.py` target an external
   server (`VITALS_URL=http://host:port`) as well as its in-process one. Add a golden check: run
   both implementations on one host, diff the key sets of every section and the values of
   readings that should agree within tolerance. This is the parity gate for every later phase.
2. **Skeleton:** `Cargo.toml` in `rust/`, HTTP server, routing, JSON envelope, `Rates`, `Hub` with
   sampler, static cache, SSE stream with cap/keepalive/intervals. Collectors stubbed. Gate: the
   stream, cap and interval tests in `test_api.py`.
3. **Host collectors:** host, cpu, memory, pressure, sensors, storage (with the NFS timeout),
   network, rdma, pci, processes. Gate: the API tests plus the golden diff on the VM and CT 124.
4. **GPUs:** `nvml-wrapper`, tiers, window rates, topology, the clock cap, the process join. Gate:
   the golden diff on CT 124 (Turing, NVLink, discrete memory) and Sparky (GB10, UMA, spbm).
5. **`/metrics.json`:** the legacy shape. Gate: `test_legacy.py`'s reference-field test and
   pulsar-gui's round-trip test against the binary.
6. **Packaging:** both architectures, the systemd unit, `contrib/` for containers, README.
7. **Cutover:** run the binary on :9421 beside the Python one on each host for a day and compare.
   Then switch the unit's `ExecStart`, keeping the zipapp as the rollback.

**Effort:** about 4,000 lines of Rust. Phases 1–2 are the riskiest per line: the HTTP/SSE edge
cases and the test harness. Phase 4 has the most host-specific behaviour.

## Risks

- **Parity drift:** two implementations of one contract. Mitigated by the golden diff, and by
  retiring the Python one after cutover rather than keeping both.
- **psutil semantics:** process `cpu_percent`, `memory_percent`, `disk_io_counters` `busy_time`,
  and guest time inside user. Each needs a note and a test against the Python reading.
- **NVML coverage:** anything `nvml-wrapper` lacks goes through `-sys`. GB10 answers
  `NOT_SUPPORTED` to more calls than Turing does. Both must map to `null`, never to an error.
- **Two architectures to build and ship,** where the zipapp is one file for all.

## Recommendation

Port only if deployment and memory matter enough: one binary for containers, ~5 MB instead of
~53 MB, no Python to match per host. On CPU, `REASONS_TTL_S` = 5 s gets most of what's left, in
an afternoon, without a second implementation. If the port goes ahead, phase 1 is worth doing
anyway: an API test suite that runs against any server is useful on its own.
