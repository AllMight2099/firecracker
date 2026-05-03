// SPDX-License-Identifier: Apache-2.0

//! Deterministic replay primitives.

use std::io::{self, Read, Write};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

use crate::logger::{IncMetric, METRICS};

const REPLAY_LOG_MAGIC: [u8; 4] = *b"DET0";
const REPLAY_LOG_VERSION: u16 = 1;

/// Source tag for an `IrqInjection` event: legacy / ACPI device path
/// (`EventFdTrigger::trigger`).
pub const IRQ_SOURCE_LEGACY: u8 = 0;
/// Source tag for an `IrqInjection` event: virtio config-change interrupt.
pub const IRQ_SOURCE_VIRTIO_CONFIG: u8 = 1;
/// Source tag for an `IrqInjection` event: virtio used-ring interrupt.
pub const IRQ_SOURCE_VIRTIO_VRING: u8 = 2;

/// Process-wide registry holding a `Weak<ReplayController>` so device-level
/// IRQ trigger paths can record observability events without each device
/// owning a controller reference.
///
/// This is a prototype simplification: the proper fix is to thread the
/// controller through every device's construction. We accept the global for
/// now because (a) IRQ observability is record-only diagnostic data, (b)
/// replay-aware Firecracker is single-vmm-per-process, and (c) the registry
/// holds a `Weak` so it cannot extend the controller's lifetime.
static GLOBAL_REPLAY_CONTROLLER: OnceLock<Mutex<Option<Weak<ReplayController>>>> =
    OnceLock::new();

fn global_slot() -> &'static Mutex<Option<Weak<ReplayController>>> {
    GLOBAL_REPLAY_CONTROLLER.get_or_init(|| Mutex::new(None))
}

/// Register `controller` as the process-wide replay controller for IRQ
/// observability. Subsequent `record_irq_via_global` calls will record
/// against it for as long as it remains live.
///
/// Setting twice (e.g. across snapshot restores) is allowed; the last
/// registration wins. Pass `None` to clear.
pub fn register_global_replay_controller(controller: Option<&Arc<ReplayController>>) {
    let mut slot = global_slot().lock().expect("global replay controller lock poisoned");
    *slot = controller.map(Arc::downgrade);
}

/// Record an IRQ injection through the global replay controller, if one is
/// registered and still live. No-op otherwise. Intended to be called from
/// device IRQ trigger paths.
pub fn record_irq_via_global(source_tag: u8, payload: u32) {
    let weak = {
        let slot = global_slot().lock().expect("global replay controller lock poisoned");
        slot.clone()
    };
    if let Some(rc) = weak.and_then(|w| w.upgrade()) {
        rc.record_irq(source_tag, payload);
    }
}

/// Execution mode for deterministic replay support.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
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
    /// Guest-visible VMClock state update.
    VmClockState,
    /// Guest TSC read event (rdmsr MSR_IA32_TSC).
    Rdtsc,
    /// Guest MSR read trapped to userspace via KVM_CAP_X86_USER_SPACE_MSR.
    MsrRead,
    /// Guest MSR write trapped to userspace via KVM_CAP_X86_USER_SPACE_MSR.
    MsrWrite,
    /// IRQ injection observed at a userspace device → guest delivery point.
    /// Recorded for divergence diagnostics; not yet replayed (replay would
    /// require instruction-position pinning via PMU).
    IrqInjection,
}

impl DetExitKind {
    fn as_u8(self) -> u8 {
        match self {
            Self::MmioRead => 0,
            Self::MmioWrite => 1,
            Self::IoIn => 2,
            Self::IoOut => 3,
            Self::VmClockState => 4,
            Self::Rdtsc => 5,
            Self::MsrRead => 6,
            Self::MsrWrite => 7,
            Self::IrqInjection => 8,
        }
    }

