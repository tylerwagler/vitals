# vitals

One host's hardware, OS and live state as JSON over HTTP: what the machine
*is* (bare metal, VM or LXC; OS, kernel, firmware; CPUs, GPUs, PCI devices,
disks, NICs, RDMA, sensors; driver versions) and what it is *doing* (CPU,
RAM, pressure, GPUs, temperatures, fans, power, disk usage and I/O, network,
RDMA), as request/reply or as a Server-Sent Events stream.

```sh
python3 vitals.pyz                       # serve on 0.0.0.0:9420
python3 vitals.pyz --once                # print static + live and exit (also: static, live, processes, metrics)
curl -s localhost:9420/v1/static | jq .host.machine
curl -s localhost:9420/v1/live/gpus | jq '.gpus[0].power'
curl -sN 'localhost:9420/v1/stream?interval=1&sections=cpu,memory'
```

It needs `psutil` and, for GPUs, `nvidia-ml-py` (imported as `pynvml`), and
nothing else. Without NVML it still reports everything else, with no GPUs.
pulsar-gui's cluster view polls `/metrics.json`, which keeps its old shape.

## The API

| Endpoint | What |
|---|---|
| `GET /` | the index: endpoints and section names |
| `GET /health` | liveness, version, NVML state, streams, static generation |
| `GET /v1/static[/<section>]` | what the machine is and has. Collected at most once a minute; `?refresh=1` re-collects. `ETag` is the generation |
| `GET /v1/live[/<section>]` | one live sample. `?sections=a,b` for a subset |
| `GET /v1/processes` | the process table: `?top=25&sort=cpu\|memory\|gpu` |
| `GET /v1/stream` | SSE: `?interval=1` (0.25–60 s), `?sections=a,b`, `?static=1` |
| `GET /metrics.json` | pulsar-gui's snapshot, in the reference sparkview-exporter 0.6.0 shape |

Every endpoint takes `?pretty=1`. Responses are gzipped when the client
accepts it, and carry `Access-Control-Allow-Origin: *`, so a browser
dashboard can use `EventSource` directly. Request/reply connections are kept
alive (HTTP/1.1).

### The stream

Modelled on pulsar-server's `/metrics/stream`:

```
retry: 3000

event: hello
data: {"service":"vitals","version":"0.2.0","hostname":"turing","interval_s":1.0,"keepalive_s":15.0,"sections":[...],"static_generation":"1c431b93f3e0"}

id: 42
event: sample
data: {"hostname":"turing","seq":42,"time":...,"interval_s":1.0002,"duration_s":0.18,"static_generation":"1c431b93f3e0","cpu":{...},...}

event: static
data: {"generation":"7d0e5a91c2b4"}

: keepalive
```

- `hello` comes first; with `?static=1` it carries the whole static document.
- A `sample` carries the live sample itself, filtered to the requested
  sections. pulsar's event carries only a generation because its body is a
  re-scrape; here the sample *is* the reading.
- `static` means the inventory changed (a card, link, disk or address);
  fetch `/v1/static` again.
- `: keepalive` is sent after 15 s of silence.
- At most 8 streams (`--max-streams`); past that the answer is `503` with
  `Retry-After`, and the client should poll `/v1/live` instead.
- There is no backpressure: each subscriber gets the latest sample when it is
  due, so a slow reader skips samples and holds nothing up.

One sampler thread runs while anyone is subscribed, at the fastest interval
anyone asked for. A subscriber with a longer interval gets every Nth sample.
With no subscribers nothing runs, and `/v1/live` samples when asked (shared
by requests within 250 ms, or within one interval while a stream runs).

### Readings and rates

- **Missing is not zero.** A reading that cannot be taken is `null` or
  absent, never `0`. A GeForce card has no ECC, GB10 has no device memory,
  a VM has no fans.
- **Rates:** every counter the kernel or NVML keeps cumulatively (bytes,
  packets, I/O, context switches, page faults, stall time, GPU energy,
  throttle time) also has a `*_per_s` or `*_percent` rate. A rate covers the
  time since the previous sample, which is `interval_s`. The first sample
  has no rates, and neither does a counter that went backwards.

### Static sections

