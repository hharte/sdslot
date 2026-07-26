// SPDX-License-Identifier: MIT OR Apache-2.0
//! Optional on-card table of contents (design §2.6). A small versioned
//! structure in a reserved region: magic, layout hash, then per-slot records
//! (bank, unit, image byte length, SHA-256, name, timestamp). Ignored by the
//! FPGA; lets `status` and the GUI describe a card with no host-side records.
//!
//! On-card format: a fixed 48-byte header — magic (8), version (u32 LE),
//! payload length (u32 LE), payload SHA-256 (32) — followed by a JSON
//! payload, zero-padded to a sector boundary. The whole region is
//! `TOC_REGION_LEN` bytes.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::device::RawDevice;
use crate::engine::AlignedBuf;
use crate::error::{Error, Result};
use crate::layout::hex;

pub const TOC_MAGIC: [u8; 8] = *b"SDSLTOC\x01";
pub const TOC_VERSION: u32 = 1;
/// Reserved on-card size of the TOC region.
pub const TOC_REGION_LEN: u64 = 128 * 1024;
const HEADER_LEN: usize = 48;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TocEntry {
    pub bank: String,
    pub unit: u32,
    /// Absolute device byte offset of the slot, so a card can be described
    /// (and hashed) with no manifest at hand.
    pub offset: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Meaningful byte length of the slot's content (the image length as
    /// written), used by `read --length toc`.
    pub length: u64,
    /// SHA-256 of those bytes at write time, lowercase hex.
    pub sha256: String,
    /// Unix seconds at write time.
    pub written_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Toc {
    pub version: u32,
    /// `Layout::layout_hash()` of the manifest the card was written with.
    pub layout_hash: String,
    pub entries: Vec<TocEntry>,
}

impl Toc {
    pub fn new(layout_hash: String) -> Toc {
        Toc {
            version: TOC_VERSION,
            layout_hash,
            entries: Vec::new(),
        }
    }

    pub fn find(&self, bank: &str, unit: u32) -> Option<&TocEntry> {
        self.entries
            .iter()
            .find(|e| e.bank == bank && e.unit == unit)
    }

    pub fn upsert(&mut self, entry: TocEntry) {
        match self
            .entries
            .iter_mut()
            .find(|e| e.bank == entry.bank && e.unit == entry.unit)
        {
            Some(e) => *e = entry,
            None => self.entries.push(entry),
        }
    }

    pub fn remove(&mut self, bank: &str, unit: u32) {
        self.entries.retain(|e| !(e.bank == bank && e.unit == unit));
    }
}

/// Read the TOC at `offset`. Returns `Ok(None)` when the region holds no
/// valid TOC (bad magic, corrupt header, payload hash mismatch).
pub fn read_toc(dev: &mut dyn RawDevice, offset: u64) -> Result<Option<Toc>> {
    let sector = u64::from(dev.sector_size());
    let first = sector.clamp(4096, TOC_REGION_LEN);
    let mut buf = AlignedBuf::new(first as usize);
    dev.read_at(offset, &mut buf)?;

    if buf[..8] != TOC_MAGIC {
        return Ok(None);
    }
    let version = u32::from_le_bytes(buf[8..12].try_into().unwrap());
    let payload_len = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
    let payload_sha: [u8; 32] = buf[16..48].try_into().unwrap();
    if version != TOC_VERSION || payload_len as u64 > TOC_REGION_LEN - HEADER_LEN as u64 {
        return Ok(None);
    }

    let total = HEADER_LEN + payload_len;
    let mut payload = vec![0u8; payload_len];
    if total <= buf.len() {
        payload.copy_from_slice(&buf[HEADER_LEN..total]);
    } else {
        let round = total.div_ceil(sector as usize) * sector as usize;
        let mut big = AlignedBuf::new(round);
        dev.read_at(offset, &mut big)?;
        payload.copy_from_slice(&big[HEADER_LEN..total]);
    }

    let digest: [u8; 32] = Sha256::digest(&payload).into();
    if digest != payload_sha {
        return Ok(None);
    }
    match serde_json::from_slice::<Toc>(&payload) {
        Ok(t) => Ok(Some(t)),
        Err(_) => Ok(None),
    }
}

/// Serialize and write the TOC at `offset` (one aligned write, then it is up
/// to the caller to `flush()`).
pub fn write_toc(dev: &mut dyn RawDevice, offset: u64, toc: &Toc) -> Result<()> {
    let payload = serde_json::to_vec(toc)
        .map_err(|e| Error::Validation(format!("TOC serialize error: {e}")))?;
    if (HEADER_LEN + payload.len()) as u64 > TOC_REGION_LEN {
        return Err(Error::Validation(format!(
            "TOC payload ({} bytes) exceeds the {TOC_REGION_LEN}-byte TOC region",
            payload.len()
        )));
    }
    let sector = dev.sector_size() as usize;
    let total = (HEADER_LEN + payload.len()).div_ceil(sector) * sector;
    let mut buf = AlignedBuf::new(total);
    buf[..8].copy_from_slice(&TOC_MAGIC);
    buf[8..12].copy_from_slice(&TOC_VERSION.to_le_bytes());
    buf[12..16].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    let digest: [u8; 32] = Sha256::digest(&payload).into();
    buf[16..48].copy_from_slice(&digest);
    buf[HEADER_LEN..HEADER_LEN + payload.len()].copy_from_slice(&payload);
    dev.write_at(offset, &buf)
}

/// Search a card for a TOC with no manifest at hand: probe each 1 MiB
/// boundary (TOC regions are sector-aligned in practice at far coarser
/// granularity) for the magic, returning the first valid TOC found.
pub fn probe_toc(dev: &mut dyn RawDevice) -> Result<Option<(u64, Toc)>> {
    const STEP: u64 = 1 << 20;
    let capacity = dev.capacity_bytes();
    let sector = u64::from(dev.sector_size());
    let mut buf = AlignedBuf::new(sector as usize);
    let mut offset = 0u64;
    while offset + sector <= capacity {
        dev.read_at(offset, &mut buf)?;
        if buf[..8] == TOC_MAGIC {
            if let Some(t) = read_toc(dev, offset)? {
                return Ok(Some((offset, t)));
            }
        }
        offset += STEP;
    }
    Ok(None)
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(bank: &str, unit: u32, length: u64) -> TocEntry {
        TocEntry {
            bank: bank.to_string(),
            unit,
            offset: 0,
            name: None,
            length,
            sha256: sha256_hex(b"x"),
            written_unix: 0,
        }
    }

    #[test]
    fn upsert_replaces_and_remove_drops() {
        let mut toc = Toc::new("hash".into());
        toc.upsert(entry("rl", 0, 100));
        toc.upsert(entry("rl", 1, 200));
        toc.upsert(entry("rl", 0, 300)); // replaces, not duplicates
        assert_eq!(toc.entries.len(), 2);
        assert_eq!(toc.find("rl", 0).unwrap().length, 300);
        toc.remove("rl", 0);
        assert!(toc.find("rl", 0).is_none());
        assert!(toc.find("rl", 1).is_some());
    }

    #[test]
    fn header_constants_are_stable() {
        // On-card format invariants: changing these breaks existing cards.
        assert_eq!(TOC_MAGIC, *b"SDSLTOC\x01");
        assert_eq!(TOC_VERSION, 1);
        assert_eq!(HEADER_LEN, 48);
        assert_eq!(TOC_REGION_LEN, 128 * 1024);
    }

    #[test]
    fn test_toc_read_write_probe() {
        use crate::device::AccessMode;
        use crate::device::FileDevice;

        assert!(now_unix() > 0);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("card.img");

        let mut dev = FileDevice::open(&path, AccessMode::Write, 512).unwrap();
        dev.ensure_len(2 * 1024 * 1024).unwrap();

        let probed = probe_toc(&mut dev).unwrap();
        assert!(probed.is_none());

        let mut toc = Toc::new("some-layout-hash".into());
        toc.upsert(entry("rl", 0, 10_485_760));
        toc.upsert(entry("rp", 0, 200_000_000));

        let offset = 1024 * 1024;
        write_toc(&mut dev, offset, &toc).unwrap();

        let read = read_toc(&mut dev, offset)
            .unwrap()
            .expect("should read toc");
        assert_eq!(read.layout_hash, "some-layout-hash");
        assert_eq!(read.entries.len(), 2);

        let probed = probe_toc(&mut dev).unwrap().expect("should probe toc");
        assert_eq!(probed.0, offset);
        assert_eq!(probed.1.layout_hash, "some-layout-hash");

        let mut big_toc = Toc::new("big-layout-hash".into());
        for i in 0..30 {
            big_toc.upsert(entry("rl", i, 10_485_760));
        }
        write_toc(&mut dev, offset, &big_toc).unwrap();
        let read_big = read_toc(&mut dev, offset)
            .unwrap()
            .expect("should read big toc");
        assert_eq!(read_big.entries.len(), 30);

        let payload = b"{invalid-json}";
        let sector = 512;
        let total = (HEADER_LEN + payload.len()).div_ceil(sector) * sector;
        let mut buf = AlignedBuf::new(total);
        buf[..8].copy_from_slice(&TOC_MAGIC);
        buf[8..12].copy_from_slice(&TOC_VERSION.to_le_bytes());
        buf[12..16].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        let digest: [u8; 32] = Sha256::digest(payload).into();
        buf[16..48].copy_from_slice(&digest);
        buf[HEADER_LEN..HEADER_LEN + payload.len()].copy_from_slice(payload);
        dev.write_at(offset, &buf).unwrap();

        let corrupt_json = read_toc(&mut dev, offset).unwrap();
        assert!(corrupt_json.is_none());

        buf[HEADER_LEN] ^= 0xff;
        dev.write_at(offset, &buf).unwrap();
        let corrupt_sha = read_toc(&mut dev, offset).unwrap();
        assert!(corrupt_sha.is_none());
    }
}
