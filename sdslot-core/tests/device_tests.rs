// SPDX-License-Identifier: MIT OR Apache-2.0
//! Device-layer tests: the file-backed device's contract (zero-fill reads
//! past EOF, growth semantics), platform-path dispatch, and a metadata-only
//! smoke of the real enumerator (safe: it never touches media contents).

use sdslot_core::device::{
    elevation_hint, enumerate_devices, open_device, AccessMode, FileDevice, RawDevice,
};

#[test]
fn file_device_zero_fills_past_eof_and_grows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dev.img");
    let mut dev = FileDevice::open(&path, AccessMode::Write, 512).unwrap();
    assert_eq!(dev.sector_size(), 512);
    assert!(dev.growable());

    dev.write_at(1024, &[0xAAu8; 512]).unwrap();
    assert_eq!(dev.capacity_bytes(), 1536);

    // Read spanning written data and EOF: the tail comes back as zeros.
    let mut buf = [0xFFu8; 1024];
    dev.read_at(1024, &mut buf).unwrap();
    assert!(buf[..512].iter().all(|&b| b == 0xAA));
    assert!(buf[512..].iter().all(|&b| b == 0));

    // ensure_len grows but never shrinks.
    dev.ensure_len(4096).unwrap();
    assert_eq!(dev.capacity_bytes(), 4096);
    dev.ensure_len(2048).unwrap();
    assert_eq!(dev.capacity_bytes(), 4096);
    dev.flush().unwrap();
}

#[test]
fn file_device_read_mode_requires_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.img");
    assert!(FileDevice::open(&missing, AccessMode::Read, 512).is_err());
    // Read-only open of an existing file works and flush is a no-op.
    std::fs::write(dir.path().join("x.img"), [1u8; 512]).unwrap();
    let mut dev = FileDevice::open(&dir.path().join("x.img"), AccessMode::Read, 512).unwrap();
    let mut buf = [0u8; 512];
    dev.read_at(0, &mut buf).unwrap();
    assert_eq!(buf[0], 1);
    dev.flush().unwrap();
}

#[test]
fn platform_device_open_fails_cleanly_for_nonexistent_device() {
    // Metadata-free open of a device that cannot exist: exercises the
    // platform open + error classification without touching real media.
    let bogus = if cfg!(windows) {
        "\\\\.\\PhysicalDrive91"
    } else {
        "/dev/sdslot-no-such-device"
    };
    assert!(open_device(bogus, AccessMode::Read, 512).is_err());
}

#[test]
fn enumeration_is_metadata_only_and_total() {
    // Runs the real per-platform enumerator; it opens devices with metadata
    // access only, so this is safe anywhere. The result set may legitimately
    // be empty (locked-down CI), but the call must not fail or panic.
    let devices = enumerate_devices().expect("enumeration should not error");
    for d in &devices {
        assert!(!d.path.is_empty());
        assert!(!d.model.is_empty());
        assert!(!d.bus.is_empty());
    }
    // At most one device is flagged as the system disk.
    assert!(devices.iter().filter(|d| d.system).count() <= 1);
    assert!(!elevation_hint().is_empty());
}
