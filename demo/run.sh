#!/usr/bin/env bash
# Guest-visible clock deterministic replay demo.
#
# This demo proves a narrower claim than "all guest time queries are deterministic":
# Firecracker now records and replays the guest-visible VMClock state exposed during
# snapshot restore. We show that by:
#   1. creating a snapshot origin,
#   2. restoring it in Record mode to capture the VMClock update,
#   3. restoring the same snapshot in Replay mode with the saved log, and
#   4. showing that replay succeeds before the guest is even resumed.
#
# The script also includes two negative proofs:
#   - a valid but empty replay log fails at restore time,
#   - a tampered first event fails at restore time.

set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
FC_BIN="$REPO/build/cargo_target/x86_64-unknown-linux-musl/debug/firecracker"
KERNEL="$REPO/demo/vmlinux-6.1.155"
INITRD="$REPO/demo/initrd.cpio"
SOCK=/tmp/fc-demo.sock
LOG=/tmp/replay.detlog
EMPTY_LOG=/tmp/replay-empty.detlog
TAMPERED_LOG=/tmp/replay-tampered.detlog
SNAP_VMSTATE=/tmp/fc-demo.vmstate
SNAP_MEM=/tmp/fc-demo.mem
METRICS=/tmp/fc-demo.metrics
FC_LOG_BASE=/tmp/fc-demo-base.log
FC_LOG_REC=/tmp/fc-demo-record.log
FC_LOG_REP=/tmp/fc-demo-replay.log
FC_LOG_EMPTY=/tmp/fc-demo-empty.log
FC_LOG_TAMPER=/tmp/fc-demo-tamper.log
FC_LOG=

rm -f "$SOCK" "$LOG" "$EMPTY_LOG" "$TAMPERED_LOG" "$SNAP_VMSTATE" "$SNAP_MEM" \
      "$METRICS" "$FC_LOG_BASE" "$FC_LOG_REC" "$FC_LOG_REP" "$FC_LOG_EMPTY" "$FC_LOG_TAMPER"
: > "$METRICS"

[[ -x "$FC_BIN" ]] || {
    echo "missing firecracker binary at $FC_BIN" >&2
    echo "build it first with: tools/devtool build --debug" >&2
    exit 1
}
[[ -f "$INITRD" ]] || {
    echo "missing $INITRD — run demo/build_initrd.sh first" >&2
    exit 1
}
[[ -r /dev/kvm && -w /dev/kvm ]] || {
    echo "cannot access /dev/kvm — add \$USER to the 'kvm' group:" >&2
    echo "    sudo usermod -aG kvm \$USER && newgrp kvm" >&2
    echo "or re-run this script under 'sg kvm -c ...' / 'sudo ...'" >&2
    exit 1
}

pause() { echo; read -rp "[enter to continue] " _; }
section() { printf '\n\033[1;34m=== %s ===\033[0m\n' "$*"; }

api() {
    local method=$1 path=$2 body=${3:-}
    if [[ -n "${FC_PID:-}" ]] && ! kill -0 "$FC_PID" 2>/dev/null; then
        echo >&2
        echo "!! firecracker (pid=$FC_PID) is dead — tail of $FC_LOG:" >&2
        tail -n 30 "$FC_LOG" >&2
        exit 1
    fi
    if [[ -n "$body" ]]; then
        curl -sS --unix-socket "$SOCK" -X "$method" "http://localhost$path" \
            -H "Content-Type: application/json" -d "$body"
    else
        curl -sS --unix-socket "$SOCK" -X "$method" "http://localhost$path"
    fi
    echo
}

api_status() {
    local method=$1 path=$2 body=${3:-}
    local out http
    if [[ -n "$body" ]]; then
        out=$(mktemp)
        http=$(curl -sS --unix-socket "$SOCK" -o "$out" -w '%{http_code}' -X "$method" \
            "http://localhost$path" -H "Content-Type: application/json" -d "$body")
    else
        out=$(mktemp)
        http=$(curl -sS --unix-socket "$SOCK" -o "$out" -w '%{http_code}' -X "$method" \
            "http://localhost$path")
    fi
    cat "$out"
    rm -f "$out"
    echo
    echo "http_status=$http"
}

