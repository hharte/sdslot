// SPDX-License-Identifier: MIT OR Apache-2.0
//! Raw device access (design §5.1, §5.3). The engine performs all alignment,
//! chunking, and padding, so platform implementations stay minimal and
//! identical in contract: sector-aligned `read_at`/`write_at` plus capacity
//! and sector-size queries.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Result;

mod file;
pub use file::FileDevice;
pub mod hotplug;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    /// Shared access for extraction/status: a concurrent writer is excluded
    /// but other readers are fine.
    Read,
    /// Exclusive access for writes: fails if the device is in use (mounted)
    /// and cannot be safely claimed.
    Write,
}

/// One enumerated candidate device (design §3 `sdslot list`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub path: String,
    pub model: String,
    pub bus: String,
    pub size_bytes: Option<u64>,
    pub sector_size: Option<u32>,
    /// `None` when the removable flag could not be determined (treated as
    /// non-removable by the safety rails).
    pub removable: Option<bool>,
    /// The system/boot disk: refused for writing even with `--force`.
    pub system: bool,
}

pub trait RawDevice {
    /// Logical sector size (verified against the manifest by the engine).
    fn sector_size(&self) -> u32;

    /// Total capacity in bytes (current length for file-backed devices).
    fn capacity_bytes(&self) -> u64;

    /// True for file-backed devices, which grow on write and zero-fill reads
    /// past EOF; the engine skips fixed-capacity bounds checks for them.
    fn growable(&self) -> bool {
        false
    }

    /// `offset` and `buf.len()` are guaranteed sector-aligned by the engine,
    /// and `buf` is 4 KiB-aligned in memory.
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()>;
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// Durable before "done" is reported.
    fn flush(&mut self) -> Result<()>;

    /// For growable (file-backed) devices: ensure the backing store is at
    /// least `bytes` long, so a flat image spans the full card layout.
    /// No-op on real block devices.
    fn ensure_len(&mut self, _bytes: u64) -> Result<()> {
        Ok(())
    }
}

/// True when `path` names a platform raw device rather than a regular file.
pub fn is_platform_device_path(path: &str) -> bool {
    let p = path.trim();
    p.starts_with("\\\\.\\") || p.starts_with("//./") || p.starts_with("/dev/")
}

/// Open either a platform raw device or a regular file (design §9
/// file-backed mode). `expected_sector` is the manifest's sector size; file
/// devices adopt it, platform devices report their real one and the engine
/// rejects a mismatch.
pub fn open_device(
    path: &str,
    mode: AccessMode,
    expected_sector: u32,
) -> Result<Box<dyn RawDevice>> {
    if is_platform_device_path(path) {
        open_platform_device(path, mode)
    } else {
        Ok(Box::new(FileDevice::open(
            Path::new(path),
            mode,
            expected_sector,
        )?))
    }
}

#[cfg(windows)]
fn open_platform_device(path: &str, mode: AccessMode) -> Result<Box<dyn RawDevice>> {
    Ok(Box::new(windows::WinDevice::open(path, mode)?))
}

#[cfg(target_os = "linux")]
fn open_platform_device(path: &str, mode: AccessMode) -> Result<Box<dyn RawDevice>> {
    Ok(Box::new(linux::LinuxDevice::open(path, mode)?))
}

#[cfg(target_os = "macos")]
fn open_platform_device(path: &str, mode: AccessMode) -> Result<Box<dyn RawDevice>> {
    Ok(Box::new(macos::MacDevice::open(path, mode)?))
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn open_platform_device(_path: &str, _mode: AccessMode) -> Result<Box<dyn RawDevice>> {
    Err(crate::error::Error::Device(
        "raw device access is not supported on this platform".into(),
    ))
}

/// Enumerate candidate block devices (size, model, bus, removable flag).
pub fn enumerate_devices() -> Result<Vec<DeviceInfo>> {
    #[cfg(windows)]
    {
        windows::enumerate()
    }
    #[cfg(target_os = "linux")]
    {
        linux::enumerate()
    }
    #[cfg(target_os = "macos")]
    {
        macos::enumerate()
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        Ok(Vec::new())
    }
}

/// Platform-appropriate hint appended to permission errors (design §5.3).
pub fn elevation_hint() -> &'static str {
    #[cfg(windows)]
    {
        "run from an elevated (Administrator) prompt"
    }
    #[cfg(not(windows))]
    {
        "re-run with sudo"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockDevice;
    impl RawDevice for MockDevice {
        fn sector_size(&self) -> u32 {
            512
        }
        fn capacity_bytes(&self) -> u64 {
            0
        }
        fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> Result<()> {
            Ok(())
        }
        fn read_at(&mut self, _offset: u64, _buf: &mut [u8]) -> Result<()> {
            Ok(())
        }
        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_device_defaults() {
        let mut d = MockDevice;
        assert!(!d.growable());
        assert!(d.ensure_len(100).is_ok());
    }

    #[test]
    fn test_open_device_errors() {
        let res = open_device("", AccessMode::Read, 512);
        assert!(res.is_err());
    }
}
