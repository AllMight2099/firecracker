# Deterministic Replay for Firecracker — Speaker Notes

## Slide 1 — Domain & problem: why this matters

**On the slide**
- Modern serverless runs on microVMs (Firecracker → AWS Lambda, Fargate, Fly.io)
- Bugs inside microVMs are hard to reproduce
- Root cause: *nondeterminism* — same program, different runs, different behavior
- Debugging, post-mortem, and security analysis all need reliable reproduction

**Speaker notes (~45s)**
> "Firecracker runs hundreds of millions of untrusted workloads a day. When a Lambda function hits a rare bug, the engineer trying to reproduce it on their laptop can run the exact same code, with the exact same inputs, and get a completely different execution path. That's not because the code is flaky — it's because the VM itself is nondeterministic. Timers, interrupts, device reads — every one of these depends on host-level state that the guest can't see. So we lose bugs. We lose them silently, and when they matter we can't get them back. Deterministic replay is the standard fix: record once, replay arbitrarily many times, get the same execution every time. It's how Mozilla's `rr` made Firefox debuggable. We want the same for microVMs."

**Inspiration note (optional, ~15s)**
> "One of the ideas that pushed this project forward was dOS: the notion that execution should be something you can capture, reason about, and replay instead of a one-shot event that disappears the moment it happens. I didn't reimplement dOS here, but it shaped the mindset. The Firecracker version narrows that idea to the microVM boundary: if I can make the guest's conversation with virtual hardware reproducible, I have a practical foothold on deterministic replay."

---

## Slide 2 — Domain-specific challenges

**On the slide**
- Every source of nondeterminism crosses the guest/host boundary as a **vCPU exit**
- Exit types to cover: PIO, MMIO, IRQs, RDTSC, CPUID
- IRQs are the hardest: delivered at host-chosen instruction boundaries, not guest-chosen
- Snapshotting must define a common replay origin
- Performance: recording can't 10× boot time

**Speaker notes (~50s)**
> "A VM's guest is a black box executing code. The only way nondeterminism leaks in is through the hypervisor boundary — KVM exits control back to the host on specific instructions, and those are the moments external state enters the guest. There are roughly five categories: port I/O, memory-mapped I/O, interrupts, the timestamp counter, and CPUID. Each one is a separate engineering problem. PIO and MMIO are the easiest — they're synchronous request/response, you can tap them cleanly. IRQs are brutally hard because an interrupt arrives *between* guest instructions at a moment the host chose, not the guest, and to replay you need to re-inject it at the *exact same instruction*. That needs hardware performance counters. The other hard part is that replay has to start from a known state — which means pairing replay with a snapshot."

---

## Slide 3 — Proposed solution (high level)

**On the slide** (diagram)

```
 RECORD                         REPLAY
 snapshot ──┐                   ┌── snapshot
            ↓                   ↓
    ┌── vCPU run ──┐      ┌── vCPU run ──┐
    │  PIO/MMIO    │      │  PIO/MMIO    │
    │   exit       │      │   exit       │
    └──────┬───────┘      └──────┬───────┘
           │                     │
     real device            log lookup
           │                     │
      data ─┤                data ◄── log
           ↓                     ↓
      log ◄── append         vCPU resumes
```

- `/replay/mode` toggles Off / Record / Replay
- Binary sidecar log format: `DET0` magic + ordered event stream
- Paired with existing Firecracker snapshot machinery

**Speaker notes (~45s)**
> "The prototype plugs into the vCPU exit path. In Record mode, when the guest does a port I/O or an MMIO access, the real device services it like normal — we just tap the answer and append it, with a logical clock, to an in-memory log. In Replay mode, the device doesn't run. The guest still executes, but every PIO/MMIO read is answered from the log, and every write is validated against the log. If the guest's behavior drifts from what we recorded — the next exit doesn't match the next log entry — we report divergence and tear the vCPU down. No silent drift. The whole thing is 400 lines of Rust sitting at the KVM exit handler."

---

## Slide 4 — Domain-specific insights

**On the slide**
- Nondeterminism has a *finite*, *enumerable* interface — KVM's exit loop
- **Read / write asymmetry**: reads are *synthesized* from log, writes are *validated* against log
- Scalar logical clock is sufficient for single-vCPU
- Snapshot + log are a *pair*; neither is useful alone
- Divergence detection is a feature, not a failure mode

**Speaker notes (~60s)**
> "Four insights shaped the design. First: you don't have to go hunting for nondeterminism. KVM already enumerates every exit — five categories, a handful of opcodes each — so the scope is finite. We attack it one exit type at a time. Second: reads and writes aren't symmetric. A read pulls nondeterministic data *into* the guest. A write doesn't — the guest wrote what the guest wrote. So replay fabricates reads from the log, but for writes it just checks the guest produced the right bytes. That turns the write path into a *verification* of determinism, which is what gives us divergence detection for free. Third: with a single vCPU, there's nothing concurrent about the exits — a scalar counter is enough to order them. We don't need vector clocks until multi-core. Fourth: we didn't build replay-origin infrastructure from scratch. Firecracker already snapshots VMs. We just insist that a replay run load the same snapshot the recording started from."

