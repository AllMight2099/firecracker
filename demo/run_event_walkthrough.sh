#!/usr/bin/env bash
# Real replay event walkthrough.
#
# This demo uses a real Firecracker microVM rather than a synthetic
# ReplayController-only example. It isolates two views:
#   1. restore-time recording only, so we can explain the exact first events,
#   2. a short resumed recording window, so we can show what comes next.
#
# The goal is to answer two questions:
#   - what sequence of events is actually recorded?
#   - why does Firecracker record that exact sequence?

set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
FC_BIN="$REPO/build/cargo_target/x86_64-unknown-linux-musl/debug/firecracker"
KERNEL="$REPO/demo/vmlinux-6.1.155"
INITRD="$REPO/demo/initrd.cpio"

SOCK=/tmp/fc-event-walk.sock
SNAP_VMSTATE=/tmp/fc-event-walk.vmstate
SNAP_MEM=/tmp/fc-event-walk.mem
METRICS=/tmp/fc-event-walk.metrics

RESTORE_LOG=/tmp/fc-event-walk-restore.detlog
RUNTIME_LOG=/tmp/fc-event-walk-runtime.detlog

FC_LOG_BASE=/tmp/fc-event-walk-base.log
FC_LOG_RESTORE=/tmp/fc-event-walk-restore.log
FC_LOG_RUNTIME=/tmp/fc-event-walk-runtime.log
FC_LOG_REPLAY=/tmp/fc-event-walk-replay.log

FC_LOG=

rm -f "$SOCK" "$SNAP_VMSTATE" "$SNAP_MEM" "$METRICS" \
      "$RESTORE_LOG" "$RUNTIME_LOG" \
      "$FC_LOG_BASE" "$FC_LOG_RESTORE" "$FC_LOG_RUNTIME" "$FC_LOG_REPLAY"
: > "$METRICS"

