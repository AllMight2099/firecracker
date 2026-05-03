Deterministic Replay Demo Notes

Goal

Show the first end-to-end replay proof beyond MMIO/PIO unit tests:
Firecracker now records and replays guest-visible VMClock state during snapshot restore.

What this demo does not claim

- It does not prove that arbitrary guest `clock_gettime()` calls are deterministic yet.
- It does not prove full concurrent-program replay yet.
- It does not prove interrupt timing replay yet.

What it does prove

- The replay controller can operate at snapshot-restore time, not only in the vCPU run loop.
- A guest-visible clock-related interface can be recorded and replayed deterministically.
- Replay mode and replay-log selection can now influence snapshot restore itself.

Demo story

1. Boot a tiny one-vCPU VM and create a paused snapshot.
   This gives us the fixed replay origin.

2. Restore that snapshot in `Record` mode.
   During restore, Firecracker updates the VMClock device and records the exact guest-visible
   state into the sidecar log.

3. Save and inspect the replay log.
   The important thing to point out is the `VMCLOCK` event in the decoded log preview.

4. Restore the exact same snapshot in `Replay` mode with that log.
   We do not need to resume the guest. If `events_replayed > 0` immediately after restore,
   replay already consulted the sidecar log at restore time.

5. Show the negative proofs.
   An empty log fails because restore cannot find the expected VMClock event.
   A tampered log fails because the first event no longer matches the expected restore-time kind.

Key lines to emphasize

- `events_recorded > 0` after the Record restore:
  Restore itself produced replayable state.

- `events_replayed > 0` after the Replay restore:
  Replay happened before the guest resumed.

- `divergences = 0` on the clean replay:
  The logged restore-time VMClock state matched.

- Snapshot-load failure with empty or tampered log:
  Replay is actively consulting the log, not ignoring it.

Why this matters to the larger project

- MMIO/PIO replay proved the run-loop path.
- VMClock replay proves the restore-time guest-visible-state path.
- Together they form the pattern we need for broader deterministic replay:
  some nondeterminism appears during execution, and some appears while reconstructing guest state.

Reasonable Q&A answer

If someone asks whether this means application time syscalls are deterministic now:

"Not yet. This is a stepping stone. We now control one guest-visible clock-related interface at
restore time. The next step is to target the actual guest clock source used by Linux time APIs."
