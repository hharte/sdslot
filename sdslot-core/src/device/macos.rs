// SPDX-License-Identifier: MIT OR Apache-2.0
//! macOS raw disk access (design §5.3): the raw character device
//! `/dev/rdiskN` (buffered `/dev/diskN` is dramatically slower), unmounted
//! first via `diskutil unmountDisk` — the approach used by balenaEtcher et
//! al. Capacity via `DKIOCGETBLOCKCOUNT` / `DKIOCGETBLOCKSIZE`.

use std::ffi::CString;
use std::io;
use std::process::Command;

use super::{AccessMode, DeviceInfo, RawDevice};
use crate::error::{Error, Result};

// <sys/disk.h>
const DKIOCGETBLOCKSIZE: libc::c_ulong = 0x4004_6418; // _IOR('d', 24, u32)
const DKIOCGETBLOCKCOUNT: libc::c_ulong = 0x4008_6419; // _IOR('d', 25, u64)

pub struct MacDevice {
    fd: libc::c_int,
    sector_size: u32,
    capacity: u64,
}

/// EPERM on a raw disk device is macOS's TCC gate, not a credentials
/// problem: the open already cleared the file-mode check (an unprivileged
/// open of a `root:operator` device node fails with EACCES instead), and was
/// then denied above the filesystem. Elevation cannot lift it — root does not
/// bypass TCC — so "re-run with sudo" sends the user in circles, typically
/// after they have already authenticated at the GUI's elevation prompt.
fn full_disk_access_hint() -> String {
    let prefix = if unsafe { libc::geteuid() } == 0 {
        "already running as root — "
    } else {
        ""
    };
    format!(
        "{prefix}macOS requires Full Disk Access for raw disk devices: grant it to the app that \
         launched sdslot (sdslot-gui.app, Terminal, iTerm2, …) under System Settings > Privacy & \
         Security > Full Disk Access, then quit and relaunch that app"
    )
}

fn last_err(ctx: &str) -> Error {
    let e = io::Error::last_os_error();
    let hint = match e.raw_os_error() {
        Some(libc::EPERM) => format!(" ({})", full_disk_access_hint()),
        Some(libc::EACCES) => format!(" ({})", super::elevation_hint()),
        _ => String::new(),
    };
    Error::Device(format!("{ctx}: {e}{hint}"))
}

/// "/dev/rdisk4" -> "disk4" (what diskutil wants).
fn disk_name(path: &str) -> Option<&str> {
    let name = path.strip_prefix("/dev/")?;
    Some(name.strip_prefix('r').unwrap_or(name))
}

impl MacDevice {
    pub fn open(path: &str, mode: AccessMode) -> Result<MacDevice> {
        if mode == AccessMode::Write {
            let name = disk_name(path).ok_or_else(|| {
                Error::Validation(format!("bad device path {path:?}: expected /dev/rdiskN"))
            })?;
            let status = Command::new("diskutil")
                .args(["unmountDisk", name])
                .output()
                .map_err(|e| Error::Device(format!("cannot run diskutil: {e}")))?;
            if !status.status.success() {
                return Err(Error::Device(format!(
                    "diskutil unmountDisk {name} failed: {}",
                    String::from_utf8_lossy(&status.stderr).trim()
                )));
            }
        }
        let cpath = CString::new(path)
            .map_err(|_| Error::Validation(format!("bad device path {path:?}")))?;
        let flags = match mode {
            AccessMode::Read => libc::O_RDONLY | libc::O_CLOEXEC,
            AccessMode::Write => libc::O_RDWR | libc::O_CLOEXEC,
        };
        let fd = unsafe { libc::open(cpath.as_ptr(), flags) };
        if fd < 0 {
            return Err(last_err(&format!("cannot open {path}")));
        }
        let op = match mode {
            AccessMode::Read => libc::LOCK_SH | libc::LOCK_NB,
            AccessMode::Write => libc::LOCK_EX | libc::LOCK_NB,
        };
        if unsafe { libc::flock(fd, op) } != 0 {
            unsafe { libc::close(fd) };
            return Err(last_err(&format!(
                "cannot lock {path} (another sdslot operation in progress?)"
            )));
        }
        let mut bsize: u32 = 0;
        if unsafe { libc::ioctl(fd, DKIOCGETBLOCKSIZE as _, &mut bsize) } != 0 {
            unsafe { libc::close(fd) };
            return Err(last_err(&format!("{path}: DKIOCGETBLOCKSIZE failed")));
        }
        let mut bcount: u64 = 0;
        if unsafe { libc::ioctl(fd, DKIOCGETBLOCKCOUNT as _, &mut bcount) } != 0 {
            unsafe { libc::close(fd) };
            return Err(last_err(&format!("{path}: DKIOCGETBLOCKCOUNT failed")));
        }
        Ok(MacDevice {
            fd,
            sector_size: bsize,
            capacity: bcount * u64::from(bsize),
        })
    }
}

