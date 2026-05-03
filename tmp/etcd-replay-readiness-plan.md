# Plan: Get the VM Ready to Demo Deterministic etcd (Minimal)

## Goal

Get the Firecracker stack to a state where we can run etcd inside the microVM,
record a window of its execution, restore from the same snapshot under replay,
and **show a deterministic prefix** of that execution. The demo's deliverable is
a reproducible "this many events of etcd execution match across runs, here is
the divergence point and why."

This plan does *not* try to make etcd byte-for-byte deterministic for arbitrary
workloads. It tries to make a minimal etcd demo possible using what we already
have plus the smallest set of additions.

## What "minimal" means here

- **Single-node etcd, single vCPU.** No peers, no quorum, no multi-vCPU.
- **In-memory storage.** etcd runs against a tmpfs data directory — no virtio-blk replay needed.
- **No TLS.** etcd's HTTPS path pulls in entropy and timing we don't want.
- **Loopback only.** etcd binds to `127.0.0.1`; no virtio-net replay needed for the first demo.
- **Snapshot taken after etcd is fully booted and idle.** All boot-time MSR/clock
  setup is captured by the snapshot, so we don't have to replay it.
- **Replay window is the post-snapshot idle period plus one client request.**
  We are not trying to replay hours of etcd activity; we want a clean prefix
  that ends in a real, observable divergence.

## What the demo will and won't prove

**It will prove:**

- etcd boots and runs inside Firecracker.
- The same snapshot, restored under Record then under Replay, produces an
  identical prefix of MMIO/PIO exits.
- Restore-time guest-visible state (VMClock) is identical between record and
  replay runs, so etcd's perception of the world starts from the same anchor.
- Eventual divergence is **detected and reported** by the replay controller,
  not silently ignored.

**It will not prove:**

- That etcd's user-visible output is byte-for-byte identical.
- That arbitrary etcd workloads can be replayed.
- That replay survives indefinitely.

The honest framing for the audience: "this is the first multi-component,
non-trivial workload we've replayed; here is the deterministic prefix we got
out of the existing primitives, and here are the gaps that bound how far it goes."

## Where we are right now

Already shipped:

- Ordered MMIO/PIO record + replay with divergence detection.
- VMClock state replay at snapshot restore.
- Sidecar replay log format.
- Snapshot-load API accepts `replay_mode` + `replay_log_path`.
- Interrupt observability through legacy + virtio trigger paths, with recent-IRQ
  context attached to replay divergence reports.
- Working hello.c demo proving the above.

In flight (from the previous milestone):

- MSR record/replay event types and helpers in `replay.rs`.
- Raw `KVM_X86_SET_MSR_FILTER` wrapper and cap-enable helper on `ArchVm`.
- Exit handling for `VcpuExit::X86Rdmsr/X86Wrmsr` is **not** wired.

## What etcd actually needs from the replay system

| etcd dependency | Source of nondeterminism | Existing coverage | Minimal-demo strategy |
| --- | --- | --- | --- |
| Process boot | initrd / rootfs bring-up | covered by snapshot | snapshot post-boot, never replay this |
| Time (heartbeats, election timeouts) | guest `rdtsc` instruction | **none** — KVM doesn't expose rdtsc trap to userspace | accept drift; report it |
| Time (kvmclock page) | one-shot kvmclock MSR setup at boot | implicit via snapshot | snapshot after this happens |
| Time (boundary anchor) | VMClock state at restore | shipped | reused as-is |
| Disk I/O (WAL, snapshots) | virtio-blk request streams | **none** | run etcd against tmpfs — no disk I/O |
| Network I/O | virtio-net RX/TX | **none** | loopback-only, no virtio-net |
| Entropy | TLS, request IDs, raft IDs | **none** | disable TLS; pin random seed via env if exposed |
| Interrupts (timer, virtio) | KVM-injected | userspace IRQ observability only | accept drift; report it |
| Console / serial | UART PIO | shipped (PIO replay) | reused as-is |