| Section | Contents |
|---|---|
| `host` | hostname; **machine type** (`bare-metal` / `vm` / `lxc` / `container`) with hypervisor and container tech; OS (os-release, DGX release, Proxmox version); kernel release, version, arch, cmdline, taint; DMI firmware (vendor, product, board, BIOS); boot time; timezone |
| `exporter` | vitals version, Python, psutil and nvidia-ml-py versions, NVML state, pid, user, listen address |
| `cpu` | model, logical and physical counts, sockets, clusters (model and min/max MHz per CPU group, so hybrid parts show both), caches, NUMA nodes, SMT, isolated CPUs, governor, frequency driver, EPP, boost, flags, vulnerabilities |
| `memory` | total, swap, swap devices, huge page sizes, THP mode, swappiness, overcommit, min_free, zswap, KSM |
| `gpus` | driver, CUDA and NVML versions; per card: name, UUID, serial, board part, brand, architecture, compute capability, cores, VBIOS, InfoROM, PCI ids and max link, memory size and bus width, max clocks, power limits (default, min, max, enforced), temperature thresholds, persistence/compute/ECC/MIG modes, fans, NVLink links and their peers; pairwise topology and P2P |
| `pci` | every device: address, vendor/device/subsystem ids and names (from `pci.ids`), class, driver, NUMA node, IOMMU group, parent, link speed/width (current and max); and every **driver** in use, with its module version |
| `storage` | block devices (model, vendor, serial, firmware, transport, size, rotational, block sizes, scheduler, partitions, holders) and filesystems (device, mountpoint, type, read-only, network) |
| `network` | interfaces (kind, MAC, MTU, driver, PCI address, speed, duplex, bridge/bond membership, IPv4/IPv6 addresses) and RDMA devices (HCA, board, firmware, GUID, ports) |
| `sensors` | every hwmon chip and sensor with its unit and limits (min, max, crit, …); thermal zones with policy and trip points |

The `generation` is a hash of the contents, leaving out each PCI link's
current speed and width: a GPU's link downtrains whenever the card idles,
and that is not a change of hardware.

### Live sections

| Section | Contents |
|---|---|
| `host` | uptime, load average (and per CPU), processes and threads (running, blocked), users, file handles, context switches / interrupts / forks per second, systemd state, failed units, time sync |
| `cpu` | utilization and its breakdown (user, system, iowait, irq, steal, …), per-core utilization and frequency, min/mean/max frequency, CPU temperature, x86 thermal-throttle events (read every 10 s) |
| `memory` | **RAM**: total, used (total − available), available, free, cached, buffers, used %; swap; huge pages; paging rates (faults, major faults, swap in/out, reclaim); OOM kills; every `/proc/meminfo` field |
| `pressure` | PSI for cpu, memory, io and irq: some/full avg10/60/300, totals, stall % over the interval; memory's level (LOW/MOD/HIGH/CRITICAL) |
| `gpus` | per card: utilization (GPU, memory, encoder, decoder), memory and BAR1, temperature, fans, power draw, limit, energy and mean power, clocks (graphics, SM, memory, video), P-state, throttle status and reasons, clocks-event time and %, the clock cap, PCIe link and throughput and replays, ECC errors, remapped rows, processes |
| `sensors` | **temperatures**, fans, voltages, currents, power and energy from every hwmon sensor (with alarms); the hottest temperature; thermal zones; cooling devices; GB10's spbm rails |
| `storage` | **disk usage** per filesystem (total, used, free, used %, inodes); per-disk I/O (bytes and ops per second, busy %) |
| `network` | per interface: state, carrier, counters and rates (bytes, bits, packets, errors, drops); socket counts; TCP opens, resets, segments, retransmits and retransmit % |
| `rdma` | per port: state, rate, data and packet counters and rates, error and hw counters |

Process tables give names and owners, never command lines, which can carry
secrets (an `--api-key`). On the LAN, everything here is readable by anyone.

**Cost.** Readings are tiered by what they cost. Every sample takes the cheap
ones: utilization, memory, temperatures, power, the graphics clock. A GPU's
clocks-event reasons (about 25 ms of driver CPU on an idle Turing card) are
asked at most once a second. PCIe throughput (20 ms per call by design),
encoder/decoder, the other clocks, energy, event counters, ECC and fans are
asked every 5 s (`slow_readings_age_s` says how old). hwmon labels are cached
for a minute, and cooling devices for 10 s. At 1 Hz, the whole process costs
about 57 ms of CPU per sample on CT 124 (one RTX 5000, 5.7% of a core) and
10 ms on Sparky (1%). Nothing runs while nobody listens. `docs/rust-port.md`
scopes a native rewrite.

## Build