    fn from_u8(value: u8) -> Result<Self, ReplayLogError> {
        match value {
            0 => Ok(Self::MmioRead),
            1 => Ok(Self::MmioWrite),
            2 => Ok(Self::IoIn),
            3 => Ok(Self::IoOut),
            4 => Ok(Self::VmClockState),
            5 => Ok(Self::Rdtsc),
            6 => Ok(Self::MsrRead),
            7 => Ok(Self::MsrWrite),
            8 => Ok(Self::IrqInjection),
            _ => Err(ReplayLogError::InvalidExitKind(value)),
        }
    }

    fn is_diagnostic_only(self) -> bool {
        matches!(self, Self::IrqInjection)
    }
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

const IRQ_DIAGNOSTIC_HISTORY_LEN: usize = 4;

/// Diagnostic context carried on replay divergence: the most recent IRQ
/// observability events seen in the recorded stream before the mismatch point.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecentIrqEvents(pub Vec<DetExitEvent>);

impl fmt::Display for RecentIrqEvents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return write!(f, "no recent IRQs");
        }

        write!(f, "recent IRQs [")?;
        for (idx, event) in self.0.iter().enumerate() {
            if idx != 0 {
                write!(f, ", ")?;
            }

            let payload = match event.data.as_slice() {
                [a, b, c, d] => u32::from_le_bytes([*a, *b, *c, *d]),
                _ => 0,
            };
            write!(
                f,
                "#{} src={} payload=0x{:08x}",
                event.seqno, event.addr, payload
            )?;
        }
        write!(f, "]")
    }
}

/// Error returned when a replayed event does not match the expected stream.
#[derive(Debug, thiserror::Error)]
pub enum ReplayDivergence {
    /// Replay log exhausted: expected event at seqno {seqno}
    #[error("Replay log exhausted: expected event at seqno {seqno}")]
    LogExhausted {
        /// Sequence number at which the log ran out.
        seqno: u64,
    },
    /// Replay divergence at seqno {seqno}: expected {expected_kind:?} @ {expected_addr:#x}, got {actual_kind:?} @ {actual_addr:#x}
    #[error(
        "Replay divergence at seqno {seqno}: expected {expected_kind:?} @ {expected_addr:#x}, \
         got {actual_kind:?} @ {actual_addr:#x}; {recent_irqs}"
    )]
    KindOrAddrMismatch {
        /// Sequence number of the diverging event.
        seqno: u64,
        /// Exit kind recorded in the log.
        expected_kind: DetExitKind,
        /// Bus address recorded in the log.
        expected_addr: u64,
        /// Exit kind observed during replay.
        actual_kind: DetExitKind,
        /// Bus address observed during replay.
        actual_addr: u64,
        /// Recent IRQ observability events preceding the mismatch.
        recent_irqs: RecentIrqEvents,
    },
    /// Replay divergence at seqno {seqno}: size mismatch, expected {expected_size}, got {actual_size}
    #[error(
        "Replay divergence at seqno {seqno}: size mismatch, expected {expected_size}, got \
         {actual_size}"
    )]
    SizeMismatch {
        /// Sequence number of the diverging event.
        seqno: u64,
        /// Access size recorded in the log.
        expected_size: u32,
        /// Access size observed during replay.
        actual_size: u32,
    },
    /// Replay divergence at seqno {seqno}: write data mismatch @ {addr:#x}
    #[error("Replay divergence at seqno {seqno}: write data mismatch @ {addr:#x}")]
    WriteDataMismatch {
        /// Sequence number of the diverging event.
        seqno: u64,
        /// Bus address of the diverging write.
        addr: u64,
    },
}

/// Shared state for deterministic replay recording.
#[derive(Debug, Default)]
pub struct ReplayController {
    mode: Mutex<ReplayMode>,
    next_seqno: AtomicU64,
    /// Cursor into the event list advanced during replay.
    replay_cursor: AtomicU64,
    events: Mutex<Vec<DetExitEvent>>,
}