---

## Slide 5 — Preliminary results (demo slide)

**On the slide**
- Three-phase demo: Record → Replay → Proofs
- Metrics captured per run:
  - `events_recorded` — how many exits the tap saw
  - `events_replayed` — how many exits the replay controller serviced
  - `divergences` — how many times the guest drifted
- Empty-log test: divergence on *first* exit
- Tampered-log test: one-byte edit changes replay outcome

**Speaker notes / live demo (~2 min)**
> "I'll step through the demo quickly.
>
> **[Phase A]** We boot Firecracker with a tiny guest, pause mid-boot, and take a snapshot. That snapshot is the replay origin. Then we turn on Record, resume for three seconds, pause again, and save the sidecar log. That file is not a console transcript. It's a bus-level event trace: an ordered stream of PIO and MMIO exits, each with kind, address, size, and data.
>
> **[Phase B]** Kill Firecracker. Start a fresh one. Restore the *same* snapshot. Load the log. Turn on Replay. Resume.
>
> What you see: the replay controller walks that event stream from the snapshot point forward. Reads are synthesized from the log, writes are validated against it, and `events_replayed` counts how many exits matched exactly. The decoded preview in the script lets you point at the exact prefix of events that replay consumed before drift. Then, at some later event, IRQ timing differs from the original run, the guest reaches a different next exit than the one in the log, and replay reports divergence and tears the vCPU down.
>
> **[Proofs]** To prove replay is actually consulting the log and not just passing through: first, replay with an *empty* log diverges on the first exit, not event 50. Second, if we flip one byte of the saved log and re-replay, the events_replayed count changes. Same snapshot, same Firecracker, one byte different in the log — completely different outcome. That's the byte-level evidence that the log is driving replay."

---

## Slide 6 — What was unexpectedly hard

**On the slide**
- IRQ timing replay (gave up for v1) — needs `perf_event` instruction counting
- Firecracker device model is *too minimal* for a good non-determinism showcase
- Test-artifact infrastructure was a rabbit hole
- Writing the log serialization was easy; deciding *what* to record was the hard part

**Speaker notes (~45s)**
> "A few things that didn't go as planned.
>
> IRQ replay is where I hit the wall. To pin an interrupt to the right instruction, you need to count retired instructions between the snapshot and the delivery point — that means wiring a hardware perf counter into the KVM run loop and steering the vCPU to break at the exact count. It's a real engineering project, not a weekend. So v1 controls PIO/MMIO only, and accepts IRQ drift as the expected divergence source.
>
> Second: Firecracker's device model is so trimmed down that I couldn't easily demo replay reproducing a flashy user-visible value from userspace. Most legacy I/O ports return stubs. What I can show today is bus-level determinism: the same snapshot plus the same sidecar causes the same prefix of PIO/MMIO exits to be consumed, and changing or deleting the sidecar changes where replay fails.
>
> Third: the Firecracker integration-test harness expects downloaded CI artifacts plus jailer plus networking. I burned a few hours trying to make tests run before giving up and writing a pure shell demo against my own kernel and a custom initrd. The shell script turned out to be more useful for presenting anyway."

---

## Slide 7 — What's next

**On the slide**
- IRQ replay via `perf_event` instruction counting
- TSC offset control (record RDTSC values, replay via MSR intercept)
- Full Phase 5 tests (record/replay equivalence, not just metric-level)
- Upstream: move `replay.rs` behind a feature flag, land integration tests

**Speaker notes (~20s)**
> "The next milestone is IRQ pinning, which unlocks byte-for-byte guest-output equivalence from the snapshot point onward. Once that lands, the Phase 5 tests go from 'the bus-level event stream matched for N exits' to 'the guest produces the exact same observable behavior twice.' That's the actual deliverable for real-world debuggability."

---

## Timing / delivery tips

- Slides 1–4: talk at a steady clip, no demo — maybe 3 minutes total.
- Slide 5 is the heavy one. Run `./demo/run.sh` in a pre-staged terminal; most section transitions are `[enter to continue]`, so pacing is in your hands. Budget 2–3 minutes.
- Slide 6 is your opportunity to show self-awareness — it's the single highest-signal slide for audience trust. Don't skip it.
- If you get a "why not just snapshot and restore instead of recording?" question, the answer is: snapshots give you *one* state; replay gives you a *trajectory*. Debugging a race condition means stepping through the trajectory that led to the bad state, not just observing it.
