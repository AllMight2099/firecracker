# Deterministic Replay Project Glossary

Grouped roughly by "where the concept lives," not alphabetical, so related
terms read next to each other.

## 1. Determinism vocabulary (the project's goal)

- **Deterministic replay** — the property that re-running a system produces
  the exact same execution given the same starting state and the same
  recorded inputs.
- **dOS (Deterministic OS)** — Bergan et al., OSDI 2010. The conceptual
  ancestor of this project. Treated a group of processes as a unit and
  recorded everything crossing the unit's boundary; replayed by feeding
  those inputs back in logical order.
- **DPG (Deterministic Process Group)** — dOS's unit. Inside, threads run
  deterministically; outside, all inputs get recorded.
  *Our analog:* the whole guest VM.
- **DMP (Deterministic MultiProcessing)** — dOS's mechanism for making
  thread interleavings inside a DPG deterministic.
  *Our analog:* eventually, instruction-position-pinned IRQ injection. We
  don't have it yet.
- **Boundary input** — anything externally-controlled crossing into the
  deterministic unit. In dOS: syscalls. In our project: vCPU exits.
- **Record mode / Replay mode / Off** — the three states of our
  `ReplayController`. Record passively taps boundary inputs; Replay
  actively services them from a log; Off is no-op.
- **Replay origin** — the snapshot from which a replay run starts. Both
  record and replay must use the same origin or the first input already
  mismatches.
- **Sidecar log** — the binary file alongside the snapshot that holds the
  recorded boundary inputs. Format begins with `DET0` magic + 16-bit
  version.
- **Scalar logical clock (`seqno`)** — a single integer counter incremented
  per recorded event. Sufficient for single-vCPU because exits already form
  a total order; vector clocks only matter once you go multi-vCPU.
- **Divergence** — replay observes an exit that doesn't match the next
  event in the log. Reported as a structured error and increments the
  `divergences` metric.
- **Deterministic prefix** — how far replay survives before drifting.
  Today, bounded by uncontrolled IRQ timing.

## 2. Firecracker + KVM substrate

- **Firecracker** — the AWS VMM (virtual machine monitor) we're modifying.
  Single-process, runs on top of Linux/KVM.
- **KVM** — the Linux kernel virtualization subsystem. Exposes `/dev/kvm`
  and a set of ioctls for creating VMs and vCPUs.
- **vCPU** — virtual CPU. A KVM-managed thread that executes guest
  instructions. Single-vCPU is our entire scope.
- **VMM** (in our code) — the `Vmm` struct. Holds the VM, vCPUs, device
  manager, and (recently) the replay controller.
- **VcpuFd / VmFd** — kvm-ioctls Rust wrappers around `/dev/kvm` file
  descriptors. Where we call `KVM_RUN`, `KVM_SET_MSRS`, etc.
- **`KVM_RUN` / `VcpuExit`** — the loop where the vCPU runs guest code
  until something traps. Returns a `VcpuExit` variant naming why we exited.
- **`VcpuExit::IoIn` / `IoOut` / `MmioRead` / `MmioWrite`** — the four
  classic boundary inputs we record/replay. PIO and MMIO accesses to
  device registers.
- **`VcpuExit::X86Rdmsr` / `X86Wrmsr`** — userspace MSR exits. Only fire
  if `KVM_CAP_X86_USER_SPACE_MSR` is enabled and the MSR is in our filter.
  Not yet wired in our exit handler.
- **`kvm_run` page** — shared mmap between KVM and userspace where exit
  info lives. `VcpuExit` returned from `run()` borrows from this page.
- **ArchVm** — Firecracker's per-arch wrapper around `VmFd`. We added
  `enable_user_space_msr` and `set_replay_msr_filter` on it.
- **Peripherals** — per-vCPU struct holding the PIO/MMIO buses + (now) the
  replay controller.

## 3. x86 / hardware concepts that surface in our work

- **MMIO** (memory-mapped I/O) — guest does a load/store to a magic
  physical address that's wired to a virtual device. Traps as
  `VcpuExit::Mmio*`.
- **PIO** (port I/O) — guest executes `in`/`out` to an I/O port (16-bit
  address space). Traps as `VcpuExit::Io*`.
- **IRQ** (interrupt request) — asynchronous notification injected into
  the vCPU.
- **GSI** (global system interrupt) — Linux/ACPI-level numeric IRQ
  identifier. KVM associates eventfds with GSIs via `register_irqfd`.