launch_firecracker() {
    [[ -n "$FC_LOG" ]] || { echo "launch_firecracker: FC_LOG not set" >&2; exit 1; }
    rm -f "$SOCK"
    : > "$FC_LOG"
    "$FC_BIN" --api-sock "$SOCK" >>"$FC_LOG" 2>&1 &
    FC_PID=$!
    sleep 0.3
    echo "firecracker pid=$FC_PID  sock=$SOCK  log=$FC_LOG"
}

kill_firecracker() {
    if [[ -n "${FC_PID:-}" ]] && kill -0 "$FC_PID" 2>/dev/null; then
        kill "$FC_PID"
        wait "$FC_PID" 2>/dev/null || true
    fi
    FC_PID=
}

cleanup() { kill_firecracker; rm -f "$SOCK"; }
trap cleanup EXIT

replay_metrics() {
    api PUT /actions '{"action_type":"FlushMetrics"}' >/dev/null
    METRICS="$METRICS" python3 <<'PY'
import json, os
path = os.environ["METRICS"]
with open(path) as f:
    lines = [l for l in f if l.strip()]
if not lines:
    print("  (no metrics flushed yet)")
else:
    m = json.loads(lines[-1])["replay"]
    print("  events_recorded =", m["events_recorded"])
    print("  events_replayed =", m["events_replayed"])
    print("  divergences     =", m["divergences"])
PY
}

dump_replay_log() {
    local path=$1
    local limit=${2:-12}
    REPLAY_LOG_PATH="$path" REPLAY_LOG_LIMIT="$limit" python3 <<'PY'
import os
import struct
import sys

path = os.environ["REPLAY_LOG_PATH"]
limit = int(os.environ["REPLAY_LOG_LIMIT"])

kind_names = {
    0: "MMIO_READ",
    1: "MMIO_WRITE",
    2: "PIO_IN",
    3: "PIO_OUT",
    4: "VMCLOCK",
    5: "RDTSC",
}

def fmt_bytes(buf: bytes) -> str:
    if not buf:
        return "-"
    hexed = " ".join(f"{b:02x}" for b in buf[:8])
    if len(buf) > 8:
        hexed += " ..."
    return hexed

with open(path, "rb") as f:
    blob = f.read()

if len(blob) < 6 or blob[:4] != b"DET0":
    print(f"  {path}: not a DET0 replay log", file=sys.stderr)
    sys.exit(1)

version = struct.unpack_from("<H", blob, 4)[0]
offset = 6
events = []
while offset < len(blob):
    seqno = struct.unpack_from("<Q", blob, offset)[0]
    offset += 8
    kind = blob[offset]
    offset += 1
    offset += 3
    addr = struct.unpack_from("<Q", blob, offset)[0]
    offset += 8
    size = struct.unpack_from("<I", blob, offset)[0]
    offset += 4
    data = blob[offset:offset + size]
    offset += size
    events.append((seqno, kind, addr, size, data))

print(f"  log_version = {version}")
print(f"  total_events = {len(events)}")
for seqno, kind, addr, size, data in events[:limit]:
    print(
        f"  seq={seqno:03d}  {kind_names.get(kind, f'KIND_{kind}'):<8} "
        f"addr=0x{addr:x}  size={size:<3} data=[{fmt_bytes(data)}]"
    )
if len(events) > limit:
    print(f"  ... {len(events) - limit} more events not shown")
PY
}

make_empty_replay_log() {
    python3 - "$1" <<'PY'
import struct
import sys
with open(sys.argv[1], "wb") as f:
    f.write(b"DET0")
    f.write(struct.pack("<H", 1))
PY
}

tamper_first_event_kind() {
    python3 - "$1" "$2" <<'PY'
import sys
src, dst = sys.argv[1], sys.argv[2]
with open(src, "rb") as f:
    data = bytearray(f.read())
assert data[:4] == b"DET0"
kind_offset = 6 + 8
original = data[kind_offset]
data[kind_offset] = 2
with open(dst, "wb") as f:
    f.write(data)
print(f"tampered first event kind at offset {kind_offset}: {original} -> {data[kind_offset]}")
PY
}

section "1. Boot a baseline VM and create the snapshot origin"
FC_LOG="$FC_LOG_BASE"
launch_firecracker
api PUT /boot-source \
    "{\"kernel_image_path\":\"$KERNEL\",\"initrd_path\":\"$INITRD\",\"boot_args\":\"console=ttyS0 reboot=k panic=1 pci=off quiet\"}"
