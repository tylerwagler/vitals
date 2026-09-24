# sparkview-exporter

One host's GPU and hardware state as JSON, for the cluster view in
[pulsar-gui](../../pulsar-gui). Run one on each host that holds a TP rank; the
GUI polls `http://{host}:9420/metrics.json` once a second.

```sh
python3 sparkview_exporter.py            # serve on 0.0.0.0:9420
python3 sparkview_exporter.py --once     # print one snapshot and exit
curl -s localhost:9420/metrics.json | jq .gpus[0].throttle
```

It needs `nvidia-ml-py` (imported as `pynvml`) and `psutil`, and nothing else.
DGX OS ships both system-wide, so on a Spark it runs with the system Python.
Without NVML it still reports the host, with `gpus: []`.

## What it reports

The shape is the reference sparkview-exporter 0.6.0's, as pulsar-gui's
`tests/fixtures/sparkview-*.json` capture it; `tests/` checks every one of
those fields is present and runs a real snapshot through pulsar-gui's own
`ClusterPoller`.

| Key | Source |
|---|---|
| `gpus[]` | NVML: utilization, temperature, power, clocks, P-state, clocks-event reasons and their cumulative counters, identity. On GB10 the memory is the host's (`is_uma`). |
| `gpus[].throttle.status` | sparkview's load-gated `IDLE / PASS / LOCKED / THROTTLED` |
| `gpus[].clock_cap` | the `-lgc MIN,MAX` of an **active** `*clock-cap*.service` unit, which NVML cannot see; `null` otherwise |
| `spbm` | every labelled power, temperature and energy channel of the spark_hwmon driver, plus `prochot`, `pl_level`, `tj_rise_celsius` |
| `memory`, `pressure`, `cpu` | psutil and `/proc/pressure/memory`. Used memory is `total - available`. |
| `thermal_zones`, `hwmon_temperatures` | `/sys/class/thermal`, `/sys/class/hwmon` |
| `network` | physical interfaces' kernel counters (these do **not** include RDMA traffic) |
| `rdma` | every `/sys/class/infiniband` port: state, rate, byte counters (words × 4), error and hw counters |

Readings are taken when a request arrives and shared by requests within
250 ms, so an idle exporter costs nothing and several dashboards cost one
scrape. A reading that cannot be taken is `null` or absent, never `0`.

## Install as a service

```sh
sudo useradd --system --no-create-home --shell /usr/sbin/nologin sparkview-exporter
sudo install -D -m 0644 sparkview_exporter.py /opt/sparkview-exporter/sparkview_exporter.py
sudo install -m 0644 sparkview-exporter.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now sparkview-exporter
```

It runs as that bare system account with a read-only filesystem: everything it reads is
world-readable. It needs `AF_NETLINK` for `rdma statistic`, which reads every
port's hw counters in one query instead of one firmware query per sysfs file.

### In a Proxmox app container

An OCI app container runs its workload as PID 1, so nothing inside it can keep
the exporter running. `contrib/sparkview-exporter-pct@.service` supervises it
from the host instead, with `pct exec`, so it still runs in the container's
namespaces: it answers on the container's address and reports its hostname,
memory and GPU processes. The container needs `python3-psutil` and the two
files; `pynvml.py` is a single pure-Python file and can be copied from any
`nvidia-ml-py` install.

```sh
# in the container
apt-get install -y --no-install-recommends python3-psutil
mkdir -p /opt/sparkview-exporter   # + sparkview_exporter.py, pynvml.py
# on the host
install -m 0644 contrib/sparkview-exporter-pct@.service /etc/systemd/system/
systemctl daemon-reload && systemctl enable --now sparkview-exporter-pct@130
```

On a discrete card only the GPU and the container's own view are specific to
it; hwmon, thermal zones and RDMA ports are the host's, as `/sys` is.

## Tests

```sh
python3 -m venv .venv
.venv/bin/pip install psutil pytest nvidia-ml-py httpx fastapi -e ../../pulsar-tui
.venv/bin/pip install --no-deps -e ../../pulsar-gui   # its pyproject pins pulsar-tui by path
.venv/bin/python -m pytest -q
```

## Credit

The clock-status logic, PSI levels and spbm rail layout come from
[sparkview](https://github.com/parallelArchitect/sparkview) (MIT,
© 2026 parallelArchitect). Two places differ on purpose: throttle reasons use
NVML's bit values (sparkview's table has `0x100` as the power cap; NVML has
it as the display clock), and spbm's status attributes are read from the hwmon
directory first, where current kernels put them.