So for the minimal etcd demo, there is **no new replay primitive we strictly
need** to obtain a deterministic prefix. The shipped interrupt observability
work does not pin IRQ timing, but it does make the eventual drift point much
more explainable.

## Stages

### Stage 1 — Guest image with etcd

The current `demo/initrd.cpio` is just hello.c. We need a guest image that can
run etcd. Two approaches:

**Approach A: extend initrd**

- Statically link a small etcd build (recent etcd builds aren't fully static
  but the official release tarball is close enough on glibc).
- Add a tiny `/init` shell or compiled C binary that:
  - mounts `/proc`, `/sys`, `/dev`, tmpfs at `/var/lib/etcd`
  - sets `HOME=/root`, etc.
  - exec's etcd with `--data-dir=/var/lib/etcd --listen-client-urls=http://127.0.0.1:2379 --advertise-client-urls=http://127.0.0.1:2379 --listen-peer-urls=http://127.0.0.1:2380 --initial-cluster=default=http://127.0.0.1:2380 --initial-advertise-peer-urls=http://127.0.0.1:2380 --log-level=error`
- The current `demo/build_initrd.sh` extends naturally — add etcd to the cpio.

**Approach B: build a real rootfs image**

- A small Alpine or Buildroot image with etcd + minimal init.
- Heavier but more realistic. Probably overkill for a minimal demo.

**Recommendation: Approach A.** Initrd is enough for a single-binary workload
and matches the existing demo scaffolding.

Acceptance: `./demo/run-etcd.sh` boots Firecracker, etcd reaches "ready to
serve" state on the serial console, and a host-side `curl --unix-socket` (or
host network if we add a tap) returns version info from the etcd HTTP API.

### Stage 2 — Networking strategy

For a minimal demo, etcd is talking to itself on loopback. The host doesn't
need to reach etcd's HTTP port. The simplest scaffolding:

- **Don't add a tap interface.** Skip `add_net_iface`.
- **Don't reach etcd from the host.** The demo verifies etcd from *inside*
  the guest by having the init script run a few `etcdctl put / get` commands
  against `127.0.0.1:2379` and print results to the serial console.
- **Snapshot after etcdctl confirms etcd is healthy.** That puts us in a known
  good state with no in-flight peer traffic.

If later we want to drive etcd from outside, we'd add a virtio-net tap and
need virtio-net replay. Out of scope for the minimal demo.

### Stage 3 — Snapshot point

The snapshot needs to be taken *after* etcd is fully running and idle, so:

- All kvmclock setup MSR writes are already done.
- Initial `etcdctl put`/`get` round-trips are done.
- No client request is in flight.
- The serial console transcript shows "etcd ready" and at least one successful
  client round-trip.

This is the "replay origin." Same snapshot, two runs (Record + Replay).

### Stage 4 — Recording window

A short window after restore is what we record:

- Restore the snapshot under Record mode.
- Resume the vCPU.
- Issue exactly one client request from inside the guest (e.g., a follow-up
  `etcdctl put foo bar` invoked by the init script after some sentinel).
- Pause as soon as the response comes back.
- Save the replay log.

That window contains: the client RPC, etcd's response, and whatever timer-tick
activity happens during it. The replay log is going to be much larger than the
hello.c demo (probably tens of thousands of events), but it's still bounded.

### Stage 5 — Replay run + divergence reporting

