# Deterministic Replay Plan For Firecracker

## Goal

Build a deterministic replay prototype for a **single-vCPU** Firecracker microVM by
recording and replaying a narrow set of nondeterministic events after snapshot restore.

This is intentionally narrower than full dOS-style determinism. The immediate target is:

- same snapshot
- same workload
- same sequence of trapped exits
- same values returned to the guest
- same guest-observable output

## Why This Scope

The dOS paper is still the right conceptual guide: separate internal execution from external
nondeterministic inputs, then record and control the external inputs.

Inside Firecracker, the cleanest first boundary is not "all nondeterminism in a VM". It is:

- vCPU exit handling for `MmioRead`, `MmioWrite`, `IoIn`, `IoOut`
- snapshot save/load
- a small replay controller carried in snapshot state or sidecar log state

The repo already exposes these hooks cleanly in:

- `src/vmm/src/vstate/vcpu.rs`
- `src/vmm/src/arch/x86_64/vcpu.rs`
- `src/vmm/src/lib.rs`
- `src/vmm/src/builder.rs`
- `src/vmm/src/device_manager/persist.rs`

## Key Feasibility Findings

### What is straightforward

- Firecracker already pauses vCPUs cleanly before snapshotting.
- Firecracker already serializes VM, vCPU, and device state.
- MMIO and PIO exits are centralized in the vCPU exit path.
- There are existing snapshot, pause/resume, and VMClock integration tests we can extend.

### What is risky for an MVP

- Exact retired-instruction counting is not wired into Firecracker today.
- Firecracker currently disables or limits PMU exposure by default, so a PMU-based timeline is not
  the best first milestone.
- Exact interrupt reinjection at a retired-instruction count is much harder than replaying exit data.
- RDTSC trapping is not currently present in this codebase as a ready-made Firecracker feature.

## Recommended MVP

Implement **ordered exit replay** first, not exact instruction-count replay.

The replay contract for MVP:

1. Start from the same snapshot.
2. Restrict the VM to one vCPU.
3. Restrict or disable the noisiest asynchronous nondeterminism.
4. Record the ordered stream of trapped exits and returned data.
5. Restore from the snapshot and replay those same results in the same order.
6. Compare guest-visible output and log divergence points.

This gives a working deterministic replay prototype much faster, and it creates the scaffolding
needed for later instruction-counted interrupt replay.

## MVP Scope

### Include

- `MmioRead`
- `MmioWrite`
- `IoIn`
- `IoOut`
- replay mode selection
- per-run sidecar log
- divergence detection
- snapshot-to-replay workflow

### Exclude for first milestone

- exact interrupt timing replay
- PMU retired-instruction timeline
- RDTSC replay
- multi-vCPU determinism
- unrestricted networking
- entropy devices as replay targets

## Execution Phases

### Phase 0: Make nondeterminism small enough to study

Target configuration:

- 1 vCPU only
- fixed machine config
- start from snapshot
- no guest workload that depends on wall clock progress
- initially avoid networked workloads like `etcd`
- initially avoid randomness-heavy workloads

Use a simple guest program first:

- deterministic userspace loop reading from a virtio/MMIO-backed device
- or a workload that writes stable output to a file/serial console

### Phase 1: Add replay mode and event log format

Add a replay subsystem with two modes:

- `Record`
- `Replay`

Suggested event schema:

```rust
enum DetExitKind {
    MmioRead,
    MmioWrite,
    IoIn,
    IoOut,
}

struct DetExitEvent {
    seqno: u64,
    kind: DetExitKind,
    addr: u64,
    size: u32,
    data: Vec<u8>,
}
```

Notes:

- `seqno` is a scalar logical clock, not an instruction count.
- For the single-vCPU MVP, this scalar clock is enough because trapped exits already form a total
  order.
- For reads, `data` is what Firecracker returned to the guest.
- For writes, `data` is what the guest wrote.
- In replay mode, any mismatch in kind/address/size/write-data should hard fail.

Why not vector clocks:

- with one vCPU there is no concurrent partial order to preserve
- a vector clock adds complexity without improving replay fidelity for the MVP
- if the project later expands to multi-vCPU replay, we can revisit per-vCPU clocks or richer
  happens-before metadata then

### Phase 2: Intercept exits at the vCPU boundary

Primary hook points:

- `src/vmm/src/vstate/vcpu.rs`
- `src/vmm/src/arch/x86_64/vcpu.rs`

Plan:

- wrap handling of `VcpuExit::MmioRead` and `VcpuExit::MmioWrite`
- wrap handling of `VcpuExit::IoIn` and `VcpuExit::IoOut`
- on record:
  - let normal emulation happen
  - capture the result into the exit log
- on replay:
  - validate the next expected event
  - for reads, fill the exit buffer from the log instead of invoking the device bus
  - for writes, validate and optionally skip actual device execution only if safe

Important safety rule:

- for writes, do **not** skip executing the write initially
- validate first, then still execute the device path unless we later prove it is safe to elide

That keeps device state evolution aligned with normal execution.

### Phase 3: Carry replay state across snapshot/restore

Primary hook points:

- `src/vmm/src/lib.rs`
- `src/vmm/src/builder.rs`
- snapshot persistence code under `src/vmm/src/persist.rs` and related persist modules

