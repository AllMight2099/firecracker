// SPDX-License-Identifier: Apache-2.0

use std::fmt::Write as _;
use std::io::Cursor;

use vmm::replay::{
    DetExitKind, IRQ_SOURCE_LEGACY, IRQ_SOURCE_VIRTIO_VRING, ReplayController, ReplayMode,
};

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::new();
    for (idx, byte) in bytes.iter().enumerate() {
        if idx != 0 {
            out.push(' ');
        }
        write!(&mut out, "{byte:02x}").unwrap();
    }
    out
}

fn fmt_preview(bytes: &[u8], limit: usize) -> String {
    let shown = bytes.len().min(limit);
    let mut out = hex_bytes(&bytes[..shown]);
    if bytes.len() > shown {
        out.push_str(" ...");
    }
    out
}

fn kind_name(kind: DetExitKind) -> &'static str {
    match kind {
        DetExitKind::MmioRead => "MMIO_READ",
        DetExitKind::MmioWrite => "MMIO_WRITE",
        DetExitKind::IoIn => "PIO_IN",
        DetExitKind::IoOut => "PIO_OUT",
        DetExitKind::VmClockState => "VMCLOCK",
        DetExitKind::Rdtsc => "RDTSC",
        DetExitKind::MsrRead => "MSR_READ",
        DetExitKind::MsrWrite => "MSR_WRITE",
        DetExitKind::IrqInjection => "IRQ",
    }
}

fn irq_source_name(addr: u64) -> &'static str {
    match addr as u8 {
        IRQ_SOURCE_LEGACY => "legacy",
        IRQ_SOURCE_VIRTIO_VRING => "virtio_vring",
        _ => "other",
    }
}

fn sample_vmclock_bytes() -> [u8; 112] {
    let mut bytes = [0u8; 112];
    bytes[..8].copy_from_slice(&[0x56, 0x43, 0x4c, 0x4b, 0x00, 0x10, 0x00, 0x00]);
    bytes[8] = 0x01; // version low byte
    bytes[10] = 0x01; // counter id
    bytes[12..16].copy_from_slice(&0x0000_0002_u32.to_le_bytes()); // seq_count
    bytes[16..24].copy_from_slice(&0x0000_0000_0000_0001_u64.to_le_bytes()); // disruption marker
    bytes[72..80].copy_from_slice(&0x0000_0000_1234_5678_u64.to_le_bytes()); // time_sec
    bytes[104..112].copy_from_slice(&0x0000_0000_0000_0003_u64.to_le_bytes()); // vm_generation_counter
    bytes
}

fn main() {
    println!("=== Replay Walkthrough Demo ===");
    println!("This demo drives ReplayController directly to show what gets");
    println!("stored for MMIO/PIO, VMClock, and IRQ observability events.");
    println!();

    let vmclock = sample_vmclock_bytes();
    let recorder = ReplayController::new(ReplayMode::Record);

    // Put IRQs around the replay-driving events to show that they are logged
    // but skipped by replay matching.
    recorder.record_irq(IRQ_SOURCE_LEGACY, 0);
    recorder.record(DetExitKind::VmClockState, 0xde000, &vmclock);
    recorder.record(DetExitKind::MmioRead, 0x1000, &[0xde, 0xad]);
    recorder.record(DetExitKind::MmioWrite, 0x1004, &[0xbe, 0xef]);
    recorder.record(DetExitKind::IoIn, 0x3f8, &[0x42]);
    recorder.record(DetExitKind::IoOut, 0x3f8, &[0x5a, 0x6b]);
    recorder.record_irq(IRQ_SOURCE_VIRTIO_VRING, 0x01);

    println!("Recorded event stream:");
    for event in recorder.snapshot() {
        match event.kind {
            DetExitKind::IrqInjection => {
                let payload = u32::from_le_bytes(event.data.as_slice().try_into().unwrap());
                println!(
                    "  seq={}  {:<10} source={} payload=0x{payload:08x}",
                    event.seqno,
                    kind_name(event.kind),
                    irq_source_name(event.addr)
                );
            }
            DetExitKind::VmClockState => {
                println!(
                    "  seq={}  {:<10} addr=0x{:x} size={} data=[{}]",
                    event.seqno,
                    kind_name(event.kind),
                    event.addr,
                    event.size,
                    fmt_preview(&event.data, 16)
                );
            }
            _ => {
                println!(
                    "  seq={}  {:<10} addr=0x{:x} size={} data=[{}]",
                    event.seqno,
                    kind_name(event.kind),
                    event.addr,
                    event.size,
                    fmt_preview(&event.data, 16)
                );
            }
        }
    }
    println!();

    let mut encoded = Vec::new();
    recorder.save_to_writer(&mut encoded).unwrap();
    println!("Binary sidecar layout:");
    println!("  header magic = {:?}", String::from_utf8_lossy(&encoded[..4]));
    println!(
        "  version bytes = [{}] (little-endian u16)",
        hex_bytes(&encoded[4..6])
    );
    println!("  total bytes = {}", encoded.len());
    println!("  first 48 bytes = [{}]", fmt_preview(&encoded, 48));
    println!();

    let replayer = ReplayController::new(ReplayMode::Off);
    replayer.load_from_reader(&mut Cursor::new(&encoded)).unwrap();
    replayer.set_mode(ReplayMode::Replay);

    println!("Replay walk-through:");

    let mut vmclock_buf = [0u8; 112];
    replayer
        .consume_read(DetExitKind::VmClockState, 0xde000, &mut vmclock_buf)
        .unwrap();
    println!(
        "  replay VmClockState  -> [{}]",
        fmt_preview(&vmclock_buf, 16)
    );

    let mut mmio_read_buf = [0u8; 2];
    replayer
        .consume_read(DetExitKind::MmioRead, 0x1000, &mut mmio_read_buf)
        .unwrap();
    println!(
        "  replay MmioRead      -> buffer [{}]",
        hex_bytes(&mmio_read_buf)
    );

    replayer
        .validate_write(DetExitKind::MmioWrite, 0x1004, &[0xbe, 0xef])
        .unwrap();
    println!("  replay MmioWrite     -> validated buffer [be ef]");

    let mut pio_read_buf = [0u8; 1];
    replayer
        .consume_read(DetExitKind::IoIn, 0x3f8, &mut pio_read_buf)
        .unwrap();
    println!(
        "  replay PioIn         -> buffer [{}]",
        hex_bytes(&pio_read_buf)
    );

    replayer
        .validate_write(DetExitKind::IoOut, 0x3f8, &[0x5a, 0x6b])
        .unwrap();
    println!("  replay PioOut        -> validated buffer [5a 6b]");
    println!();

    println!("Notice what happened:");
    println!("  - The IRQ events were stored in the same log stream.");
    println!("  - Replay skipped them because they are diagnostic-only.");
    println!("  - VMClock, MMIO, and PIO events drove replay directly.");
    println!();

    let err = replayer
        .consume_read(DetExitKind::MmioRead, 0x0, &mut [0u8; 1])
        .unwrap_err();
    println!("Final replay status after events are consumed:");
    println!("  {err}");
}