- Restore the same snapshot under Replay mode with the saved log.
- Resume.
- Wait either for the log to be consumed end-to-end (best case) or for a
  divergence (expected case, since IRQ timing isn't pinned).
- Flush metrics.
- Print:
  - `events_replayed` — how far the replay got
  - `divergences` — how many mismatches were caught (likely 1)
  - the divergence error itself (kind, address, expected vs actual)
  - the seqno at which it occurred, as a percentage of the log

That last percentage is the headline number: "etcd replayed deterministically
through X% of the recorded window before drifting."

### Stage 6 — Interrupt observability (the only new replay primitive)

Currently a divergence prints expected/actual MMIO/PIO. For etcd that's
useful but not explanatory. With interrupt observability the divergence
report can include the last K injected interrupts before the mismatch, which
is what a viewer would need to understand why etcd's vCPU drifted.

This is the single near-term replay-system change that most materially improved
the existing replay diagnostics, and it has independent value for any future
workload.

If time is tight, **the etcd demo works without this** — it just won't have
a satisfying explanation for the drift point.

## Decision: what to do with the in-flight MSR work?

For the etcd-readiness target, the MSR-trap work has very low value:

- etcd's runtime time path is **rdtsc**, which KVM doesn't expose to userspace.
- The kvmclock setup MSRs (`MSR_KVM_SYSTEM_TIME_NEW`, `MSR_KVM_WALL_CLOCK_NEW`)
  are written **once at SMP boot**. Our snapshot is taken *after* boot, so
  during replay the guest never re-issues those writes — there are no MSR
  exits to replay.
- The `MSR_IA32_TSC` rdmsr path is also rare in modern Linux.

So for an etcd-snapshot-after-boot demo, MSR replay would:
- record zero events
- diverge zero times
- contribute nothing visible

**Recommendation:** stop finishing the MSR exit-handling pipeline. Keep the
already-landed pieces (event types, ioctl wrapper, cap helpers, unit tests) —
they're cheap to carry and useful for a future workload that does post-boot
MSR setup. Update the main plan to reflect this state. Do not wire the exit
handler or the snapshot-load integration yet.

Move the freed-up time to:

1. Stage 1 — etcd guest image. Highest leverage. No replay work, just packaging.
2. Packaging and workload-level etcd scaffolding now that interrupt observability
   is already in place.

Skip Stage 6 if presentation timing demands; the etcd demo still works without it.

## Milestone checklist

### Pre-demo readiness

- [ ] guest image: extend `demo/build_initrd.sh` to include etcd binary
- [ ] guest init: minimal `/init` (or shell script) that mounts pseudo-fs and exec's etcd
- [ ] verify etcd boots inside Firecracker and reports "ready" on serial console
- [ ] verify etcdctl round-trip works inside the guest (init script runs `etcdctl put` + `get` and prints results)
- [ ] new demo script `demo/run-etcd.sh` that mirrors `demo/run.sh`'s structure: boot, snapshot, record window, save log, replay, report

### Optional but valuable

- [x] interrupt observability (record IRQ injections, surface in divergence reports)
- [ ] document the divergence point measured for the recorded etcdctl round-trip

### Not required for this demo

- virtio-blk replay
- virtio-net replay
- exact IRQ-timing replay
- rdtsc trapping
- MSR exit-handling pipeline (the partially-finished work)

## Risks

- **etcd startup time.** etcd takes seconds to boot inside an unaccelerated
  KVM guest. The demo will have a long pause between section 1 and 3. Acceptable.
- **etcd binary size.** Increases initrd size from ~800 KB to ~30 MB. Memory
  config needs to be bumped from 128 MiB to ~256 MiB.
- **Replay log size.** Tens of thousands of events instead of dozens. The
  binary log format scales fine; the python decoder we use in the demo will
  truncate output at K events, which is what we want.
- **Divergence happens too early.** If replay diverges within the first few
  hundred events (before etcd has serviced the etcdctl request), the demo
  reads as "this barely works." Mitigation: pick a snapshot point and recording
  window that maximizes deterministic-prefix length. Possibly reduce the
  recording window to the bare minimum (just one client RPC).
- **etcd refuses to run in initrd.** Some setup may be missing (`/etc/hosts`,
  `/etc/nsswitch.conf`, etc.). Detect early during Stage 1 and add as needed.

## Bottom line

The etcd-ready demo is mostly a **packaging exercise** plus **one optional
replay primitive (interrupt observability)**. The replay system already has
enough machinery for a minimal demo. The right next move is:

1. Land the partial MSR work as scaffolding, don't finish wiring it.
2. Build the etcd guest image and a parallel `run-etcd.sh`.
3. Optionally add interrupt observability so the divergence report explains
   why the deterministic prefix ends.
