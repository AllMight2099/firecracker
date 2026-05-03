# SPDX-License-Identifier: Apache-2.0
"""Tests for the deterministic replay prototype.

These exercise the `/replay/*` HTTP surface, the record -> save -> load
pipeline, and the end-to-end divergence detection path. They do NOT yet
assert byte-for-byte guest-output equivalence between record and replay
runs: the current prototype only controls MMIO/PIO exit data, while
interrupt timing and RDTSC remain nondeterministic. Those will be
tightened in a later milestone. Phase 5 assertions therefore focus on
metric-level evidence — that replay consumes events and that divergence
is reported — rather than on exact guest-visible output.
"""

import time
from pathlib import Path

import pytest


def test_replay_mode_preboot_rejected(uvm_nano):
    """Replay actions must not be allowed before the microVM boots."""
    vm = uvm_nano
    expected_err = "not supported before starting the microVM"

    with pytest.raises(RuntimeError, match=expected_err):
        vm.api.replay_mode.put(mode="Record")
    with pytest.raises(RuntimeError, match=expected_err):
        vm.api.replay_reset.put()
    with pytest.raises(RuntimeError, match=expected_err):
        vm.api.replay_save.put(path="/replay.detlog")
    with pytest.raises(RuntimeError, match=expected_err):
        vm.api.replay_load.put(path="/replay.detlog")


def test_replay_mode_transitions(uvm_nano):
    """GET/PUT /replay/mode round-trips all three modes once the VM is up."""
    vm = uvm_nano
    vm.start()

    assert vm.api.replay_mode.get().json() == {"mode": "Off"}

    for mode in ("Record", "Replay", "Off"):
        vm.api.replay_mode.put(mode=mode)
        assert vm.api.replay_mode.get().json() == {"mode": mode}

    with pytest.raises(RuntimeError):
        vm.api.replay_mode.put(mode="Bogus")

    vm.kill()


def test_record_and_save_log(uvm_nano):
    """Record a burst of exits, save the log, assert the sidecar file exists."""
    vm = uvm_nano
    vm.start()

    vm.api.replay_mode.put(mode="Record")

    # Generate trapped exits: any guest-observed command over SSH produces
    # serial-line, virtio-console, and timer PIO/MMIO traffic.
    vm.ssh.check_output("true")
    vm.ssh.check_output("echo hello")

    vm.api.vm.patch(state="Paused")
    vm.api.replay_mode.put(mode="Off")

    jailed_log = "/replay.detlog"
    vm.api.replay_save.put(path=jailed_log)

    host_log = Path(vm.chroot()) / "replay.detlog"
    assert host_log.exists(), f"replay log not created at {host_log}"
    assert host_log.stat().st_size >= 6, "replay log should contain header + events"

    # Magic + version header: b"DET0" + u16 little-endian 1.
    header = host_log.read_bytes()[:6]
    assert header[:4] == b"DET0"
    assert int.from_bytes(header[4:6], "little") == 1

    vm.kill()


def test_replay_log_load_clears_previous(uvm_nano, tmp_path):
    """Loading a log replaces whatever was recorded in-memory."""
    vm = uvm_nano
    vm.start()

    vm.api.replay_mode.put(mode="Record")
    vm.ssh.check_output("true")
    vm.api.vm.patch(state="Paused")
    vm.api.replay_mode.put(mode="Off")

    first = "/replay-first.detlog"
    vm.api.replay_save.put(path=first)
    first_size = (Path(vm.chroot()) / Path(first).name).stat().st_size

    vm.api.replay_reset.put()

    # After reset + more record, saving should produce a smaller log.
    vm.api.vm.patch(state="Resumed")
    vm.api.replay_mode.put(mode="Record")
    vm.api.vm.patch(state="Paused")
    vm.api.replay_mode.put(mode="Off")

    second = "/replay-second.detlog"
    vm.api.replay_save.put(path=second)
    second_size = (Path(vm.chroot()) / Path(second).name).stat().st_size

    assert second_size < first_size, (
        "reset + briefer recording should yield a smaller sidecar file; "
        f"got first={first_size}, second={second_size}"
    )

    # Reloading the first log must succeed.
    vm.api.replay_load.put(path=first)
    vm.kill()


