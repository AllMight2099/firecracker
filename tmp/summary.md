# Deterministic Replay Prototype — Progress Summary

Working branch: `deteterminstic-firecracker`
Baseline: `main`
Scope: single-vCPU MMIO/PIO exit record-and-replay, per `deterministic-replay-plan.md`.

## What is in place

### 1. Replay module — `src/vmm/src/replay.rs`

A standalone replay engine with no device-layer coupling.

- `ReplayMode` — `Off` / `Record` / `Replay`.
- `DetExitKind` — `MmioRead`, `MmioWrite`, `IoIn`, `IoOut`.
- `DetExitEvent` — `{ seqno, kind, addr, size, data }`.
- `ReplayController`
  - `record(kind, addr, data)` — appends an event during `Record` (no-op otherwise); `seqno` comes from an atomic counter.
  - `consume_read(kind, addr, &mut data)` — during `Replay`, fills the caller's buffer with logged bytes after validating kind/addr/size.
  - `validate_write(kind, addr, data)` — validates kind/addr/data against the log; caller still executes the device write.
  - `save_to_writer` / `load_from_reader` — binary sidecar format with `DET0` magic + `u16` version + `{seqno, kind, reserved, addr, size, data}` records.
  - `reset` — clears events and both counters.
- `ReplayDivergence` — rich error enum covering `LogExhausted`, `KindOrAddrMismatch`, `SizeMismatch`, `WriteDataMismatch`. Carries enough context for divergence reporting.
- `ReplayLogError` — `Io`, `InvalidMagic`, `UnsupportedVersion`, `InvalidExitKind`.

### 2. Exit interception

- **MMIO** — [src/vmm/src/vstate/vcpu.rs](src/vmm/src/vstate/vcpu.rs)
  - `VcpuExit::MmioRead` and `MmioWrite` handlers dispatch on mode:
    - `Off`: normal bus emulation.
    - `Record`: normal emulation, then record the exit.
    - `Replay`: reads fill the exit buffer from the log; writes validate then still execute (per the plan's safety rule — do not skip writes for MVP).
  - `VcpuError::ReplayDivergence(ReplayDivergence)` was added.
- **PIO** — [src/vmm/src/arch/x86_64/vcpu.rs](src/vmm/src/arch/x86_64/vcpu.rs)
  - `VcpuExit::IoIn` / `IoOut` handlers follow the same mode-dispatch pattern.

### 3. VMM plumbing

- [src/vmm/src/lib.rs](src/vmm/src/lib.rs)
  - `Vmm::replay_controller: Arc<ReplayController>` shared with vCPUs.
  - Methods: `replay_mode`, `set_replay_mode`, `reset_replay_log`, `save_replay_log(path)`, `load_replay_log(path)`.
  - `VmmError::ReplayLogIo` / `ReplayLog` variants for surfacing sidecar errors.
- [src/vmm/src/builder.rs](src/vmm/src/builder.rs) — vCPU construction receives the shared controller.

### 4. Tests — all passing

13 unit tests under `src/vmm/src/replay.rs`:

- Record gating on mode, scalar `seqno` ordering, `reset` semantics.
- `consume_read` — success, kind mismatch, log-exhausted.
- `validate_write` — success, data-mismatch.
- Log format round-trip via in-memory cursor.
- **End-to-end via tempfile** (`test_record_save_load_replay_end_to_end`) — interleaved MMIO/PIO exits recorded, saved through a real file, loaded into a fresh controller, replayed with byte-for-byte verification, and asserts `LogExhausted` on overflow.
- **Divergence on modified workload** (`test_divergence_on_modified_workload`) — changing the replay-time request surfaces `KindOrAddrMismatch` at seqno 0.
- **Truncated log** — surfaces as `ReplayLogError::Io`.
- **Bad magic** — surfaces as `InvalidMagic`.

## Plan status vs. `deterministic-replay-plan.md`

| Phase | Status |
|-------|--------|
| Phase 1 — replay module + log format | Done |
| Phase 2 — exit interception (record + replay) | Done |
| Phase 3 — snapshot integration / HTTP API | **Partial** — VMM-level `save_replay_log`/`load_replay_log` done; HTTP VmmAction/parsed_request wiring deferred |
| Phase 4 — divergence reporting | Done in structured form (`ReplayDivergence`); guest-output comparison deferred to Phase 5 |
| Phase 5 — end-to-end Python integration test | Pending |

## Deferred / next steps

- **HTTP API surface for replay control** — `VmmAction` variants (`SetReplayMode`, `SaveReplayLog`, `LoadReplayLog`, …), dispatch arms in `rpc_interface.rs`, a new `src/firecracker/src/api_server/request/replay.rs`, and routing in `parsed_request.rs`. Skipped until we know the schema we actually want from Phase 5 usage.
- **Optional** `LoadSnapshotParams` extension with `replay_log_path` / `replay_mode` for one-shot restore-with-replay.
- **Phase 5 end-to-end test** — boot VM, run a deterministic guest workload that reads/writes a known MMIO/PIO-exposed device, snapshot, record through restore, replay, diff guest-visible output. Needs a minimal deterministic guest program first.
- **Out of scope (Phase 5+)** — interrupt timing replay, PMU retired-instruction timeline, RDTSC replay, multi-vCPU, networking.

## Files touched vs. `main`

```
src/vmm/src/arch/x86_64/vcpu.rs   +53    PIO exit interception
src/vmm/src/builder.rs             +4    plumb controller to vCPUs
src/vmm/src/lib.rs                 +6    VMM-level replay API
src/vmm/src/replay.rs             +162   replay module (committed)
src/vmm/src/vstate/vcpu.rs        +55    MMIO exit interception
.gitignore                         +2
```

Uncommitted additions on top of the last commit (`bdf7b843a`):

```
src/vmm/src/arch/x86_64/vcpu.rs    PIO replay dispatch (read/write injection + validation)
src/vmm/src/lib.rs                 save_replay_log / load_replay_log / reset / set_mode
src/vmm/src/replay.rs              ReplayDivergence, consume_read, validate_write, 9 new tests
src/vmm/src/vstate/vcpu.rs         MMIO replay dispatch; VcpuError::ReplayDivergence
```