/// Eject the disk in `path` via `diskutil eject` — the supported way to
/// offline removable media on macOS. The writing fd must already be closed
/// so diskutil doesn't see the device as busy.
pub fn eject(path: &str) -> Result<()> {
    let name = disk_name(path).ok_or_else(|| {
        Error::Validation(format!("bad device path {path:?}: expected /dev/rdiskN"))
    })?;
    let output = Command::new("diskutil")
        .args(["eject", name])
        .output()
        .map_err(|e| Error::Device(format!("cannot run diskutil: {e}")))?;
    if !output.status.success() {
        return Err(Error::Device(format!(
            "diskutil eject {name} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

impl Drop for MacDevice {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

impl RawDevice for MacDevice {
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

pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir("/dev") {
        Ok(e) => e,
        Err(_) => return Ok(out),
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            // Whole disks only: "disk0", not "disk0s1", and not the raw alias.
            let rest = n.strip_prefix("disk")?;
            rest.chars().all(|c| c.is_ascii_digit()).then_some(n)
        })
        .collect();
    names.sort();

    let system_disk = system_disk_name();
    for name in names {
        let info = diskutil_info(&name);
        // Skip hdiutil/APFS virtual disk images (Docker Desktop, Xcode
        // Simulator, mounted .dmg/.sparsebundle, Time Machine local
        // snapshots, …): `diskutil info` reports these as "Protocol: Disk
        // Image" and/or "Virtual: Yes", and they otherwise pass the
        // removable-media filter, flooding the picker with entries that are
        // never a real SD card. drivelist (balenaEtcher's device layer)
        // filters the same way.
        if info.virtual_device {
            continue;
        }
        out.push(DeviceInfo {
            path: format!("/dev/r{name}"),
            model: info.model,
            bus: info.bus,
            size_bytes: info.size_bytes,
            sector_size: None,
            removable: info.removable,
            system: system_disk.as_deref() == Some(&name),
        });
    }
    Ok(out)
}

struct DiskutilInfo {
    model: String,
    bus: String,
    size_bytes: Option<u64>,
    removable: Option<bool>,
    /// An hdiutil/APFS virtual disk image rather than physical media
    /// (`Protocol: Disk Image` and/or `Virtual: Yes`).
    virtual_device: bool,
}

/// Best-effort parse of `diskutil info <disk>` text output.
fn diskutil_info(name: &str) -> DiskutilInfo {
    let mut info = DiskutilInfo {
        model: "(unknown)".to_string(),
        bus: "unknown".to_string(),
        size_bytes: None,
        removable: None,
        virtual_device: false,
    };
    let Ok(output) = Command::new("diskutil").args(["info", name]).output() else {
        return info;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "Device / Media Name" => info.model = value.to_string(),
            "Protocol" => {
                info.bus = value.to_ascii_lowercase();
                if info.bus.contains("disk image") {
                    info.virtual_device = true;
                }
            }
            "Virtual" => {
                if value.starts_with("Yes") {
                    info.virtual_device = true;
                }
            }
            "Removable Media" => info.removable = Some(value.eq_ignore_ascii_case("removable")),
            "Disk Size" => {
                // e.g. "31.9 GB (31914983424 Bytes) (exactly 62333952 512-Byte-Units)"
                if let Some(bytes) = value
                    .split('(')
                    .nth(1)
                    .and_then(|s| s.split_whitespace().next())
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    info.size_bytes = Some(bytes);
                }
            }
            _ => {}
        }
    }
    info
}

/// Whole disk backing the boot volume, e.g. "disk0".
fn system_disk_name() -> Option<String> {
    let output = Command::new("diskutil").args(["info", "/"]).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(':') {
            if key.trim() == "Part of Whole" {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}
