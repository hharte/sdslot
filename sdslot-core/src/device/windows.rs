// SPDX-License-Identifier: MIT OR Apache-2.0
//! Windows raw disk access (design §5.3): `\\.\PhysicalDriveN` opened with
//! `FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH`. Before writing, every
//! volume residing on the target disk is locked (`FSCTL_LOCK_VOLUME`) and
//! dismounted (`FSCTL_DISMOUNT_VOLUME`), and the handles — and thus the
//! locks — are held for the duration of the operation.

use std::ffi::c_void;
use std::io;
use std::iter::once;

use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, FlushFileBuffers, ReadFile,
    SetFilePointerEx, WriteFile, FILE_BEGIN, FILE_FLAG_NO_BUFFERING, FILE_FLAG_WRITE_THROUGH,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::{
    PropertyStandardQuery, StorageDeviceProperty, DISK_GEOMETRY_EX, FSCTL_DISMOUNT_VOLUME,
    FSCTL_LOCK_VOLUME, GET_LENGTH_INFORMATION, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
    IOCTL_DISK_GET_LENGTH_INFO, IOCTL_STORAGE_EJECT_MEDIA, IOCTL_STORAGE_GET_DEVICE_NUMBER,
    IOCTL_STORAGE_MEDIA_REMOVAL, IOCTL_STORAGE_QUERY_PROPERTY, PREVENT_MEDIA_REMOVAL,
    STORAGE_DEVICE_DESCRIPTOR, STORAGE_DEVICE_NUMBER, STORAGE_PROPERTY_QUERY,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use super::{AccessMode, DeviceInfo, RawDevice};
use crate::error::{Error, Result};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(once(0)).collect()
}

fn last_err(ctx: &str) -> Error {
    let e = io::Error::last_os_error();
    let hint = if e.raw_os_error() == Some(5) {
        format!(" ({})", super::elevation_hint())
    } else {
        String::new()
    };
    Error::Device(format!("{ctx}: {e}{hint}"))
}

/// An open handle that closes on drop.
struct Handle(HANDLE);

