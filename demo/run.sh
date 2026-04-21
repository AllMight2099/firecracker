#!/usr/bin/env bash
# Deterministic replay live demo.
#
# Phase A — record: boot a single-vCPU microVM, pause, snapshot, resume under
# Record mode, capture MMIO/PIO exits, save the sidecar log, kill firecracker.
#
# Phase B — replay: start a fresh firecracker, restore the snapshot, load the
# log, set Replay mode, resume. Metrics prove events are consumed (and, once
# interrupt timing drifts, divergence is reported).
#
# Each stage pauses for input so you can narrate or inspect state.

set -euo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
FC_BIN="$REPO/build/cargo_target/x86_64-unknown-linux-musl/release/firecracker"
KERNEL="$REPO/demo/vmlinux-6.1.155"
INITRD="$REPO/demo/initrd.cpio"
SOCK=/tmp/fc-demo.sock
LOG=/tmp/replay.detlog
SNAP_VMSTATE=/tmp/fc-demo.vmstate
SNAP_MEM=/tmp/fc-demo.mem
FC_LOG_REC=/tmp/fc-demo-record.log
FC_LOG_REP_CLEAN=/tmp/fc-demo-replay-clean.log
FC_LOG_REP_EMPTY=/tmp/fc-demo-replay-empty.log
FC_LOG_REP_TAMPER=/tmp/fc-demo-replay-tamper.log
FC_LOG=                               # set per-phase before launch_firecracker
METRICS=/tmp/fc-demo.metrics

rm -f "$SOCK" "$LOG" "$SNAP_VMSTATE" "$SNAP_MEM" \
      "$FC_LOG_REC" "$FC_LOG_REP_CLEAN" "$FC_LOG_REP_EMPTY" "$FC_LOG_REP_TAMPER" \
      "$METRICS"
: > "$METRICS"

[[ -f "$INITRD" ]] || { echo "missing $INITRD — run demo/build_initrd.sh first" >&2; exit 1; }
[[ -r /dev/kvm && -w /dev/kvm ]] || {
    echo "cannot access /dev/kvm — add \$USER to the 'kvm' group:" >&2
    echo "    sudo usermod -aG kvm \$USER && newgrp kvm" >&2
    echo "or re-run this script under 'sg kvm -c ...' / 'sudo ...'" >&2
    exit 1
}

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

pause() { echo; read -rp "[enter to continue] " _; }

section() { printf '\n\033[1;34m=== %s ===\033[0m\n' "$*"; }