The source is a package under `src/vitals/` (one collector module per
subsystem under `collect/`). It deploys as a single zipapp:

```sh
make pyz        # -> dist/vitals.pyz, runs with any python3 >= 3.11
```

A `pynvml.py` copied next to `vitals.pyz` is importable, for containers
without `nvidia-ml-py`.

## Install as a service

```sh
make pyz
sudo useradd --system --no-create-home --shell /usr/sbin/nologin vitals
sudo install -D -m 0644 dist/vitals.pyz /opt/vitals/vitals.pyz
sudo install -m 0644 vitals.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now vitals
curl -s localhost:9420/health
```

It runs as that bare system account on a read-only filesystem, since
everything it reads is world-readable. Readings that need root (DIMMs, disk
SMART, DMI serials) are left out. It needs `AF_NETLINK` for `rdma
statistic`, which reads every port's hw counters in one query instead of one
firmware query per sysfs file. NFS and other network mounts get a 0.5 s
timeout on `statvfs` and are reported `"responsive": false` instead of
hanging a sample.

### In a Proxmox app container

An OCI app container runs its workload as PID 1, so nothing inside it keeps
the exporter running. `contrib/entrypoint.sh` handles that without touching
the host: set it as the container's entrypoint, and it starts the exporter
(as nobody, restarted if it exits), then `exec`s the workload, which stays
PID 1. Its arguments are the workload; with none it runs `/app/llama-server`.

```sh
# in the container: python3-psutil, and in /opt/vitals
# vitals.pyz, pynvml.py and entrypoint.sh (0755)
# on the host, then restart the container
pct set 130 --entrypoint /opt/vitals/entrypoint.sh
```

If the container's entrypoint must stay as it is,
`contrib/vitals-pct@.service` supervises the exporter from the host instead,
with `pct exec`. It still runs in the container's namespaces, so it answers
on the container's address and reports the container's hostname, memory and
GPU processes. The container needs `python3-psutil` and the two files.
`pynvml.py` is a single pure-Python file and can be copied from any
`nvidia-ml-py` install.

```sh
# in the container
apt-get install -y --no-install-recommends python3-psutil
mkdir -p /opt/vitals   # + vitals.pyz, pynvml.py
# on the host
install -m 0644 contrib/vitals-pct@.service /etc/systemd/system/
systemctl daemon-reload && systemctl enable --now vitals-pct@130
```

In a container, only the GPU and the container's own view (memory,
processes, filesystems) are specific to it. hwmon, thermal zones, PCI and
RDMA ports are the host's, because `/sys` is.

## Moving a host from sparkview-exporter

Install `vitals` as above, then stop and remove the old unit before starting
the new one, since both want :9420:

```sh
sudo systemctl disable --now sparkview-exporter
sudo rm /etc/systemd/system/sparkview-exporter.service && sudo systemctl daemon-reload
sudo systemctl enable --now vitals
sudo rm -r /opt/sparkview-exporter && sudo userdel sparkview-exporter
curl -s localhost:9420/metrics.json | jq .gpus[0].throttle
```

In a container that uses `entrypoint.sh`, copy the files to `/opt/vitals`, then
`pct set <ctid> --entrypoint /opt/vitals/entrypoint.sh` and restart it.
`SPARKVIEW_PORT` is now `VITALS_PORT`.

## Tests

```sh
python3 -m venv .venv
.venv/bin/pip install psutil pytest nvidia-ml-py httpx fastapi -e ../../pulsar/tui
.venv/bin/pip install --no-deps -e ../../pulsar/gui   # its pyproject pins pulsar-tui by path
.venv/bin/pip install --no-deps -e .
.venv/bin/python -m pytest -q
```

`test_collect.py` runs the collectors on fake `/sys` trees and a fake NVML,
`test_api.py` runs a real server on a free port (endpoints, the stream, its
cap and intervals), and `test_legacy.py` holds `/metrics.json` to the
reference shape and runs it through pulsar-gui's own cluster poller (skipped
unless `pulsar_gui` is importable).

## Credit

The clock-status logic, PSI levels and spbm rail layout come from
[sparkview](https://github.com/parallelArchitect/sparkview) (MIT,
© 2026 parallelArchitect). Two places differ on purpose: throttle reasons use
NVML's bit values (sparkview's table has `0x100` as the power cap; NVML has
it as the display clock), and spbm's status attributes are read from the hwmon
directory first, where current kernels put them.
