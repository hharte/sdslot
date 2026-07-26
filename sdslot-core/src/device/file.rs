// SPDX-License-Identifier: MIT OR Apache-2.0
//! File-backed device (design §9): a regular file with relaxed alignment,
//! for unit tests and for building full card images when that is desired.
//! Reads past EOF return zeros (a fresh card region reads as whatever is
//! there; a fresh file region reads as nothing — zeros are the useful
//! equivalent), and writes extend the file, so capacity reports "growable".

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::{AccessMode, RawDevice};
use crate::error::{dev_err, Result};

pub struct FileDevice {
    file: File,
    sector_size: u32,
    writable: bool,
}

impl FileDevice {
    pub fn open(path: &Path, mode: AccessMode, sector_size: u32) -> Result<FileDevice> {
        let writable = mode == AccessMode::Write;
        let file = OpenOptions::new()
            .read(true)
            .write(writable)
            .create(writable)
            .open(path)
            .map_err(|e| dev_err(format!("cannot open {}", path.display()), e))?;
        Ok(FileDevice {
            file,
            sector_size,
            writable,
        })
    }
}

impl RawDevice for FileDevice {
    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn capacity_bytes(&self) -> u64 {
        self.file.metadata().map(|m| m.len()).unwrap_or(0)
    }

    fn growable(&self) -> bool {
        true
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.file.write_all(buf))
            .map_err(|e| dev_err(format!("write at offset {offset}"), e))
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| dev_err(format!("seek to offset {offset}"), e))?;
        let mut filled = 0;
        while filled < buf.len() {
            match self.file.read(&mut buf[filled..]) {
                Ok(0) => break, // EOF: zero-fill the rest
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(dev_err(format!("read at offset {offset}"), e)),
            }
        }
        buf[filled..].fill(0);
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.writable {
            self.file.sync_all().map_err(|e| dev_err("flush", e))?;
        }
        Ok(())
    }

    fn ensure_len(&mut self, bytes: u64) -> Result<()> {
        let current = self.file.metadata().map_err(|e| dev_err("stat", e))?.len();
        if current < bytes {
            self.file
                .set_len(bytes)
                .map_err(|e| dev_err(format!("extend to {bytes} bytes"), e))?;
        }
        Ok(())
    }
}
