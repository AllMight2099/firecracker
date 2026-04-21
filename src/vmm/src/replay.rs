// SPDX-License-Identifier: Apache-2.0

//! Deterministic replay primitives.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

const REPLAY_LOG_MAGIC: [u8; 4] = *b"DET0";
const REPLAY_LOG_VERSION: u16 = 1;

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
}

impl DetExitKind {
    fn as_u8(self) -> u8 {
        match self {
            Self::MmioRead => 0,
            Self::MmioWrite => 1,
            Self::IoIn => 2,
            Self::IoOut => 3,
        }
    }

    fn from_u8(value: u8) -> Result<Self, ReplayLogError> {
        match value {
            0 => Ok(Self::MmioRead),
            1 => Ok(Self::MmioWrite),
            2 => Ok(Self::IoIn),
            3 => Ok(Self::IoOut),
            _ => Err(ReplayLogError::InvalidExitKind(value)),
        }
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
         got {actual_kind:?} @ {actual_addr:#x}"
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
        let seqno = self.replay_cursor.fetch_add(1, Ordering::SeqCst);
        let events = self.events.lock().expect("Replay events lock poisoned");
        let expected = events
            .get(seqno as usize)
            .ok_or(ReplayDivergence::LogExhausted { seqno })?;

        if expected.kind != kind || expected.addr != addr {
            return Err(ReplayDivergence::KindOrAddrMismatch {
                seqno,
                expected_kind: expected.kind,
                expected_addr: expected.addr,
                actual_kind: kind,
                actual_addr: addr,
            });
        }

        let actual_size = data.len() as u32;
        if expected.size != actual_size {
            return Err(ReplayDivergence::SizeMismatch {
                seqno,
                expected_size: expected.size,
                actual_size,
            });
        }

        data.copy_from_slice(&expected.data);
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
        let seqno = self.replay_cursor.fetch_add(1, Ordering::SeqCst);
        let events = self.events.lock().expect("Replay events lock poisoned");
        let expected = events
            .get(seqno as usize)
            .ok_or(ReplayDivergence::LogExhausted { seqno })?;

        if expected.kind != kind || expected.addr != addr {
            return Err(ReplayDivergence::KindOrAddrMismatch {
                seqno,
                expected_kind: expected.kind,
                expected_addr: expected.addr,
                actual_kind: kind,
                actual_addr: addr,
            });
        }

        if expected.data.as_slice() != data {
            return Err(ReplayDivergence::WriteDataMismatch { seqno, addr });
        }

        Ok(())
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
        controller.record(DetExitKind::IoOut, 0x20, &[3, 4, 5]);
        controller.save_to_writer(&mut buf).unwrap();

        let loaded = ReplayController::new(ReplayMode::Off);
        loaded.load_from_reader(&mut Cursor::new(buf)).unwrap();

        assert_eq!(loaded.snapshot(), controller.snapshot());
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