Plan:

- add a replay configuration to VMM runtime state
- store metadata such as current replay mode and log cursor
- keep bulk event data in a sidecar file rather than stuffing it into the existing snapshot blob

Suggested first design:

- snapshot remains unchanged
- record/replay log is stored in a separate file
- snapshot restore API accepts an optional replay-log path plus mode

This is lower risk than changing Firecracker's existing snapshot format too early.

Replay log storage details:

- keep the deterministic replay log as a host-side sidecar file, separate from `vmstate` and guest
  memory snapshot files
- use a simple binary event stream rather than embedding replay events into the Firecracker snapshot
  blob
- for the first prototype, it is reasonable to keep events in memory during recording and flush them
  to the sidecar file when the VM is paused, snapshotted, or explicitly exported
- later, if needed, recording can be upgraded to streaming appends for long-running executions

Suggested first layout next to a snapshot:

- `vmstate`
- `mem`
- `replay.detlog`

Why a sidecar file:

- avoids changing Firecracker snapshot compatibility too early
- keeps the replay format easy to inspect and iterate on
- lets replay logs evolve independently from snapshot serialization
- makes it easier to compare, archive, or regenerate replay traces without touching VM state files

### Phase 4: Add divergence reporting

On replay mismatch, emit:

- expected event
- actual event
- sequence number
- VM state summary if cheap to obtain

This is essential for debugging and for proving the prototype is meaningful.

### Phase 5: Build the first end-to-end deterministic test

Workflow:

1. boot VM
2. run controlled guest program
3. pause VM
4. create snapshot
5. continue in record mode for N events
6. stop VM and save log
7. restore snapshot in replay mode
8. replay N events
9. compare guest output byte-for-byte

## Suggested Initial Workloads

Start with workloads simpler than Redis or etcd.

Best first candidates:

- a guest userspace test program that repeatedly reads a known MMIO/PIO-exposed source
- a serial-console-driven workload with deterministic command sequence
- a guest program that writes a transcript to disk for byte-for-byte comparison

Defer:

- `etcd`
- network-heavy Redis scenarios

Reason:

- host network delivery timing adds a lot of asynchronous nondeterminism before the replay
  machinery is mature enough to control it.

## Verification Strategy

### Level 1: Unit tests

Add Rust unit tests around the new replay engine:

- record appends correct event entries
- replay validates sequence correctly
- replay fails on mismatched address/kind/size/data
- replay read substitutes device-returned bytes correctly

### Level 2: Integration tests in existing framework

Extend Firecracker tests with a new deterministic replay suite using the existing Python harness.

Good places to pattern-match:

- `tests/integration_tests/functional/test_snapshot_basic.py`
- `tests/integration_tests/functional/test_pause_resume.py`
- `tests/integration_tests/functional/test_vmclock.py`

First integration assertions:

- record run and replay run produce identical guest file output
- replay consumes the entire log with no leftover entries
- replay fails if the guest workload is modified

### Level 3: Negative testing

Deliberately perturb replay:

- wrong log file
- truncated log
- mismatched machine config
- mismatched snapshot

Expected result:

- deterministic failure with a clear divergence report

### Level 4: Stability testing

Run the same record/replay test 20 to 100 times and check:

- no divergence across identical runs
- same output hash each time
- same event count each time

## Metrics To Track

- total trapped events
- reads recorded
- writes validated
- replay mismatches
- output hash of guest artifact
- replay success rate across repeated runs
- slowdown versus normal snapshot restore

## What To Do About Interrupts Later

Only after the exit-sequence replay works:

1. identify one interrupt source to control, likely virtio IRQ delivery or VMClock-style
   notification
2. add a logical-time marker richer than `seqno`
3. evaluate whether exact instruction count is really required, or whether replaying at the next
   deterministic trap boundary is sufficient
4. only then revisit PMU or hardware-breakpoint support

## Expected Code Touch Points

- `src/vmm/src/vstate/vcpu.rs`
- `src/vmm/src/arch/x86_64/vcpu.rs`
- `src/vmm/src/lib.rs`
- `src/vmm/src/builder.rs`
- likely a new replay module under `src/vmm/src/`
- new integration tests under `tests/integration_tests/functional/`

## Concrete Milestone Plan

### Milestone 1

Add a replay module and log format, with pure unit tests only.

Success means:

- event log can be recorded and replay-validated in Rust tests

### Milestone 2

Hook MMIO/PIO exits in record mode only.

Success means:

- Firecracker runs normally
- a sidecar log is produced
- event stream looks stable across repeated runs of the same snapshot/workload

### Milestone 3

Hook replay for MMIO/PIO reads and validate writes.

Success means:

- restored VM reproduces the same guest-visible output from the same snapshot

### Milestone 4

Add divergence diagnostics and repeated-run tests.

Success means:

- failures are explainable
- replay can be regression-tested automatically

### Milestone 5

Expand scope carefully to one additional source of nondeterminism.

Candidates:

- RTC/time-related reads
- one interrupt class
- limited networking

## Bottom Line

The project is feasible if we treat it as a staged deterministic replay system rather than jumping
directly to full instruction-counted hypervisor determinism.

The highest-probability path is:

- single vCPU
- snapshot-based replay
- ordered exit log first
- exact interrupt timing later