api PUT /machine-config '{"vcpu_count":1,"mem_size_mib":128}'
api PUT /metrics "{\"metrics_path\":\"$METRICS\"}"
api PUT /actions '{"action_type":"InstanceStart"}'
sleep 0.2
api PATCH /vm '{"state":"Paused"}'
api PUT /snapshot/create "{\"snapshot_path\":\"$SNAP_VMSTATE\",\"mem_file_path\":\"$SNAP_MEM\"}"
echo "Snapshot files:"
ls -la "$SNAP_VMSTATE" "$SNAP_MEM"
kill_firecracker
pause

section "2. Restore the snapshot in Record mode"
echo "This is the key new path: replay mode is supplied as part of /snapshot/load,"
echo "so restore-time VMClock state can be captured before the guest is resumed."
FC_LOG="$FC_LOG_REC"
launch_firecracker
api PUT /metrics "{\"metrics_path\":\"$METRICS\"}"
api PUT /snapshot/load \
    "{\"snapshot_path\":\"$SNAP_VMSTATE\",\"mem_backend\":{\"backend_path\":\"$SNAP_MEM\",\"backend_type\":\"File\"},\"resume_vm\":false,\"replay_mode\":\"Record\"}"
echo
echo "Replay metrics immediately after Record restore:"
replay_metrics
api PUT /replay/save "{\"path\":\"$LOG\"}"
echo
echo "Saved sidecar replay log:"
ls -la "$LOG"
echo
echo "Decoded log preview:"
dump_replay_log "$LOG" 8
kill_firecracker
pause

section "3. Restore the same snapshot in Replay mode with the saved log"
echo "We have not resumed the guest. Any replayed events here come from restore-time"
echo "guest-visible state, which is exactly what we wanted to prove for VMClock."
FC_LOG="$FC_LOG_REP"
launch_firecracker
api PUT /metrics "{\"metrics_path\":\"$METRICS\"}"
api PUT /snapshot/load \
    "{\"snapshot_path\":\"$SNAP_VMSTATE\",\"mem_backend\":{\"backend_path\":\"$SNAP_MEM\",\"backend_type\":\"File\"},\"resume_vm\":false,\"replay_mode\":\"Replay\",\"replay_log_path\":\"$LOG\"}"
echo
echo "Replay metrics immediately after Replay restore:"
replay_metrics
echo
echo "Reading the numbers:"
echo "  events_replayed > 0  -> restore-time replay consumed the saved VMClock event"
echo "  divergences = 0      -> the replay log matched the expected restore-time state"
kill_firecracker
pause

section "4. Proof A — a valid but empty replay log fails during snapshot restore"
make_empty_replay_log "$EMPTY_LOG"
echo "Empty log created at $EMPTY_LOG"
FC_LOG="$FC_LOG_EMPTY"
launch_firecracker
api PUT /metrics "{\"metrics_path\":\"$METRICS\"}"
api_status PUT /snapshot/load \
    "{\"snapshot_path\":\"$SNAP_VMSTATE\",\"mem_backend\":{\"backend_path\":\"$SNAP_MEM\",\"backend_type\":\"File\"},\"resume_vm\":false,\"replay_mode\":\"Replay\",\"replay_log_path\":\"$EMPTY_LOG\"}"
echo
echo "Tail of firecracker log:"
tail -n 20 "$FC_LOG" || true
kill_firecracker
pause

section "5. Proof B — tampering with the first event kind fails during restore"
tamper_first_event_kind "$LOG" "$TAMPERED_LOG"
echo
echo "Tampered log preview:"
dump_replay_log "$TAMPERED_LOG" 4
FC_LOG="$FC_LOG_TAMPER"
launch_firecracker
api PUT /metrics "{\"metrics_path\":\"$METRICS\"}"
api_status PUT /snapshot/load \
    "{\"snapshot_path\":\"$SNAP_VMSTATE\",\"mem_backend\":{\"backend_path\":\"$SNAP_MEM\",\"backend_type\":\"File\"},\"resume_vm\":false,\"replay_mode\":\"Replay\",\"replay_log_path\":\"$TAMPERED_LOG\"}"
echo
echo "Tail of firecracker log:"
tail -n 20 "$FC_LOG" || true
kill_firecracker

section "Done"
echo "What this demo proves:"
echo "  - Firecracker can record restore-time VMClock state into a sidecar replay log."
echo "  - Firecracker can consume that same VMClock event deterministically on replay."
echo "  - Replay mode and replay-log path now matter at snapshot-load time, not just after boot."