/// Errors returned by replay log serialization and deserialization.
#[derive(Debug, thiserror::Error)]
pub enum ReplayLogError {
    /// I/O failure while reading or writing the replay log.
    #[error("{0}")]
    Io(#[from] io::Error),
    /// Replay log magic does not match the expected file format.
    #[error("Invalid replay log magic")]
    InvalidMagic,
    /// Replay log version is not supported.
    #[error("Unsupported replay log version {0}")]
    UnsupportedVersion(u16),
    /// Replay log contains an unknown exit kind.
    #[error("Invalid replay exit kind {0}")]
    InvalidExitKind(u8),
}

impl ReplayController {
    /// Create a new replay controller in the given mode.
    pub fn new(mode: ReplayMode) -> Self {
        Self {
            mode: Mutex::new(mode),
            next_seqno: AtomicU64::new(0),
            replay_cursor: AtomicU64::new(0),
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

    /// Remove all recorded events and reset the scalar logical clock and replay cursor.
    pub fn reset(&self) {
        self.next_seqno.store(0, Ordering::SeqCst);
        self.replay_cursor.store(0, Ordering::SeqCst);
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
        METRICS.replay.events_recorded.inc();
    }

    /// Record a guest TSC read value if recording is enabled.
    pub fn record_rdtsc(&self, value: u64) {
        self.record(DetExitKind::Rdtsc, 0, &value.to_le_bytes());
    }

    /// Record a guest MSR read trapped to userspace.
    ///
    /// `msr_index` is stored as the event address; `value` is what was returned to the guest.
    pub fn record_msr_read(&self, msr_index: u32, value: u64) {
        self.record(DetExitKind::MsrRead, u64::from(msr_index), &value.to_le_bytes());
    }

    /// Record a guest MSR write trapped to userspace.
    pub fn record_msr_write(&self, msr_index: u32, value: u64) {
        self.record(DetExitKind::MsrWrite, u64::from(msr_index), &value.to_le_bytes());
    }

    /// Record an IRQ injection observed at a userspace device → guest delivery point.
    ///
    /// `source_tag` distinguishes the injection path (`IRQ_SOURCE_LEGACY`,
    /// `IRQ_SOURCE_VIRTIO_VRING`, etc.). `payload` is a 32-bit per-source summary used
    /// only for diagnostics; for legacy devices it is typically zero, for virtio it
    /// carries the `irq_status` bits.
    ///
    /// IRQ events are recorded for divergence diagnostics; replay does not yet
    /// re-inject interrupts at instruction-precise positions.
    pub fn record_irq(&self, source_tag: u8, payload: u32) {
        self.record(
            DetExitKind::IrqInjection,
            u64::from(source_tag),
            &payload.to_le_bytes(),
        );
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

    /// Serialize the recorded event stream to a writer.
    pub fn save_to_writer<W: Write>(&self, writer: &mut W) -> Result<(), ReplayLogError> {
        writer.write_all(&REPLAY_LOG_MAGIC)?;
        writer.write_all(&REPLAY_LOG_VERSION.to_le_bytes())?;

        for event in self.snapshot() {
            writer.write_all(&event.seqno.to_le_bytes())?;
            writer.write_all(&[event.kind.as_u8()])?;
            writer.write_all(&[0_u8])?;
            writer.write_all(&0_u16.to_le_bytes())?;
            writer.write_all(&event.addr.to_le_bytes())?;
            writer.write_all(&event.size.to_le_bytes())?;
            writer.write_all(&event.data)?;
        }

        Ok(())
    }

    /// Replace the current event stream with events deserialized from a reader.
    pub fn load_from_reader<R: Read>(&self, reader: &mut R) -> Result<(), ReplayLogError> {
        let mut magic = [0_u8; 4];
        reader.read_exact(&mut magic)?;
        if magic != REPLAY_LOG_MAGIC {
            return Err(ReplayLogError::InvalidMagic);
        }

        let mut version = [0_u8; 2];
        reader.read_exact(&mut version)?;
        let version = u16::from_le_bytes(version);
        if version != REPLAY_LOG_VERSION {
            return Err(ReplayLogError::UnsupportedVersion(version));
        }

        let mut events = Vec::new();
        loop {
            let mut seqno = [0_u8; 8];
            match reader.read_exact(&mut seqno) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(err) => return Err(ReplayLogError::Io(err)),
            }

            let mut kind = [0_u8; 1];
            reader.read_exact(&mut kind)?;
            let kind = DetExitKind::from_u8(kind[0])?;

            let mut reserved = [0_u8; 3];
            reader.read_exact(&mut reserved)?;

            let mut addr = [0_u8; 8];
            reader.read_exact(&mut addr)?;

            let mut size = [0_u8; 4];
            reader.read_exact(&mut size)?;
            let size = u32::from_le_bytes(size);

            let mut data = vec![0_u8; size as usize];
            reader.read_exact(&mut data)?;

            events.push(DetExitEvent {
                seqno: u64::from_le_bytes(seqno),
                kind,
                addr: u64::from_le_bytes(addr),
                size,
                data,
            });
        }

        self.next_seqno.store(events.len() as u64, Ordering::SeqCst);
        self.replay_cursor.store(0, Ordering::SeqCst);
        *self.events.lock().expect("Replay events lock poisoned") = events;
        Ok(())
    }

    fn recent_irq_events(events: &[DetExitEvent], upto_seqno: u64) -> RecentIrqEvents {
        let mut recent = events
            .iter()
            .take(upto_seqno as usize)
            .rev()
            .filter(|event| event.kind == DetExitKind::IrqInjection)
            .take(IRQ_DIAGNOSTIC_HISTORY_LEN)
            .cloned()
            .collect::<Vec<_>>();
        recent.reverse();
        RecentIrqEvents(recent)
    }

    fn next_replay_event(&self) -> Result<(u64, DetExitEvent, RecentIrqEvents), ReplayDivergence> {
        let events = self.events.lock().expect("Replay events lock poisoned");
        let mut seqno = self.replay_cursor.load(Ordering::SeqCst);

        while let Some(event) = events.get(seqno as usize) {
            if !event.kind.is_diagnostic_only() {
                let recent_irqs = Self::recent_irq_events(&events, seqno);
                self.replay_cursor.store(seqno + 1, Ordering::SeqCst);
                return Ok((seqno, event.clone(), recent_irqs));
            }
            seqno += 1;
        }

        self.replay_cursor.store(seqno, Ordering::SeqCst);
        Err(ReplayDivergence::LogExhausted { seqno })
    }

    /// Consume the next event during replay and fill `data` with the logged bytes.
    ///
    /// Validates that the expected kind and address match, then copies the logged data into the
    /// caller-supplied buffer. Advances the replay cursor on success.
    pub fn consume_read(
        &self,
        kind: DetExitKind,
        addr: u64,
        data: &mut [u8],
    ) -> Result<(), ReplayDivergence> {
        let (seqno, expected, recent_irqs) = self.next_replay_event().inspect_err(|_| {
            METRICS.replay.divergences.inc();
        })?;

        if expected.kind != kind || expected.addr != addr {
            METRICS.replay.divergences.inc();
            return Err(ReplayDivergence::KindOrAddrMismatch {
                seqno,
                expected_kind: expected.kind,
                expected_addr: expected.addr,
                actual_kind: kind,
                actual_addr: addr,
                recent_irqs,
            });
        }

        let actual_size = data.len() as u32;
        if expected.size != actual_size {
            METRICS.replay.divergences.inc();
            return Err(ReplayDivergence::SizeMismatch {
                seqno,
                expected_size: expected.size,
                actual_size,
            });
        }

        data.copy_from_slice(&expected.data);
        METRICS.replay.events_replayed.inc();
        Ok(())
    }

    /// Validate a write exit during replay.
    ///
    /// Checks that the expected kind, address, and data match the log entry. The caller is still
    /// responsible for executing the write on the device bus. Advances the replay cursor on success.
    pub fn validate_write(
        &self,
        kind: DetExitKind,
        addr: u64,
        data: &[u8],
    ) -> Result<(), ReplayDivergence> {
        let (seqno, expected, recent_irqs) = self.next_replay_event().inspect_err(|_| {
            METRICS.replay.divergences.inc();
        })?;

        if expected.kind != kind || expected.addr != addr {
            METRICS.replay.divergences.inc();
            return Err(ReplayDivergence::KindOrAddrMismatch {
                seqno,
                expected_kind: expected.kind,
                expected_addr: expected.addr,
                actual_kind: kind,
                actual_addr: addr,
                recent_irqs,
            });
        }

        if expected.data.as_slice() != data {
            METRICS.replay.divergences.inc();
            return Err(ReplayDivergence::WriteDataMismatch { seqno, addr });
        }

        METRICS.replay.events_replayed.inc();
        Ok(())
    }

    /// Consume the next replay event as a guest TSC read value.
    pub fn consume_rdtsc(&self) -> Result<u64, ReplayDivergence> {
        let mut buf = [0_u8; 8];
        self.consume_read(DetExitKind::Rdtsc, 0, &mut buf)?;
        Ok(u64::from_le_bytes(buf))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

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

    #[test]
    fn test_consume_read_returns_logged_bytes() {
        let controller = ReplayController::new(ReplayMode::Record);
        controller.record(DetExitKind::MmioRead, 0x1000, &[0xAA, 0xBB]);
        controller.set_mode(ReplayMode::Replay);

        let mut buf = [0u8; 2];
        controller
            .consume_read(DetExitKind::MmioRead, 0x1000, &mut buf)
            .unwrap();
        assert_eq!(buf, [0xAA, 0xBB]);
    }

    #[test]
    fn test_consume_read_diverges_on_kind_mismatch() {
        let controller = ReplayController::new(ReplayMode::Record);
        controller.record(DetExitKind::MmioRead, 0x1000, &[1]);
        controller.set_mode(ReplayMode::Replay);

        let mut buf = [0u8; 1];
        let err = controller
            .consume_read(DetExitKind::IoIn, 0x1000, &mut buf)
            .unwrap_err();
        assert!(matches!(
            err,
            super::ReplayDivergence::KindOrAddrMismatch { .. }
        ));
    }

    #[test]
    fn test_consume_read_diverges_on_log_exhausted() {
        let controller = ReplayController::new(ReplayMode::Replay);
        let mut buf = [0u8; 1];
        let err = controller
            .consume_read(DetExitKind::MmioRead, 0x10, &mut buf)
            .unwrap_err();
        assert!(matches!(
            err,
            super::ReplayDivergence::LogExhausted { seqno: 0 }
        ));
    }

    #[test]
    fn test_validate_write_succeeds_on_match() {
        let controller = ReplayController::new(ReplayMode::Record);
        controller.record(DetExitKind::MmioWrite, 0x2000, &[0x01, 0x02]);
        controller.set_mode(ReplayMode::Replay);

        controller
            .validate_write(DetExitKind::MmioWrite, 0x2000, &[0x01, 0x02])
            .unwrap();
    }

    #[test]
    fn test_validate_write_diverges_on_data_mismatch() {
        let controller = ReplayController::new(ReplayMode::Record);
        controller.record(DetExitKind::MmioWrite, 0x2000, &[0x01, 0x02]);
        controller.set_mode(ReplayMode::Replay);

        let err = controller
            .validate_write(DetExitKind::MmioWrite, 0x2000, &[0xFF, 0xFF])
            .unwrap_err();
        assert!(matches!(
            err,
            super::ReplayDivergence::WriteDataMismatch { .. }
        ));
    }

    #[test]
    fn test_replay_log_round_trip() {
        let controller = ReplayController::new(ReplayMode::Record);
        let mut buf = Vec::new();

        controller.record(DetExitKind::MmioRead, 0x10, &[1, 2]);
        controller.record_rdtsc(0x1122_3344_5566_7788);
        controller.record(DetExitKind::IoOut, 0x20, &[3, 4, 5]);
        controller.record_msr_read(0x10, 0xAABB_CCDD_EEFF_0011);
        controller.record_msr_write(0x4b56_4d01, 0xDEAD_BEEF_CAFE_F00D);
        controller.save_to_writer(&mut buf).unwrap();

        let loaded = ReplayController::new(ReplayMode::Off);
        loaded.load_from_reader(&mut Cursor::new(buf)).unwrap();

        assert_eq!(loaded.snapshot(), controller.snapshot());
    }

    #[test]
    fn test_consume_msr_read_returns_logged_value() {
        const MSR_IA32_TSC: u32 = 0x10;
        let controller = ReplayController::new(ReplayMode::Record);
        controller.record_msr_read(MSR_IA32_TSC, 0xAABB_CCDD_EEFF_0011);
        controller.set_mode(ReplayMode::Replay);

        let mut buf = [0_u8; 8];
        controller
            .consume_read(DetExitKind::MsrRead, u64::from(MSR_IA32_TSC), &mut buf)
            .unwrap();
        assert_eq!(u64::from_le_bytes(buf), 0xAABB_CCDD_EEFF_0011);
    }

    #[test]
    fn test_consume_msr_read_diverges_on_index_mismatch() {
        const MSR_IA32_TSC: u32 = 0x10;
        const MSR_KVM_SYSTEM_TIME_NEW: u32 = 0x4b56_4d01;
        let controller = ReplayController::new(ReplayMode::Record);
        controller.record_msr_read(MSR_IA32_TSC, 0x1234);
        controller.set_mode(ReplayMode::Replay);

        let mut buf = [0_u8; 8];
        let err = controller
            .consume_read(
                DetExitKind::MsrRead,
                u64::from(MSR_KVM_SYSTEM_TIME_NEW),
                &mut buf,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            super::ReplayDivergence::KindOrAddrMismatch { .. }
        ));
    }

    #[test]
    fn test_validate_msr_write_diverges_on_data_mismatch() {
        const MSR_KVM_SYSTEM_TIME_NEW: u32 = 0x4b56_4d01;
        let controller = ReplayController::new(ReplayMode::Record);
        controller.record_msr_write(MSR_KVM_SYSTEM_TIME_NEW, 0xDEAD_BEEF);
        controller.set_mode(ReplayMode::Replay);

        let err = controller
            .validate_write(
                DetExitKind::MsrWrite,
                u64::from(MSR_KVM_SYSTEM_TIME_NEW),
                &0xCAFE_BABE_u64.to_le_bytes(),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            super::ReplayDivergence::WriteDataMismatch { .. }
        ));
    }

    #[test]
    fn test_record_irq_appends_event_in_record_mode() {
        let controller = ReplayController::new(ReplayMode::Record);
        controller.record_irq(super::IRQ_SOURCE_LEGACY, 0);
        controller.record_irq(super::IRQ_SOURCE_VIRTIO_VRING, 0x42);

        let events = controller.snapshot();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, DetExitKind::IrqInjection);
        assert_eq!(events[0].addr, u64::from(super::IRQ_SOURCE_LEGACY));
        assert_eq!(events[1].kind, DetExitKind::IrqInjection);
        assert_eq!(events[1].addr, u64::from(super::IRQ_SOURCE_VIRTIO_VRING));
    }