[[ -x "$FC_BIN" ]] || {
    echo "missing firecracker binary at $FC_BIN" >&2
    echo "build it first with: tools/devtool -y build --debug" >&2
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

pause() {
    [[ "${NO_PAUSE:-0}" == "1" ]] && return
    echo
    read -rp "[enter to continue] " _
}

section() { printf '\n\033[1;34m=== %s ===\033[0m\n' "$*"; }

api() {
    local method=$1 path=$2 body=${3:-}
    if [[ -n "${FC_PID:-}" ]] && ! kill -0 "$FC_PID" 2>/dev/null; then
        echo >&2
        echo "!! firecracker (pid=$FC_PID) is dead — tail of $FC_LOG:" >&2
        tail -n 40 "$FC_LOG" >&2 || true
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

clear_metrics() {
    : > "$METRICS"
}

print_last_metrics() {
    METRICS="$METRICS" python3 <<'PY'
import json
import os

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

replay_metrics() {
    api PUT /actions '{"action_type":"FlushMetrics"}' >/dev/null
    print_last_metrics
}

read_replay_metric() {
    local field=$1
    METRICS="$METRICS" FIELD="$field" python3 <<'PY'
import json
import os

path = os.environ["METRICS"]
field = os.environ["FIELD"]
with open(path) as f:
    lines = [l for l in f if l.strip()]
if not lines:
    print(0)
else:
    print(json.loads(lines[-1])["replay"][field])
PY
}

dump_replay_log() {
    local path=$1
    local limit=${2:-16}
    local consumed=${3:-0}
    REPLAY_LOG_PATH="$path" REPLAY_LOG_LIMIT="$limit" REPLAY_LOG_CONSUMED="$consumed" python3 <<'PY'
import os
import struct
import sys

path = os.environ["REPLAY_LOG_PATH"]
limit = int(os.environ["REPLAY_LOG_LIMIT"])
consumed = int(os.environ["REPLAY_LOG_CONSUMED"])

kind_names = {
    0: "MMIO_READ",
    1: "MMIO_WRITE",
    2: "PIO_IN",
    3: "PIO_OUT",
    4: "VMCLOCK",
    5: "RDTSC",
    6: "MSR_READ",
    7: "MSR_WRITE",
    8: "IRQ",
}

irq_names = {
    0: "legacy",
    1: "virtio_config",
    2: "virtio_vring",
}

uart_regs = {
    0x3f8: "UART_DATA/DLL",
    0x3f9: "UART_IER/DLM",
    0x3fa: "UART_IIR/FCR",
    0x3fb: "UART_LCR",
    0x3fc: "UART_MCR",
    0x3fd: "UART_LSR",
    0x3fe: "UART_MSR",
    0x3ff: "UART_SCR",
}

def fmt_bytes(buf: bytes, preview: int = 16) -> str:
    if not buf:
        return "-"
    shown = " ".join(f"{b:02x}" for b in buf[:preview])
    if len(buf) > preview:
        shown += " ..."
    return shown

def explain_event(seqno: int, kind: int, addr: int) -> str:
    if seqno == 0 and kind == 8 and addr == 0:
        return "  <- vmgenid post-restore IRQ"
    if seqno == 1 and kind == 4:
        return "  <- vmclock bytes recorded/replayed"
    if seqno == 2 and kind == 8 and addr == 0:
        return "  <- vmclock post-restore IRQ"
    if kind in (2, 3) and addr in uart_regs:
        return f"  <- {uart_regs[addr]}"
    return ""

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
    marker = "=> " if seqno < consumed else "   "
    name = kind_names.get(kind, f"KIND_{kind}")
    if kind == 8:
        payload = struct.unpack_from("<I", data, 0)[0] if len(data) >= 4 else 0
        detail = f"source={irq_names.get(addr, f'tag_{addr}'):<13} payload=0x{payload:08x}"
    else:
        detail = f"addr=0x{addr:x}  size={size:<3} data=[{fmt_bytes(data)}]"
    print(f"{marker}seq={seqno:03d}  {name:<10} {detail}{explain_event(seqno, kind, addr)}")
if len(events) > limit:
    print(f"  ... {len(events) - limit} more events not shown")
if consumed > limit:
    print(f"  ... plus {consumed - limit} more consumed events beyond the preview")
PY
}

summarize_replay_log() {
    local path=$1
    REPLAY_LOG_PATH="$path" python3 <<'PY'
import collections
import os
import struct

path = os.environ["REPLAY_LOG_PATH"]
kind_names = {
    0: "MMIO_READ",
    1: "MMIO_WRITE",
    2: "PIO_IN",
    3: "PIO_OUT",
    4: "VMCLOCK",
    8: "IRQ",
}
irq_names = {0: "legacy", 1: "virtio_config", 2: "virtio_vring"}

with open(path, "rb") as f:
    blob = f.read()

offset = 6
kind_counts = collections.Counter()
irq_counts = collections.Counter()
while offset < len(blob):
    _seqno = struct.unpack_from("<Q", blob, offset)[0]
    offset += 8
    kind = blob[offset]
    offset += 1
    offset += 3
    addr = struct.unpack_from("<Q", blob, offset)[0]
    offset += 8
    size = struct.unpack_from("<I", blob, offset)[0]
    offset += 4
    offset += size
    kind_counts[kind_names.get(kind, f"KIND_{kind}")] += 1
    if kind == 8:
        irq_counts[irq_names.get(addr, f"tag_{addr}")] += 1

print("  Event counts by kind:")
for name in sorted(kind_counts):
    print(f"    {name:<10} {kind_counts[name]}")
if irq_counts:
    print("  IRQ counts by source tag:")
    for name in sorted(irq_counts):
        print(f"    {name:<13} {irq_counts[name]}")
PY
}

print_restore_order_story() {
    cat <<'EOF'
Expected restore-time order from the code:
  1. ACPIDeviceManager::restore() activates VMGenID.
  2. do_post_restore_vmgenid() notifies the guest.
     EventFdTrigger::trigger() records IrqInjection(source=legacy).
  3. ACPIDeviceManager::restore() activates VMClock.
  4. do_post_restore_vmclock() records VmClockState.
  5. do_post_restore_vmclock() notifies the guest.
     EventFdTrigger::trigger() records IrqInjection(source=legacy).

That order comes from:
  - src/vmm/src/device_manager/persist.rs
  - src/vmm/src/devices/acpi/vmgenid.rs
  - src/vmm/src/devices/acpi/vmclock.rs
  - src/vmm/src/devices/legacy/mod.rs
EOF
}

grep_guest_output() {
    local path=$1
    echo "Guest serial lines seen in $path:"
    if ! grep -n "det-replay demo:" "$path"; then
        echo "  (no guest transcript lines found in this window)"
    fi
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

section "2. Record restore-time events only"
echo "This phase keeps the VM paused after restore, so the log contains only"
echo "the restore-time sequence and no guest runtime MMIO/PIO traffic yet."
clear_metrics
FC_LOG="$FC_LOG_RESTORE"
launch_firecracker
api PUT /metrics "{\"metrics_path\":\"$METRICS\"}"
api PUT /snapshot/load \
    "{\"snapshot_path\":\"$SNAP_VMSTATE\",\"mem_backend\":{\"backend_path\":\"$SNAP_MEM\",\"backend_type\":\"File\"},\"resume_vm\":false,\"replay_mode\":\"Record\"}"
api PUT /replay/save "{\"path\":\"$RESTORE_LOG\"}" >/dev/null
echo
echo "Replay metrics after restore-only recording:"
replay_metrics
echo
echo "Decoded restore-only log:"
dump_replay_log "$RESTORE_LOG" 8
echo
print_restore_order_story
kill_firecracker
pause

section "3. Record a short resumed window"
echo "Now we restore the same snapshot in Record mode, resume briefly,"
echo "pause again, and save the resulting sidecar log."
clear_metrics
FC_LOG="$FC_LOG_RUNTIME"
launch_firecracker
api PUT /metrics "{\"metrics_path\":\"$METRICS\"}"
api PUT /snapshot/load \
    "{\"snapshot_path\":\"$SNAP_VMSTATE\",\"mem_backend\":{\"backend_path\":\"$SNAP_MEM\",\"backend_type\":\"File\"},\"resume_vm\":false,\"replay_mode\":\"Record\"}"
api PATCH /vm '{"state":"Resumed"}'
sleep 0.35
api PATCH /vm '{"state":"Paused"}'
api PUT /replay/save "{\"path\":\"$RUNTIME_LOG\"}" >/dev/null
echo
echo "Replay metrics after the resumed recording window:"
replay_metrics
echo
echo "Guest output during the recording window:"
grep_guest_output "$FC_LOG_RUNTIME"
echo
echo "Decoded runtime log prefix:"
dump_replay_log "$RUNTIME_LOG" 18
echo
summarize_replay_log "$RUNTIME_LOG"
echo
echo "How to read this prefix:"
echo "  - seq 0..2 are still the restore-time VMGenID/VMClock sequence."
echo "  - later PIO events come from the legacy UART at ports 0x3f8..0x3ff."
echo "  - this tiny hello guest mostly exercises serial PIO, so MMIO may be absent"
echo "    in the early window even though the replay engine supports MMIO too."
kill_firecracker
pause

section "4. Replay the same resumed window"
echo "Replay starts from the same snapshot and consumes the saved sidecar in order."
echo "If the guest stays aligned, events_replayed grows. If timing drifts, Firecracker"
echo "reports divergence and tears the vCPU down."
clear_metrics
FC_LOG="$FC_LOG_REPLAY"
launch_firecracker
api PUT /metrics "{\"metrics_path\":\"$METRICS\"}"
api PUT /snapshot/load \
    "{\"snapshot_path\":\"$SNAP_VMSTATE\",\"mem_backend\":{\"backend_path\":\"$SNAP_MEM\",\"backend_type\":\"File\"},\"resume_vm\":false,\"replay_mode\":\"Replay\",\"replay_log_path\":\"$RUNTIME_LOG\"}"
api PATCH /vm '{"state":"Resumed"}' >/dev/null || true
sleep 0.5
if [[ -n "${FC_PID:-}" ]] && kill -0 "$FC_PID" 2>/dev/null; then
    api PUT /actions '{"action_type":"FlushMetrics"}' >/dev/null || true
    api PATCH /vm '{"state":"Paused"}' >/dev/null || true
else
    echo "firecracker exited during replay (likely divergence-triggered teardown)"
fi
echo
echo "Replay metrics (last flushed):"
print_last_metrics
REPLAYED=$(read_replay_metric events_replayed)
echo
echo "Decoded replay prefix (=> = consumed before pause/divergence):"
dump_replay_log "$RUNTIME_LOG" 18 "$REPLAYED"
echo
echo "Tail of replay log:"
tail -n 20 "$FC_LOG_REPLAY" || true
kill_firecracker

section "Done"
echo "What this walkthrough shows:"
echo "  - The first recorded events are not arbitrary: they come from ACPI restore order."
echo "  - VMGenID notification records the first legacy IRQ."
echo "  - VMClock records its guest-visible bytes, then triggers the second legacy IRQ."
echo "  - Once the guest resumes, the next visible activity in this workload is UART PIO."
echo "  - Replay consumes the same ordered log until execution drifts."
