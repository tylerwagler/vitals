#!/bin/sh
# Entrypoint for a Proxmox app (OCI) container: run vitals
# alongside the workload, then become the workload.
#
#   pct set <ctid> --entrypoint /opt/vitals/entrypoint.sh
#
# The workload is this script's arguments, or /app/llama-server without any.
# It is exec'd, so it stays PID 1: the container's stop signal reaches it
# unchanged, and the container ends when it does, exporter and all.
#
# The exporter runs as nobody and is restarted if it exits. Its loop is the
# exporter's parent, so a dead exporter is reaped here rather than left for
# a PID 1 that never expected children.

EXPORTER=/opt/vitals/vitals.py

(
    while :; do
        setpriv --reuid=nobody --regid=nogroup --clear-groups --no-new-privs \
            /usr/bin/python3 "$EXPORTER" --port "${VITALS_PORT:-9420}" </dev/null
        echo "vitals exited ($?); restarting in 15 s" >&2
        sleep 15
    done
) &

if [ "$#" -eq 0 ]; then
    set -- /app/llama-server
fi
exec "$@"