- **LAPIC** (local APIC) — per-CPU interrupt controller. KVM emulates it
  in-kernel; the LAPIC timer never crosses the userspace boundary, which
  is why our IRQ observability can't see it.
- **TSC** (time-stamp counter) — a per-core monotonic counter incremented
  at CPU clock frequency. Read via `rdtsc` (instruction) or `rdmsr 0x10`
  (MSR).
- **`rdtsc`** — instruction. By default doesn't trap to userspace under
  KVM. **The core blocker for runtime time replay.**
- **`MSR_IA32_TSC`** (0x10) — the TSC exposed as an MSR. Trappable via
  `KVM_CAP_X86_USER_SPACE_MSR` + filter, but rarely used by modern Linux.
- **`MSR_KVM_SYSTEM_TIME_NEW`** (0x4b564d01) /
  **`MSR_KVM_WALL_CLOCK_NEW`** (0x4b564d00) — kvmclock setup MSRs, written
  once by the guest at SMP boot to point at a memory page KVM writes time
  into.
- **kvmclock / pvclock** — paravirtual clock. Guest reads time from a
  memory page the host updates. The reads themselves don't trap (mmap'd
  memory).
- **CPUID** — instruction returning CPU feature info. Mostly deterministic;
  some leaves are time-related.
- **PMU** (performance monitoring unit) — hardware counters for
  instructions retired, cache misses, etc. Accessed via `perf_event_open`.
  **The foundation needed for instruction-position-precise IRQ replay.**
- **VMCS** (VM Control Structure) — Intel hardware structure controlling
  what causes VM exits. Has an `RDTSC_EXITING` bit but KVM doesn't expose
  it to userspace.

## 4. KVM userspace surface we touched or considered

- **`KVM_CAP_X86_USER_SPACE_MSR`** — capability that, when enabled,
  forwards specified MSR reads/writes to userspace as
  `VcpuExit::X86Rdmsr/X86Wrmsr`.
- **`KVM_X86_SET_MSR_FILTER`** — ioctl listing which MSRs to forward to
  userspace. We wrote a raw wrapper because kvm-ioctls 0.24 doesn't
  expose it.
- **`KVM_MSR_EXIT_REASON_FILTER`** / **`UNKNOWN`** / **`INVAL`** — bits
  describing which categories of MSR access trap to userspace.
- **`kvm_msr_filter` / `kvm_msr_filter_range`** — the C structs the ioctl
  takes. Each range covers a slice of MSR indices with a bitmap.
- **irqfd** — KVM's mechanism for injecting an interrupt by writing 1 to
  a registered EventFd. The host writes; KVM observes and injects. Used
  everywhere in Firecracker for device → guest IRQs.
- **`register_irqfd`** — `VmFd` method binding an EventFd to a GSI.
- **`KVM_IRQ_LINE`** — synchronous IRQ injection ioctl (unused in
  Firecracker; everything goes through irqfd).

## 5. Our replay primitives (in `src/vmm/src/replay.rs`)

- **`ReplayController`** — the central struct. Holds the mode, the events
  vector, the cursor, and the seqno counter.
- **`ReplayMode`** — enum: `Off`, `Record`, `Replay`.
- **`DetExitKind`** — the kinds of events we know how to record. Currently
  nine variants: `MmioRead`, `MmioWrite`, `IoIn`, `IoOut`, `VmClockState`,
  `Rdtsc`, `MsrRead`, `MsrWrite`, `IrqInjection`.
- **`DetExitEvent`** — one row in the log: `{seqno, kind, addr, size,
  data}`.
- **`ReplayDivergence`** — error type with variants `LogExhausted`,
  `KindOrAddrMismatch`, `SizeMismatch`, `WriteDataMismatch`. Carries
  `recent_irqs` for diagnostics.
- **`record(kind, addr, data)`** — append a new event. Gated on
  `mode() == Record`.
- **`consume_read(kind, addr, &mut data)`** — replay path: pop the next
  event, verify it matches kind+addr+size, copy logged bytes into the
  caller's buffer.
- **`validate_write(kind, addr, data)`** — replay path: pop next event,
  verify kind+addr+size+data matches.
- **`record_irq` / `record_msr_read` / `record_msr_write` /
  `record_rdtsc`** — typed helpers around `record()`.
- **`IRQ_SOURCE_LEGACY` / `IRQ_SOURCE_VIRTIO_CONFIG` /
  `IRQ_SOURCE_VIRTIO_VRING`** — source-tag constants stored in the `addr`
  field of an `IrqInjection` event.