impl Handle {
    fn open(path: &str, access: u32, flags: u32) -> io::Result<Handle> {
        let w = wide(path);
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                flags,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Handle(h))
        }
    }

    fn ioctl(
        &self,
        code: u32,
        input: Option<&[u8]>,
        output: Option<&mut [u8]>,
    ) -> io::Result<usize> {
        let mut returned: u32 = 0;
        let (in_ptr, in_len) = match input {
            Some(b) => (b.as_ptr() as *const c_void, b.len() as u32),
            None => (std::ptr::null(), 0),
        };
        let (out_ptr, out_len) = match output {
            Some(b) => (b.as_mut_ptr() as *mut c_void, b.len() as u32),
            None => (std::ptr::null_mut(), 0),
        };
        let ok = unsafe {
            DeviceIoControl(
                self.0,
                code,
                in_ptr,
                in_len,
                out_ptr,
                out_len,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(returned as usize)
        }
    }

    fn device_number(&self) -> io::Result<u32> {
        let mut out = [0u8; std::mem::size_of::<STORAGE_DEVICE_NUMBER>()];
        self.ioctl(IOCTL_STORAGE_GET_DEVICE_NUMBER, None, Some(&mut out))?;
        let sdn = unsafe { &*(out.as_ptr() as *const STORAGE_DEVICE_NUMBER) };
        Ok(sdn.DeviceNumber)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

pub struct WinDevice {
    handle: Handle,
    sector_size: u32,
    capacity: u64,
    /// Locked+dismounted volume handles on the target disk; dropping them
    /// releases the locks.
    _volume_locks: Vec<Handle>,
}

impl WinDevice {
    pub fn open(path: &str, mode: AccessMode) -> Result<WinDevice> {
        let access = match mode {
            AccessMode::Read => GENERIC_READ,
            AccessMode::Write => GENERIC_READ | GENERIC_WRITE,
        };
        let handle = Handle::open(
            path,
            access,
            FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH,
        )
        .map_err(|e| classify_open_err(path, e))?;

        let (sector_size, geom_size) = disk_geometry(&handle)
            .map_err(|e| Error::Device(format!("{path}: cannot query disk geometry: {e}")))?;
        let capacity = disk_length(&handle).unwrap_or(geom_size);

        let volume_locks = if mode == AccessMode::Write {
            let disk_number = handle
                .device_number()
                .map_err(|e| Error::Device(format!("{path}: cannot query device number: {e}")))?;
            lock_volumes_on_disk(disk_number)?
        } else {
            Vec::new()
        };

        Ok(WinDevice {
            handle,
            sector_size,
            capacity,
            _volume_locks: volume_locks,
        })
    }
}

fn classify_open_err(path: &str, e: io::Error) -> Error {
    let hint = if e.raw_os_error() == Some(5) {
        format!(" ({})", super::elevation_hint())
    } else {
        String::new()
    };
    Error::Device(format!("cannot open {path}: {e}{hint}"))
}

fn disk_geometry(h: &Handle) -> io::Result<(u32, u64)> {
    let mut out = [0u8; std::mem::size_of::<DISK_GEOMETRY_EX>() + 256];
    h.ioctl(IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, None, Some(&mut out))?;
    let g = unsafe { &*(out.as_ptr() as *const DISK_GEOMETRY_EX) };
    Ok((g.Geometry.BytesPerSector, g.DiskSize as u64))
}

fn disk_length(h: &Handle) -> Option<u64> {
    let mut out = [0u8; std::mem::size_of::<GET_LENGTH_INFORMATION>()];
    h.ioctl(IOCTL_DISK_GET_LENGTH_INFO, None, Some(&mut out))
        .ok()?;
    let li = unsafe { &*(out.as_ptr() as *const GET_LENGTH_INFORMATION) };
    Some(li.Length as u64)
}

/// Lock + dismount every volume on `disk_number`, returning the held handles.
fn lock_volumes_on_disk(disk_number: u32) -> Result<Vec<Handle>> {
    let mut locks = Vec::new();
    let mut name = [0u16; 512];
    let find = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
    if find == INVALID_HANDLE_VALUE {
        return Ok(locks); // no volumes at all
    }
    loop {
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        let vol = String::from_utf16_lossy(&name[..len]);
        // FindFirstVolumeW yields "\\?\Volume{guid}\"; CreateFileW wants it
        // without the trailing backslash.
        let vol_path = vol.trim_end_matches('\\').to_string();
        if let Ok(h) = Handle::open(&vol_path, GENERIC_READ | GENERIC_WRITE, 0) {
            if let Ok(n) = h.device_number() {
                if n == disk_number {
                    h.ioctl(FSCTL_LOCK_VOLUME, None, None).map_err(|e| {
                        Error::Device(format!(
                            "volume {vol_path} on target disk is in use and cannot be locked: {e}"
                        ))
                    })?;
                    h.ioctl(FSCTL_DISMOUNT_VOLUME, None, None).map_err(|e| {
                        Error::Device(format!("cannot dismount volume {vol_path}: {e}"))
                    })?;
                    locks.push(h);
                }
            }
        }
        let more = unsafe { FindNextVolumeW(find, name.as_mut_ptr(), name.len() as u32) };
        if more == 0 {
            break;
        }
    }
    unsafe { FindVolumeClose(find) };
    Ok(locks)
}

/// Eject the media in `path` (`\\.\PhysicalDriveN`). The writing handle must
/// already be closed: its held volume locks would make relocking here fail.
/// Volumes are relocked and dismounted first so the eject isn't refused for
/// files Windows reopened after the write's locks were released.
pub fn eject(path: &str) -> Result<()> {
    let handle = Handle::open(path, GENERIC_READ, 0).map_err(|e| classify_open_err(path, e))?;
    let disk_number = handle
        .device_number()
        .map_err(|e| Error::Device(format!("{path}: cannot query device number: {e}")))?;
    let _locks = lock_volumes_on_disk(disk_number)?;
    // Best-effort: allow removal in case something set the prevent flag.
    let prevent = PREVENT_MEDIA_REMOVAL {
        PreventMediaRemoval: 0,
    };
    let prevent_bytes = unsafe {
        std::slice::from_raw_parts(
            (&prevent as *const PREVENT_MEDIA_REMOVAL) as *const u8,
            std::mem::size_of::<PREVENT_MEDIA_REMOVAL>(),
        )
    };
    let _ = handle.ioctl(IOCTL_STORAGE_MEDIA_REMOVAL, Some(prevent_bytes), None);
    handle
        .ioctl(IOCTL_STORAGE_EJECT_MEDIA, None, None)
        .map_err(|e| Error::Device(format!("cannot eject {path}: {e}")))?;
    Ok(())
}

impl RawDevice for WinDevice {
    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn capacity_bytes(&self) -> u64 {
        self.capacity
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        seek(&self.handle, offset)?;
        let mut written: u32 = 0;
        let ok = unsafe {
            WriteFile(
                self.handle.0,
                buf.as_ptr(),
                buf.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_err(&format!(
                "write of {} bytes at offset {offset} failed",
                buf.len()
            )));
        }
        if written as usize != buf.len() {
            return Err(Error::Device(format!(
                "short write at offset {offset}: {written} of {} bytes",
                buf.len()
            )));
        }
        Ok(())
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        seek(&self.handle, offset)?;
        let mut got: u32 = 0;
        let ok = unsafe {
            ReadFile(
                self.handle.0,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut got,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_err(&format!(
                "read of {} bytes at offset {offset} failed",
                buf.len()
            )));
        }
        if got as usize != buf.len() {
            return Err(Error::Device(format!(
                "short read at offset {offset}: {got} of {} bytes",
                buf.len()
            )));
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        let ok = unsafe { FlushFileBuffers(self.handle.0) };
        if ok == 0 {
            return Err(last_err("flush failed"));
        }
        Ok(())
    }
}

fn seek(h: &Handle, offset: u64) -> Result<()> {
    let ok = unsafe { SetFilePointerEx(h.0, offset as i64, std::ptr::null_mut(), FILE_BEGIN) };
    if ok == 0 {
        return Err(last_err(&format!("seek to offset {offset} failed")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------

fn bus_name(bus_type: u8) -> &'static str {
    // STORAGE_BUS_TYPE values.
    match bus_type {
        1 => "scsi",
        2 => "atapi",
        3 => "ata",
        4 => "1394",
        7 => "usb",
        8 => "raid",
        9 => "iscsi",
        10 => "sas",
        11 => "sata",
        12 => "sd",
        13 => "mmc",
        14 | 15 => "virtual",
        16 => "spaces",
        17 => "nvme",
        _ => "unknown",
    }
}

fn cstr_at(buf: &[u8], offset: u32) -> Option<String> {
    let off = offset as usize;
    if off == 0 || off >= buf.len() {
        return None;
    }
    let end = buf[off..].iter().position(|&b| b == 0)? + off;
    let s = String::from_utf8_lossy(&buf[off..end]).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn query_device_descriptor(h: &Handle) -> io::Result<(String, &'static str, Option<bool>)> {
    let mut query: STORAGE_PROPERTY_QUERY = unsafe { std::mem::zeroed() };
    query.PropertyId = StorageDeviceProperty;
    query.QueryType = PropertyStandardQuery;
    let query_bytes = unsafe {
        std::slice::from_raw_parts(
            (&query as *const STORAGE_PROPERTY_QUERY) as *const u8,
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>(),
        )
    };
    let mut out = vec![0u8; 1024];
    h.ioctl(
        IOCTL_STORAGE_QUERY_PROPERTY,
        Some(query_bytes),
        Some(&mut out),
    )?;
    let desc = unsafe { &*(out.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR) };
    let vendor = cstr_at(&out, desc.VendorIdOffset);
    let product = cstr_at(&out, desc.ProductIdOffset);
    let model = match (vendor, product) {
        (Some(v), Some(p)) => format!("{v} {p}"),
        (Some(v), None) => v,
        (None, Some(p)) => p,
        (None, None) => "(unknown)".to_string(),
    };
    let bus = bus_name(desc.BusType as u8);
    Ok((model, bus, Some(desc.RemovableMedia != 0)))
}

/// Disk number holding the system drive (usually C:), or None.
fn system_disk_number() -> Option<u32> {
    let sysdrive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
    let h = Handle::open(&format!("\\\\.\\{sysdrive}"), 0, 0).ok()?;
    h.device_number().ok()
}

pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    let system_disk = system_disk_number();
    let mut out = Vec::new();
    for n in 0..64u32 {
        let path = format!("\\\\.\\PhysicalDrive{n}");
        // Access 0 is enough for the metadata ioctls and works unelevated.
        let Ok(h) = Handle::open(&path, 0, 0) else {
            continue;
        };
        let (model, bus, removable) = match query_device_descriptor(&h) {
            Ok(v) => v,
            Err(_) => ("(unknown)".to_string(), "unknown", None),
        };
        let (sector, geom_size) = match disk_geometry(&h) {
            Ok(v) => (Some(v.0), Some(v.1)),
            Err(_) => (None, None),
        };
        let size_bytes = disk_length(&h).or(geom_size);
        out.push(DeviceInfo {
            path,
            model,
            bus: bus.to_string(),
            size_bytes,
            sector_size: sector,
            removable,
            system: system_disk == Some(n),
        });
    }
    Ok(out)
}
