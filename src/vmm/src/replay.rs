// SPDX-License-Identifier: Apache-2.0

//! Deterministic replay primitives.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

/// Execution mode for deterministic replay support.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReplayMode {
    /// Disable deterministic replay logging.
    #[default]
    Off,
    /// Record trapped exits in scalar logical-clock order.
    Record,
    /// Replay trapped exits from a prior log.
    Replay,
}

/// Supported trapped exit kinds for the first replay MVP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetExitKind {
    /// MMIO read exit.
    MmioRead,
    /// MMIO write exit.
    MmioWrite,
    /// PIO input exit.
    IoIn,
    /// PIO output exit.
    IoOut,
}

/// One trapped exit recorded on the scalar logical timeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetExitEvent {
    /// Scalar logical clock for the trapped exit.
    pub seqno: u64,
    /// Exit kind.
    pub kind: DetExitKind,
    /// Bus address.
    pub addr: u64,
    /// Access size in bytes.
    pub size: u32,
    /// Bytes read from or written by the guest.
    pub data: Vec<u8>,
}

/// Shared state for deterministic replay recording.
#[derive(Debug, Default)]
pub struct ReplayController {
    mode: Mutex<ReplayMode>,
    next_seqno: AtomicU64,
    events: Mutex<Vec<DetExitEvent>>,
}

impl ReplayController {
    /// Create a new replay controller in the given mode.
    pub fn new(mode: ReplayMode) -> Self {
        Self {
            mode: Mutex::new(mode),
            next_seqno: AtomicU64::new(0),
            events: Mutex::new(Vec::new()),
        }
    }

    /// Return the current replay mode.
    pub fn mode(&self) -> ReplayMode {
        *self.mode.lock().expect("Replay mode lock poisoned")
    }

    /// Change the replay mode.
    pub fn set_mode(&self, mode: ReplayMode) {
        *self.mode.lock().expect("Replay mode lock poisoned") = mode;
    }

    /// Remove all recorded events and reset the scalar logical clock.
    pub fn reset(&self) {
        self.next_seqno.store(0, Ordering::SeqCst);
        self.events.lock().expect("Replay events lock poisoned").clear();
    }

    /// Record a trapped exit if recording is enabled.
    pub fn record(&self, kind: DetExitKind, addr: u64, data: &[u8]) {
        if self.mode() != ReplayMode::Record {
            return;
        }

        let seqno = self.next_seqno.fetch_add(1, Ordering::SeqCst);
        self.events
            .lock()
            .expect("Replay events lock poisoned")
            .push(DetExitEvent {
                seqno,
                kind,
                addr,
                size: data.len().try_into().unwrap_or(u32::MAX),
                data: data.to_vec(),
            });
    }

    /// Return a snapshot of the recorded event log.
    pub fn snapshot(&self) -> Vec<DetExitEvent> {
        self.events
            .lock()
            .expect("Replay events lock poisoned")
            .clone()
    }

    /// Return the recorded events without cloning.
    pub fn events(&self) -> MutexGuard<'_, Vec<DetExitEvent>> {
        self.events.lock().expect("Replay events lock poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::{DetExitKind, ReplayController, ReplayMode};

    #[test]
    fn test_record_disabled_when_off() {
        let controller = ReplayController::new(ReplayMode::Off);

        controller.record(DetExitKind::MmioRead, 0x10, &[1, 2, 3]);

        assert!(controller.snapshot().is_empty());
    }

    #[test]
    fn test_record_uses_scalar_seqno() {
        let controller = ReplayController::new(ReplayMode::Record);

        controller.record(DetExitKind::MmioRead, 0x10, &[1, 2]);
        controller.record(DetExitKind::IoOut, 0x20, &[3]);

        let events = controller.snapshot();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seqno, 0);
        assert_eq!(events[0].kind, DetExitKind::MmioRead);
        assert_eq!(events[0].addr, 0x10);
        assert_eq!(events[0].size, 2);
        assert_eq!(events[0].data, vec![1, 2]);
        assert_eq!(events[1].seqno, 1);
        assert_eq!(events[1].kind, DetExitKind::IoOut);
        assert_eq!(events[1].addr, 0x20);
        assert_eq!(events[1].size, 1);
        assert_eq!(events[1].data, vec![3]);
    }

    #[test]
    fn test_reset_clears_events_and_seqno() {
        let controller = ReplayController::new(ReplayMode::Record);

        controller.record(DetExitKind::MmioWrite, 0x30, &[7, 8]);
        controller.reset();
        controller.record(DetExitKind::IoIn, 0x40, &[9]);

        let events = controller.snapshot();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seqno, 0);
        assert_eq!(events[0].kind, DetExitKind::IoIn);
    }
}