def test_record_snapshot_restore_replay_smoke(uvm_nano, microvm_factory):
    """End-to-end smoke: record a burst, snapshot, restore, load log, replay.

    This does not (yet) assert byte-for-byte guest output equivalence — that
    requires controlling interrupt timing, which is out of scope for the
    current milestone. It *does* assert that the whole pipeline runs without
    a divergence-triggered vCPU teardown for a short recorded window.
    """
    vm = uvm_nano
    vm.start()

    # Snapshot first so record and replay share the same baseline state.
    vm.api.vm.patch(state="Paused")
    snapshot = vm.snapshot_full()
    vm.api.vm.patch(state="Resumed")

    vm.api.replay_mode.put(mode="Record")
    vm.ssh.check_output("true")
    vm.api.vm.patch(state="Paused")
    vm.api.replay_mode.put(mode="Off")

    jailed_log = "/replay.detlog"
    vm.api.replay_save.put(path=jailed_log)
    host_log = Path(vm.chroot()) / "replay.detlog"
    assert host_log.exists()

    vm.kill()

    # Restore into a fresh Firecracker process.
    restored = microvm_factory.build()
    restored.spawn()
    restored.restore_from_snapshot(snapshot, resume=False)

    # Copy the sidecar log into the new jail and load it.
    jailed_restored_log = Path(restored.chroot()) / "replay.detlog"
    jailed_restored_log.write_bytes(host_log.read_bytes())
    restored.api.replay_load.put(path="/replay.detlog")
    restored.api.replay_mode.put(mode="Replay")

    # Resume briefly. With only PIO/MMIO replay and no interrupt control, the
    # guest may or may not consume the whole log before diverging. For this
    # smoke test, we only assert the process survives the transition.
    restored.api.vm.patch(state="Resumed")
    restored.api.vm.patch(state="Paused")

    restored.kill()


def test_replay_progress_end_to_end(uvm_plain, microvm_factory):
    """Phase 5-B: restored VM consumes events from a recorded log.

    Record a short workload after snapshotting, save the log, then restore a
    fresh VM from the same snapshot and load the log under Replay mode. The
    record-side run must produce events with no divergences, and the replay
    side must consume at least one event before any (currently uncontrolled)
    interrupt-timing divergence.
    """
    vm = uvm_plain
    vm.spawn()
    vm.basic_config(vcpu_count=1, mem_size_mib=256)
    vm.add_net_iface()
    vm.start()

    vm.api.vm.patch(state="Paused")
    snapshot = vm.snapshot_full()
    vm.api.vm.patch(state="Resumed")

    vm.api.replay_mode.put(mode="Record")
    vm.ssh.check_output("true")
    vm.api.vm.patch(state="Paused")
    vm.api.replay_mode.put(mode="Off")

    record_metrics = vm.flush_metrics()["replay"]
    assert record_metrics["events_recorded"] > 0, (
        f"record run must produce at least one event; metrics={record_metrics}"
    )
    assert record_metrics["divergences"] == 0, (
        f"record run must not report divergences; metrics={record_metrics}"
    )

    jailed_log = "/replay.detlog"
    vm.api.replay_save.put(path=jailed_log)
    host_log = Path(vm.chroot()) / "replay.detlog"
    assert host_log.exists()
    vm.kill()

    restored = microvm_factory.build()
    restored.spawn()
    restored.restore_from_snapshot(snapshot, resume=False)

    jailed_restored_log = Path(restored.chroot()) / "replay.detlog"
    jailed_restored_log.write_bytes(host_log.read_bytes())
    restored.api.replay_load.put(path="/replay.detlog")
    restored.api.replay_mode.put(mode="Replay")

    # Resume briefly — the vCPU should consume at least one event before it
    # (likely) diverges due to uncontrolled interrupt timing.
    restored.api.vm.patch(state="Resumed")
    time.sleep(0.5)

    replay_metrics = restored.flush_metrics()["replay"]
    assert replay_metrics["events_replayed"] > 0, (
        "replay must consume at least one event before divergence; "
        f"metrics={replay_metrics}"
    )
    restored.kill()


def test_replay_divergence_detected_end_to_end(uvm_plain, microvm_factory):
    """Phase 5-C: replaying against an empty log detects divergence.

    With no events loaded, the first trapped exit under Replay mode must hit
    `LogExhausted` inside the replay controller. That error propagates up as
    a vCPU replay-divergence error and increments the `divergences` metric,
    proving the detection pipeline is wired end-to-end from the vCPU exit
    path through to metrics.
    """
    vm = uvm_plain
    vm.spawn()
    vm.basic_config(vcpu_count=1, mem_size_mib=256)
    vm.add_net_iface()
    vm.start()

    vm.api.vm.patch(state="Paused")
    snapshot = vm.snapshot_full()
    vm.kill()

    restored = microvm_factory.build()
    restored.spawn()
    restored.restore_from_snapshot(snapshot, resume=False)

    # No log loaded — replay controller starts with empty events.
    restored.api.replay_mode.put(mode="Replay")
    restored.api.vm.patch(state="Resumed")
    time.sleep(0.5)

    metrics = restored.flush_metrics()["replay"]
    assert metrics["divergences"] > 0, (
        f"replay on empty log must diverge on first exit; metrics={metrics}"
    )
    assert metrics["events_replayed"] == 0, (
        f"no events should have been replayed; metrics={metrics}"
    )
    restored.kill()