    #[test]
    fn test_record_irq_is_noop_off_or_replay() {
        let controller = ReplayController::new(ReplayMode::Off);
        controller.record_irq(super::IRQ_SOURCE_LEGACY, 0);
        assert!(controller.snapshot().is_empty());

        controller.set_mode(ReplayMode::Replay);
        controller.record_irq(super::IRQ_SOURCE_LEGACY, 0);
        assert!(controller.snapshot().is_empty());
    }

    #[test]
    fn test_global_replay_controller_records_via_helper() {
        // Use a unique-enough state by clearing and reinstalling a controller.
        super::register_global_replay_controller(None);

        let controller = std::sync::Arc::new(ReplayController::new(ReplayMode::Record));
        super::register_global_replay_controller(Some(&controller));

        super::record_irq_via_global(super::IRQ_SOURCE_LEGACY, 0);
        super::record_irq_via_global(super::IRQ_SOURCE_VIRTIO_VRING, 0x01);

        let events = controller.snapshot();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].addr, u64::from(super::IRQ_SOURCE_LEGACY));
        assert_eq!(events[1].addr, u64::from(super::IRQ_SOURCE_VIRTIO_VRING));

        super::register_global_replay_controller(None);
    }

    #[test]
    fn test_global_replay_controller_is_noop_when_unregistered() {
        super::register_global_replay_controller(None);
        // Just shouldn't panic or write anywhere observable.
        super::record_irq_via_global(super::IRQ_SOURCE_LEGACY, 0);
    }

    #[test]
    fn test_consume_rdtsc_returns_logged_value() {
        let controller = ReplayController::new(ReplayMode::Record);
        controller.record_rdtsc(0xAABB_CCDD_EEFF_0011);
        controller.set_mode(ReplayMode::Replay);

        let value = controller.consume_rdtsc().unwrap();
        assert_eq!(value, 0xAABB_CCDD_EEFF_0011);
    }

    #[test]
    fn test_consume_rdtsc_diverges_on_wrong_event_kind() {
        let controller = ReplayController::new(ReplayMode::Record);
        controller.record(DetExitKind::MmioRead, 0x1000, &[1; 8]);
        controller.set_mode(ReplayMode::Replay);

        let err = controller.consume_rdtsc().unwrap_err();
        assert!(matches!(
            err,
            super::ReplayDivergence::KindOrAddrMismatch { .. }
        ));
    }

    #[test]
    fn test_record_save_load_replay_end_to_end() {
        use std::fs::File;

        use vmm_sys_util::tempfile::TempFile;

        use super::ReplayDivergence;

        let log_file = TempFile::new().unwrap();
        let log_path = log_file.as_path().to_path_buf();

        let recorder = ReplayController::new(ReplayMode::Record);
        recorder.record(DetExitKind::MmioRead, 0x1000, &[0xDE, 0xAD]);
        recorder.record(DetExitKind::MmioWrite, 0x1004, &[0xBE, 0xEF]);
        recorder.record(DetExitKind::IoIn, 0x3F8, &[0x42]);
        recorder.record(DetExitKind::IoOut, 0x3F8, &[0x5A, 0x6B]);

        {
            let mut writer = File::create(&log_path).unwrap();
            recorder.save_to_writer(&mut writer).unwrap();
        }

        let replayer = ReplayController::new(ReplayMode::Off);
        {
            let mut reader = File::open(&log_path).unwrap();
            replayer.load_from_reader(&mut reader).unwrap();
        }
        replayer.set_mode(ReplayMode::Replay);

        let mut mmio_read_buf = [0u8; 2];
        replayer
            .consume_read(DetExitKind::MmioRead, 0x1000, &mut mmio_read_buf)
            .unwrap();
        assert_eq!(mmio_read_buf, [0xDE, 0xAD]);

        replayer
            .validate_write(DetExitKind::MmioWrite, 0x1004, &[0xBE, 0xEF])
            .unwrap();

        let mut pio_in_buf = [0u8; 1];
        replayer
            .consume_read(DetExitKind::IoIn, 0x3F8, &mut pio_in_buf)
            .unwrap();
        assert_eq!(pio_in_buf, [0x42]);

        replayer
            .validate_write(DetExitKind::IoOut, 0x3F8, &[0x5A, 0x6B])
            .unwrap();

        let mut overflow_buf = [0u8; 1];
        let err = replayer
            .consume_read(DetExitKind::MmioRead, 0x0, &mut overflow_buf)
            .unwrap_err();
        assert!(matches!(err, ReplayDivergence::LogExhausted { seqno: 4 }));
    }

    #[test]
    fn test_divergence_on_modified_workload() {
        use std::fs::File;

        use vmm_sys_util::tempfile::TempFile;

        use super::ReplayDivergence;

        let log_file = TempFile::new().unwrap();
        let log_path = log_file.as_path().to_path_buf();

        let recorder = ReplayController::new(ReplayMode::Record);
        recorder.record(DetExitKind::MmioRead, 0x1000, &[0x11, 0x22]);
        recorder.record(DetExitKind::MmioWrite, 0x1004, &[0x33, 0x44]);
        {
            let mut writer = File::create(&log_path).unwrap();
            recorder.save_to_writer(&mut writer).unwrap();
        }

        let replayer = ReplayController::new(ReplayMode::Off);
        {
            let mut reader = File::open(&log_path).unwrap();
            replayer.load_from_reader(&mut reader).unwrap();
        }
        replayer.set_mode(ReplayMode::Replay);

        let mut buf = [0u8; 2];
        let err = replayer
            .consume_read(DetExitKind::IoIn, 0x1000, &mut buf)
            .unwrap_err();
        assert!(matches!(
            err,
            ReplayDivergence::KindOrAddrMismatch {
                seqno: 0,
                expected_kind: DetExitKind::MmioRead,
                ..
            }
        ));
    }

    #[test]
    fn test_kind_mismatch_carries_recent_irq_history() {
        use super::ReplayDivergence;

        let controller = ReplayController::new(ReplayMode::Record);
        controller.record_irq(super::IRQ_SOURCE_LEGACY, 0);
        controller.record_irq(super::IRQ_SOURCE_VIRTIO_VRING, 0x42);
        controller.record(DetExitKind::MmioRead, 0x1000, &[0x11, 0x22]);
        controller.set_mode(ReplayMode::Replay);

        let mut buf = [0u8; 2];
        let err = controller
            .consume_read(DetExitKind::IoIn, 0x1000, &mut buf)
            .unwrap_err();

        match err {
            ReplayDivergence::KindOrAddrMismatch {
                seqno,
                expected_kind,
                recent_irqs,
                ..
            } => {
                assert_eq!(seqno, 2);
                assert_eq!(expected_kind, DetExitKind::MmioRead);
                assert_eq!(recent_irqs.0.len(), 2);
                assert_eq!(recent_irqs.0[0].addr, u64::from(super::IRQ_SOURCE_LEGACY));
                assert_eq!(
                    recent_irqs.0[1].addr,
                    u64::from(super::IRQ_SOURCE_VIRTIO_VRING)
                );
            }
            other => panic!("expected KindOrAddrMismatch, got {other:?}"),
        }
    }

    #[test]
    fn test_consume_read_skips_diagnostic_irq_events() {
        let controller = ReplayController::new(ReplayMode::Record);
        controller.record_irq(super::IRQ_SOURCE_LEGACY, 0);
        controller.record(DetExitKind::MmioRead, 0x1000, &[0xAA, 0xBB]);
        controller.record_irq(super::IRQ_SOURCE_VIRTIO_VRING, 0x42);
        controller.set_mode(ReplayMode::Replay);

        let mut buf = [0u8; 2];
        controller
            .consume_read(DetExitKind::MmioRead, 0x1000, &mut buf)
            .unwrap();
        assert_eq!(buf, [0xAA, 0xBB]);

        let mut trailing = [0u8; 1];
        let err = controller
            .consume_read(DetExitKind::IoIn, 0x3f8, &mut trailing)
            .unwrap_err();
        assert!(matches!(err, super::ReplayDivergence::LogExhausted { seqno: 3 }));
    }

    #[test]
    fn test_load_rejects_truncated_log() {
        use super::ReplayLogError;

        let recorder = ReplayController::new(ReplayMode::Record);
        recorder.record(DetExitKind::MmioRead, 0x1000, &[0xAA, 0xBB, 0xCC, 0xDD]);

        let mut buf = Vec::new();
        recorder.save_to_writer(&mut buf).unwrap();
        buf.truncate(buf.len() - 3);

        let replayer = ReplayController::new(ReplayMode::Off);
        let err = replayer
            .load_from_reader(&mut Cursor::new(buf))
            .unwrap_err();
        assert!(matches!(err, ReplayLogError::Io(_)));
    }

    #[test]
    fn test_load_rejects_bad_magic() {
        use super::ReplayLogError;

        let replayer = ReplayController::new(ReplayMode::Off);
        let garbage = b"XXXXfoo";
        let err = replayer
            .load_from_reader(&mut Cursor::new(garbage.to_vec()))
            .unwrap_err();
        assert!(matches!(err, ReplayLogError::InvalidMagic));
    }
}
