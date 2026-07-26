// SPDX-License-Identifier: MIT OR Apache-2.0
//! Linux raw block device access (design §5.3): `O_EXCL` opens (the kernel
//! refuses `O_EXCL` on a block device with mounted partitions — a free,
//! race-free safety check), `O_DIRECT` by default, `BLKGETSIZE64` /
//! `BLKSSZGET` ioctls, and enumeration by walking `/sys/block`.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::{AccessMode, DeviceInfo, RawDevice};
use crate::error::{Error, Result};

// <linux/fs.h>: not exposed by the libc crate.
const BLKSSZGET: libc::c_ulong = 0x1268; // _IO(0x12, 104)
const BLKGETSIZE64: libc::c_ulong = 0x8008_1272; // _IOR(0x12, 114, size_t)

pub struct LinuxDevice {
    fd: libc::c_int,
    sector_size: u32,
    capacity: u64,
}

fn last_err(ctx: &str) -> Error {
    let e = io::Error::last_os_error();
    let hint = if e.raw_os_error() == Some(libc::EACCES) {
        format!(" ({})", super::elevation_hint())
    } else {
        String::new()
    };
    Error::Device(format!("{ctx}: {e}{hint}"))
}

impl LinuxDevice {
    pub fn open(path: &str, mode: AccessMode) -> Result<LinuxDevice> {
        let cpath = CString::new(path)
            .map_err(|_| Error::Validation(format!("bad device path {path:?}")))?;
        let flags = match mode {
            AccessMode::Read => libc::O_RDONLY | libc::O_DIRECT | libc::O_CLOEXEC,
            AccessMode::Write => libc::O_RDWR | libc::O_EXCL | libc::O_DIRECT | libc::O_CLOEXEC,
        };
        let fd = unsafe { libc::open(cpath.as_ptr(), flags) };
        if fd < 0 {
            let e = io::Error::last_os_error();
            let busy = if e.raw_os_error() == Some(libc::EBUSY) {
                " (device is in use — unmount its filesystems first)"
            } else {
                ""
            };
            return Err(last_err(&format!("cannot open {path}{busy}")));
        }
        let dev = LinuxDevice {
            fd,
            sector_size: 512,
            capacity: 0,
        };

        // Shared/exclusive advisory lock so concurrent sdslot invocations
        // cannot tear each other (design §4: reads take a shared lock).
        let op = match mode {
            AccessMode::Read => libc::LOCK_SH | libc::LOCK_NB,
            AccessMode::Write => libc::LOCK_EX | libc::LOCK_NB,
        };
        if unsafe { libc::flock(fd, op) } != 0 {
            return Err(last_err(&format!(
                "cannot lock {path} (another sdslot operation in progress?)"
            )));
        }

        let mut ssz: libc::c_int = 0;
        if unsafe { libc::ioctl(fd, BLKSSZGET as _, &mut ssz) } != 0 {
            return Err(last_err(&format!("{path}: BLKSSZGET failed")));
        }
        let mut size: u64 = 0;
        if unsafe { libc::ioctl(fd, BLKGETSIZE64 as _, &mut size) } != 0 {
            return Err(last_err(&format!("{path}: BLKGETSIZE64 failed")));
        }
        Ok(LinuxDevice {
            sector_size: ssz as u32,
            capacity: size,
            ..dev
        })
    }
}

impl Drop for LinuxDevice {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

impl RawDevice for LinuxDevice {
    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn capacity_bytes(&self) -> u64 {
        self.capacity
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let n = unsafe {
                libc::pwrite(
                    self.fd,
                    buf[done..].as_ptr() as *const libc::c_void,
                    buf.len() - done,
                    (offset + done as u64) as libc::off_t,
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(last_err(&format!("write at offset {offset}")));
            }
            done += n as usize;
        }
        Ok(())
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let n = unsafe {
                libc::pread(
                    self.fd,
                    buf[done..].as_mut_ptr() as *mut libc::c_void,
                    buf.len() - done,
                    (offset + done as u64) as libc::off_t,
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(last_err(&format!("read at offset {offset}")));
            }
            if n == 0 {
                return Err(Error::Device(format!(
                    "unexpected EOF at offset {}",
                    offset + done as u64
                )));
            }
            done += n as usize;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if unsafe { libc::fsync(self.fd) } != 0 {
            return Err(last_err("fsync failed"));
        }
        Ok(())
    }
}

fn read_sys(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// The whole-disk device that backs the root filesystem, e.g. "sda" or
/// "nvme0n1", so it can be flagged as the system disk.
fn root_disk_name() -> Option<String> {
    let mounts = std::fs::read_to_string("/proc/self/mounts").ok()?;
    let src = mounts.lines().find_map(|l| {
        let mut f = l.split_whitespace();
        let dev = f.next()?;
        let mnt = f.next()?;
        (mnt == "/" && dev.starts_with("/dev/")).then(|| dev.to_string())
    })?;
    let name = src.strip_prefix("/dev/")?.to_string();
    // Strip a partition suffix: "sda2" -> "sda", "nvme0n1p3" -> "nvme0n1",
    // "mmcblk0p1" -> "mmcblk0".
    if let Some(pos) = name.rfind('p').filter(|&p| {
        name[p + 1..].chars().all(|c| c.is_ascii_digit())
            && !name[p + 1..].is_empty()
            && name[..p].chars().last().is_some_and(|c| c.is_ascii_digit())
    }) {
        return Some(name[..pos].to_string());
    }
    Some(
        name.trim_end_matches(|c: char| c.is_ascii_digit())
            .to_string(),
    )
}

pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    let root = root_disk_name();
    let mut out = Vec::new();
    let entries = match std::fs::read_dir("/sys/block") {
        Ok(e) => e,
        Err(_) => return Ok(out),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = String::from_utf8_lossy(name.as_bytes()).to_string();
        // Skip synthetic/loopback block devices: RAM disks and loop devices
        // (`/dev/loopN` — routinely dozens of them from snap packages,
        // squashfs mounts, container image layers, ...) are never a real
        // SD card, the same category macOS's virtual disk images are.
        if name_str.starts_with("ram")
            || name_str.starts_with("zram")
            || name_str.starts_with("loop")
        {
            continue;
        }
        let sys = entry.path();
        let size_sectors: Option<u64> = read_sys(&sys.join("size")).and_then(|s| s.parse().ok());
        // /sys/block/X/size is always in 512-byte units regardless of the
        // device's logical sector size.
        let size_bytes = size_sectors.map(|s| s * 512);
        if size_bytes == Some(0) {
            continue; // e.g. an empty loop device or card reader with no media
        }
        let removable = read_sys(&sys.join("removable")).map(|s| s == "1");
        let model = read_sys(&sys.join("device/model"))
            .or_else(|| read_sys(&sys.join("device/name")))
            .unwrap_or_else(|| "(unknown)".to_string());
        let sector_size: Option<u32> =
            read_sys(&sys.join("queue/logical_block_size")).and_then(|s| s.parse().ok());
        let bus = if name_str.starts_with("nvme") {
            "nvme"
        } else if name_str.starts_with("mmcblk") {
            "mmc"
        } else {
            // Resolve the device symlink and look for the transport in it.
            let target = std::fs::read_link(sys.join("device"))
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            if target.contains("usb") {
                "usb"
            } else if target.contains("ata") {
                "ata"
            } else {
                "unknown"
            }
        };
        out.push(DeviceInfo {
            path: format!("/dev/{name_str}"),
            model,
            bus: bus.to_string(),
            size_bytes,
            sector_size,
            removable,
            system: root.as_deref() == Some(&name_str),
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}