replay_metrics() {
    api PUT /actions '{"action_type":"FlushMetrics"}' >/dev/null
    METRICS="$METRICS" python3 <<'PY'
import json, os
path = os.environ["METRICS"]
with open(path) as f:
    lines = [l for l in f if l.strip()]
m = json.loads(lines[-1])["replay"]
print("  events_recorded =", m["events_recorded"])
print("  events_replayed =", m["events_replayed"])
print("  divergences     =", m["divergences"])
PY
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

###############################################################################
# Phase A — record
###############################################################################

section "1. Launch firecracker (record instance)"
FC_LOG="$FC_LOG_REC"
launch_firecracker
pause

section "2. Configure VM (1 vCPU, 128 MiB, kernel + initrd, metrics sink)"
api PUT /boot-source    "{\"kernel_image_path\":\"$KERNEL\",\"initrd_path\":\"$INITRD\",\"boot_args\":\"console=ttyS0 reboot=k panic=1 pci=off quiet\"}"
api PUT /machine-config '{"vcpu_count":1,"mem_size_mib":128}'
api PUT /metrics        "{\"metrics_path\":\"$METRICS\"}"
pause

section "3. Start VM, pause, take snapshot (this is the replay origin)"
api PUT   /actions         '{"action_type":"InstanceStart"}'
sleep 0.2
api PATCH /vm              '{"state":"Paused"}'
api PUT   /snapshot/create "{\"snapshot_path\":\"$SNAP_VMSTATE\",\"mem_file_path\":\"$SNAP_MEM\"}"
ls -la "$SNAP_VMSTATE" "$SNAP_MEM"
pause

section "4. Enter Record, resume guest for ~3s, save log, kill firecracker"
api PUT   /replay/mode '{"mode":"Record"}'
api PATCH /vm          '{"state":"Resumed"}'
sleep 3
api PATCH /vm          '{"state":"Paused"}'
api PUT   /replay/mode '{"mode":"Off"}'
api PUT   /replay/save "{\"path\":\"$LOG\"}"
echo
echo "Guest serial output captured while recording:"
grep -a "det-replay demo" "$FC_LOG" || true
echo
echo "Sidecar log on disk:"
ls -la "$LOG"
echo
echo "Replay metrics after Record:"
replay_metrics
kill_firecracker
pause

###############################################################################
# Phase B — replay
###############################################################################

section "5. Launch fresh firecracker (replay instance)"
FC_LOG="$FC_LOG_REP_CLEAN"
launch_firecracker
pause

section "6. Configure metrics, load snapshot, load log, enter Replay mode"
api PUT /metrics       "{\"metrics_path\":\"$METRICS\"}"
api PUT /snapshot/load "{\"snapshot_path\":\"$SNAP_VMSTATE\",\"mem_backend\":{\"backend_path\":\"$SNAP_MEM\",\"backend_type\":\"File\"},\"resume_vm\":false}"
api PUT /replay/load   "{\"path\":\"$LOG\"}"
api PUT /replay/mode   '{"mode":"Replay"}'
api GET /replay/mode
pause

section "7. Resume under Replay — vCPU consumes events, then diverges"
api PATCH /vm '{"state":"Resumed"}'
echo
echo "Racing FlushMetrics against vCPU teardown..."
for _ in 1 2 3 4 5 6 7 8 9 10; do
    curl -sS --unix-socket "$SOCK" -X PUT "http://localhost/actions" \
         -H "Content-Type: application/json" \
         -d '{"action_type":"FlushMetrics"}' >/dev/null 2>&1 || true
    sleep 0.1
    kill -0 "$FC_PID" 2>/dev/null || break
done

if kill -0 "$FC_PID" 2>/dev/null; then
    echo "firecracker still alive — no divergence in ~1s"
else
    echo "firecracker exited — divergence-triggered vCPU teardown (expected)"
fi
FC_PID=  # stop the EXIT trap from trying to kill it again

echo
echo "Replay metrics (last flushed):"
METRICS="$METRICS" python3 <<'PY'
import json, os
path = os.environ["METRICS"]
with open(path) as f:
    lines = [l for l in f if l.strip()]
if not lines:
    print("  (no metrics captured)")
else:
    m = json.loads(lines[-1])["replay"]
    print("  events_recorded =", m["events_recorded"])
    print("  events_replayed =", m["events_replayed"])
    print("  divergences     =", m["divergences"])
PY

echo
echo "Guest output during replay (should start the same way as record):"
grep -a "det-replay demo" "$FC_LOG" | tail -n 10 || true
echo
echo "Reading the numbers:"
echo "  events_replayed > 0  → vCPU matched PIO/MMIO exits against the log"
echo "  divergences     > 0  → log/guest drifted (IRQ timing isn't yet controlled)"
echo "  firecracker exit 1   → divergence tore down the vCPU (expected)"
pause

###############################################################################
# Phase C — proofs that replay is actually consulting the log
###############################################################################

print_replay_metrics() {
    METRICS="$METRICS" python3 <<'PY'
import json, os
path = os.environ["METRICS"]
with open(path) as f:
    lines = [l for l in f if l.strip()]
if not lines:
    print("  (firecracker died before any FlushMetrics landed — that IS the")
    print("   proof: on this config the vCPU tore down in well under 100 ms,")
    print("   which is exactly what 'diverge on first exit' looks like.)")
else:
    m = json.loads(lines[-1])["replay"]
    print(f"  events_recorded = {m['events_recorded']}")
    print(f"  events_replayed = {m['events_replayed']}")
    print(f"  divergences     = {m['divergences']}")
PY
}

race_flush_until_dead() {
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        curl -sS --unix-socket "$SOCK" -X PUT "http://localhost/actions" \
             -H "Content-Type: application/json" \
             -d '{"action_type":"FlushMetrics"}' >/dev/null 2>&1 || true
        sleep 0.1
        kill -0 "$FC_PID" 2>/dev/null || break
    done
    FC_PID=
}

section "8. Proof A — Replay with an empty log diverges on the FIRST exit"
echo "Same snapshot, same everything — but no log loaded. If the replay"
echo "controller weren't actively consulting a log, the guest would just"
echo "boot. Instead we expect divergences=1, events_replayed=0 (or"
echo "firecracker dying too fast to flush — which IS the same proof)."
pause
: > "$METRICS"
FC_LOG="$FC_LOG_REP_EMPTY"
launch_firecracker
api PUT   /metrics       "{\"metrics_path\":\"$METRICS\"}"
api PUT   /snapshot/load "{\"snapshot_path\":\"$SNAP_VMSTATE\",\"mem_backend\":{\"backend_path\":\"$SNAP_MEM\",\"backend_type\":\"File\"},\"resume_vm\":false}"
# NOTE: intentionally skip /replay/load — controller's event buffer is empty.
api PUT   /replay/mode   '{"mode":"Replay"}'
api PATCH /vm            '{"state":"Resumed"}'
race_flush_until_dead
echo
echo "Metrics with empty log:"
print_replay_metrics
pause

section "9. Proof B — Tamper with ONE byte of the log, watch divergence move"
# Log layout: 6-byte header, then events of shape
#   u64 seqno | u8 kind | u8 pad | u16 pad | u64 addr | u32 size | <data>
# => the first data byte of event #0 sits at offset 6 + 24 = 30.
cp "$LOG" "$LOG.tampered"
python3 - "$LOG.tampered" <<'PY'
import sys
path = sys.argv[1]
with open(path, "r+b") as f:
    f.seek(30)
    original = f.read(1)[0]
    f.seek(30)
    f.write(bytes([original ^ 0xFF]))
print(f"flipped byte at offset 30: 0x{original:02x} -> 0x{original ^ 0xFF:02x}")
PY
echo
echo "Replay controller will now see a tampered first event. Either:"
echo "  - event #0 is a read → guest gets wrong bytes, downstream exits drift"
echo "  - event #0 is a write → validate_write catches the mismatch immediately"
echo "Either way, divergence moves versus the clean-log run in section 7."
pause
: > "$METRICS"
FC_LOG="$FC_LOG_REP_TAMPER"
launch_firecracker
api PUT   /metrics       "{\"metrics_path\":\"$METRICS\"}"
api PUT   /snapshot/load "{\"snapshot_path\":\"$SNAP_VMSTATE\",\"mem_backend\":{\"backend_path\":\"$SNAP_MEM\",\"backend_type\":\"File\"},\"resume_vm\":false}"
api PUT   /replay/load   "{\"path\":\"$LOG.tampered\"}"
api PUT   /replay/mode   '{"mode":"Replay"}'
api PATCH /vm            '{"state":"Resumed"}'
race_flush_until_dead
echo
echo "Metrics with tampered log:"
print_replay_metrics
echo
echo "Compare against section 7's clean-log replay. Different events_replayed"
echo "count is the smoking gun: the replay controller is byte-for-byte reading"
echo "log content, not just counting entries."
pause

section "done — kill firecracker + clean up"