- **`RecentIrqEvents`** — the recent-IRQ context attached to divergence
  errors.
- **`is_diagnostic_only()`** — predicate on `DetExitKind`. Diagnostic-only
  kinds (currently just `IrqInjection`) are skipped by `next_replay_event`
  so they don't break replay matching.
- **`next_replay_event()`** — pops the next non-diagnostic event from the
  cursor; alongside, harvests the recent IRQs for diagnostics.
- **`save_to_writer` / `load_from_reader`** — serialize/deserialize the
  events vec to/from a sidecar file.
- **`register_global_replay_controller(...)` /
  `record_irq_via_global(...)`** — the prototype-only `Weak`-backed
  global registry for IRQ recording from device code.
- **`replay_cursor` / `next_seqno`** — atomic counters; cursor is the
  read position during replay, `next_seqno` is the write counter during
  record.

## 6. Snapshot / restore terms

- **Snapshot** — paused VM state serialized to disk. Two files: a
  `vmstate` blob (CPU + device state) and a `mem` file (guest physical
  memory).
- **`/snapshot/create`** — API to take a snapshot.
- **`/snapshot/load`** — API to restore. Now accepts `replay_mode` and
  `replay_log_path` fields.
- **`replay_mode` / `replay_log_path`** — fields we added to
  `LoadSnapshotConfig` so restore can be replay-aware.
- **`do_post_restore`** — device hook called after the snapshot's bytes
  are applied. VMClock's `do_post_restore` is where we plug in
  restore-time replay.
- **`MicrovmState`** — the in-memory deserialized snapshot.
- **`builder.rs`** — where the Vmm is constructed both from-fresh and
  from-snapshot. The snapshot-restore path is where we register the
  replay controller globally.

## 7. Guest-visible state we record/replay

- **VMClock (`vmclock_abi`)** — a small ACPI device exposing a
  guest-readable struct with `seq_count`, `disruption_marker`,
  `vm_generation_counter`. State is bumped on every restore. Replay locks
  the bumped values to match record.
- **vmgenid** — VM generation ID. Like VMClock but simpler: a single
  128-bit number that bumps on snapshot restore. Triggers an ACPI
  notification IRQ.
- **8250 UART** — the legacy serial controller at PIO `0x3F8`. Where
  guest `printf` ends up. Source of `IRQ_SOURCE_LEGACY` Serial RX events
  (none in our demo).
- **i8042** — legacy keyboard controller stub. Mostly unused in our demo.
- **virtio-mmio** — the MMIO transport for virtio devices. Source of
  `IRQ_SOURCE_VIRTIO_CONFIG` and `IRQ_SOURCE_VIRTIO_VRING` events.
- **`IrqTrigger`** — the virtio-side struct that routes a
  `VirtioInterrupt` to an EventFd write.
- **`EventFdTrigger`** — the legacy/ACPI-side struct doing the same for
  non-virtio devices. Single `Trigger` impl in
  `src/vmm/src/devices/legacy/mod.rs`.

## 8. Demo / verification artifacts

- **`demo/hello.c`** — the toy guest. PID 1 inside the initrd. Prints 7
  lines + spins.
- **`demo/initrd.cpio`** — the cpio archive containing `/init`
  (= compiled hello.c).
- **`demo/run.sh`** — the demo driver. Sections 1–5: snapshot → record →
  replay → empty-log → tampered-log.
- **`/tmp/replay.detlog`** — sidecar log from the demo's record run.
- **`/metrics` API** — Firecracker endpoint that names a file the metrics
  will be written to.
- **`FlushMetrics`** — `/actions` action that triggers a one-shot
  serialize of `METRICS` to the configured file. Uses delta semantics
  (fetches `current - last_flushed`, then resets).
- **`SharedIncMetric`** — counter type used by `events_recorded` /
  `events_replayed` / `divergences`. Drains on flush.
- **`events_recorded` / `events_replayed` / `divergences`** — the three
  replay metrics.

## 9. Out of scope but referenced

- **jailer** — Firecracker's chroot/seccomp wrapper. We don't use it.
- **virtio-blk / virtio-net** — block and network devices. Not in our
  demo guest.
- **Multi-vCPU determinism** — explicitly out of scope per the plan.
- **`rr` (Mozilla)** — the canonical user-space deterministic replay
  tool. Conceptual reference for what's possible at the process level,
  but doesn't apply at the VMM level.
