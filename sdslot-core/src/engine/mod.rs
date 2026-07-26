// SPDX-License-Identifier: MIT OR Apache-2.0
//! Write/read engine (design §5.2): resolve slots, validate, stream in
//! aligned chunks with zero-padding to sector boundaries, maintain the TOC,
//! verify, and report progress through an event sink.
//!
//! Operations run through a [`Session`], which pairs an open device with a
//! layout, enforces the sector-size match once, and caches the on-card TOC
//! so multi-slot operations read it a single time.

mod buffer;

pub use buffer::{AlignedBuf, BUFFER_ALIGN};

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::device::RawDevice;
use crate::error::{val_err, Error, Result};
use crate::events::{Event, EventSink, OpKind, PlanOp, SlotState, EVENT_SCHEMA_VERSION};
use crate::layout::{hex, Extent, Layout, SlotAssign};
use crate::toc::{self, Toc, TocEntry};
use crate::units::format_bytes;

pub const DEFAULT_CHUNK_SIZE: usize = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Options and jobs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct EngineOpts {
    pub chunk_size: usize,
    /// Re-read and compare after writing.
    pub verify: bool,
    /// Upgrade the canonical-size warning to an error (design §2.5).
    pub strict_size: bool,
}

impl Default for EngineOpts {
    fn default() -> Self {
        EngineOpts {
            chunk_size: DEFAULT_CHUNK_SIZE,
            verify: false,
            strict_size: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WriteJob {
    pub bank: String,
    pub unit: u32,
    pub slot_name: Option<String>,
    pub image: PathBuf,
    pub image_len: u64,
    pub extent: Extent,
}

/// One slot to extract, with its length already resolved (design §4) so
/// [`Session::read_slots`] can announce the whole batch's byte total
/// up front.
#[derive(Debug, Clone)]
pub struct ReadJob {
    pub bank_idx: usize,
    pub unit: u32,
    pub length: u64,
    pub out_path: PathBuf,
}

/// Resolve manifest slots + CLI overrides into write jobs (design §5.2 step 1).
/// With no overrides, every manifest slot that names an image is written;
/// with overrides, only the named slots are touched.
pub fn plan_writes(layout: &Layout, overrides: &[SlotAssign]) -> Result<Vec<WriteJob>> {
    let mut jobs = Vec::new();
    let mut add = |bank_idx: usize, unit: u32, image: PathBuf| -> Result<()> {
        let bank = &layout.banks[bank_idx];
        let meta = std::fs::metadata(&image)
            .map_err(|e| val_err(format!("cannot open image {}", image.display()), e))?;
        if !meta.is_file() {
            return Err(Error::Validation(format!(
                "image {} is not a regular file",
                image.display()
            )));
        }
        jobs.push(WriteJob {
            bank: bank.name.clone(),
            unit,
            slot_name: bank.slots.get(&unit).and_then(|s| s.name.clone()),
            image,
            image_len: meta.len(),
            extent: layout.slot_extent(bank_idx, unit),
        });
        Ok(())
    };

    if overrides.is_empty() {
        for (bank_idx, bank) in layout.banks.iter().enumerate() {
            for slot in bank.slots.values() {
                if let Some(image) = &slot.image {
                    add(bank_idx, slot.unit, image.clone())?;
                }
            }
        }
        if jobs.is_empty() {
            return Err(Error::Validation(
                "manifest names no images and no --slot overrides were given".into(),
            ));
        }
    } else {
        for assign in overrides {
            let (bank_idx, unit) = layout.resolve_slot(&assign.slot)?;
            let image = match &assign.image {
                // CLI-supplied paths are relative to the caller's cwd, not
                // the manifest directory.
                Some(p) => p.clone(),
                None => layout.banks[bank_idx]
                    .slots
                    .get(&unit)
                    .and_then(|s| s.image.clone())
                    .ok_or_else(|| {
                        Error::Validation(format!(
                            "slot {} has no image in the manifest; use --slot {}=<image>",
                            assign.slot, assign.slot
                        ))
                    })?,
            };
            add(bank_idx, unit, image)?;
        }
    }

    // Duplicate targets would make the outcome order-dependent.
    for i in 0..jobs.len() {
        for j in i + 1..jobs.len() {
            if jobs[i].bank == jobs[j].bank && jobs[i].unit == jobs[j].unit {
                return Err(Error::Validation(format!(
                    "slot {}:{} is named more than once",
                    jobs[i].bank, jobs[i].unit
                )));
            }
        }
    }
    Ok(jobs)
}

#[derive(Debug, Clone)]
pub struct PlanSummaryItem {
    pub bank: String,
    pub unit: u32,
    pub image: Option<PathBuf>,
    pub offset: u64,
    pub slot_len: u64,
    pub image_len: Option<u64>,
    pub missing: bool,
}

#[derive(Debug, Clone)]
pub struct PlanSummary {
    pub items: Vec<PlanSummaryItem>,
    pub unmapped_count: usize,
}

/// Resolves slots and missing file metadata across a selection for frontends.
pub fn summarize_plan(layout: &Layout, selected: &[(String, u32)]) -> PlanSummary {
    let mut items = Vec::new();
    let mut unmapped_count = 0usize;
    let selected_set: std::collections::HashSet<_> = selected.iter().cloned().collect();

    for bank in &layout.banks {
        for unit in 0..bank.units {
            let key = (bank.name.clone(), unit);
            if !selected_set.is_empty() && !selected_set.contains(&key) {
                continue;
            }
            let slot = bank.slots.get(&unit);
            let image = slot.and_then(|s| s.image.clone());
            match image {
                Some(p) => {
                    let (missing, image_len) = match std::fs::metadata(&p) {
                        Ok(m) => (false, Some(m.len())),
                        Err(_) => (true, None),
                    };
                    items.push(PlanSummaryItem {
                        bank: bank.name.clone(),
                        unit,
                        image: Some(p),
                        offset: bank.slot_offset(unit),
                        slot_len: bank.slot_size,
                        image_len,
                        missing,
                    });
                }
                None => {
                    unmapped_count += 1;
                }
            }
        }
    }
    PlanSummary {
        items,
        unmapped_count,
    }
}

pub fn plan_ops(device: &str, layout: &Layout, jobs: &[WriteJob], op: OpKind) -> Event {
    Event::Plan {
        schema: EVENT_SCHEMA_VERSION,
        device: device.to_string(),
        sector_size: layout.sector_size,
        ops: jobs
            .iter()
            .map(|j| PlanOp {
                op,
                bank: j.bank.clone(),
                unit: j.unit,
                offset: j.extent.offset,
                bytes: if op == OpKind::Wipe {
                    j.extent.len
                } else {
                    j.image_len
                },
                image: Some(j.image.display().to_string()),
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Shared streaming helpers
// ---------------------------------------------------------------------------

fn round_up(n: u64, align: u64) -> u64 {
    n.div_ceil(align) * align
}

fn emit_slot_start(sink: &mut dyn EventSink, op: OpKind, bank: &str, unit: u32, bytes: u64) {
    sink.emit(&Event::SlotStart {
        op,
        bank: bank.to_string(),
        unit,
        bytes,
    });
}

fn emit_slot_end(
    sink: &mut dyn EventSink,
    op: OpKind,
    bank: &str,
    unit: u32,
    ok: bool,
    detail: Option<String>,
) {
    sink.emit(&Event::SlotEnd {
        op,
        bank: bank.to_string(),
        unit,
        ok,
        detail,
    });
}

fn emit_phase(sink: &mut dyn EventSink, op: OpKind, bytes: u64) {
    sink.emit(&Event::PhaseStart { op, bytes });
}

/// SHA-256 of `len` on-device bytes starting at `offset`, plus whether the
/// whole range read as zeros (a wiped/blank slot).
pub fn hash_device_range_detect_zero_with_buf(
    dev: &mut dyn RawDevice,
    offset: u64,
    len: u64,
    chunk_size: usize,
    buf: &mut AlignedBuf,
    mut progress: impl FnMut(u64),
) -> Result<(String, bool)> {
    if buf.len() < chunk_size {
        *buf = AlignedBuf::new(chunk_size);
    }
    let sector = u64::from(dev.sector_size());
    let mut hasher = Sha256::new();
    let mut all_zero = true;
    let mut done: u64 = 0;
    while done < len {
        let want = (len - done).min(chunk_size as u64);
        let aligned = round_up(want, sector) as usize;
        dev.read_at(offset + done, &mut buf[..aligned])?;
        let data = &buf[..want as usize];
        hasher.update(data);
        all_zero = all_zero && data.iter().all(|&b| b == 0);
        done += want;
        progress(done);
    }
    Ok((hex(&hasher.finalize()), all_zero))
}

pub fn hash_device_range_detect_zero(
    dev: &mut dyn RawDevice,
    offset: u64,
    len: u64,
    chunk_size: usize,
    progress: impl FnMut(u64),
) -> Result<(String, bool)> {
    let mut buf = AlignedBuf::new(chunk_size);
    hash_device_range_detect_zero_with_buf(dev, offset, len, chunk_size, &mut buf, progress)
}

/// SHA-256 of `len` on-device bytes starting at `offset`.
pub fn hash_device_range(
    dev: &mut dyn RawDevice,
    offset: u64,
    len: u64,
    chunk_size: usize,
    progress: impl FnMut(u64),
) -> Result<String> {
    hash_device_range_detect_zero(dev, offset, len, chunk_size, progress).map(|(sha, _)| sha)
}

fn file_sha256(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).map_err(|e| val_err(format!("cannot open {}", path.display()), e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| val_err(format!("read error in {}", path.display()), e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

// ---------------------------------------------------------------------------
// Length resolution (extraction, design §4)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LengthMode {
    Bytes(u64),
    Toc,
    Canonical,
    Slot,
}

impl LengthMode {
    pub fn parse(s: &str, sector_size: u32) -> Result<LengthMode> {
        match s.to_ascii_lowercase().as_str() {
            "toc" => Ok(LengthMode::Toc),
            "canonical" => Ok(LengthMode::Canonical),
            "slot" => Ok(LengthMode::Slot),
            _ => Ok(LengthMode::Bytes(crate::units::parse_size(s, sector_size)?)),
        }
    }
}

// ---------------------------------------------------------------------------
// Status reporting types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SlotStatusReport {
    pub bank: String,
    pub unit: u32,
    pub state: SlotState,
    pub name: Option<String>,
    pub length: Option<u64>,
    pub sha256: Option<String>,
}

/// How one slot will be probed by `status`.
enum StatusProbe {
    /// Hash `length` bytes and compare against the TOC record.
    Toc(TocEntry),
    /// Hash the image's extent and compare against the file's hash.
    Image {
        path: PathBuf,
        file_len: u64,
        hash_len: u64,
    },
    /// Nothing to compare against.
    Unknown,
}

/// Hash one slot's content, announcing it and streaming progress with a
/// uniform SlotStart/Progress/SlotEnd envelope, then classify the result:
/// matching `expect` → `Matches`, all-zero → `Wiped`, else `mismatch_state`.
#[allow(clippy::too_many_arguments)]
fn probe_slot(
    dev: &mut dyn RawDevice,
    opts: &EngineOpts,
    sink: &mut dyn EventSink,
    buf: &mut AlignedBuf,
    bank: &str,
    unit: u32,
    offset: u64,
    length: u64,
    expect: &str,
    mismatch_state: SlotState,
) -> Result<(SlotState, String)> {
    emit_slot_start(sink, OpKind::Status, bank, unit, length);
    let (actual, zeroed) = hash_device_range_detect_zero_with_buf(
        dev,
        offset,
        length,
        opts.chunk_size,
        buf,
        |done| {
            sink.emit(&Event::Progress {
                bank: bank.to_string(),
                unit,
                bytes_done: done,
                bytes_total: length,
            })
        },
    )?;
    emit_slot_end(sink, OpKind::Status, bank, unit, true, None);
    let state = if actual == expect {
        SlotState::Matches
    } else if zeroed {
        SlotState::Wiped
    } else {
        mismatch_state
    };
    Ok((state, actual))
}

fn emit_status(sink: &mut dyn EventSink, report: &SlotStatusReport) {
    sink.emit(&Event::SlotStatus {
        bank: report.bank.clone(),
        unit: report.unit,
        state: report.state,
        name: report.name.clone(),
        length: report.length,
        sha256: report.sha256.clone(),
    });
}

// ---------------------------------------------------------------------------
// Session: an open device + layout, with the TOC read at most once
// ---------------------------------------------------------------------------

pub struct Session<'a> {
    dev: &'a mut dyn RawDevice,
    layout: &'a Layout,
    /// Outer None = not read yet; inner None = no TOC configured/found.
    toc_cache: Option<Option<Toc>>,
    /// Reusable buffer for streaming I/O and hashing across multi-slot ops.
    buf: AlignedBuf,
}

impl<'a> Session<'a> {
    /// Pair a device with a layout. The sector-size match is enforced here,
    /// so every operation (not just writes) gets the check.
    pub fn new(dev: &'a mut dyn RawDevice, layout: &'a Layout) -> Result<Session<'a>> {
        if dev.sector_size() != layout.sector_size {
            return Err(Error::Validation(format!(
                "device sector size {} does not match manifest sector_size {}",
                dev.sector_size(),
                layout.sector_size
            )));
        }
        Ok(Session {
            dev,
            layout,
            toc_cache: None,
            buf: AlignedBuf::new(DEFAULT_CHUNK_SIZE),
        })
    }

    fn ensure_buf(&mut self, chunk_size: usize) {
        if self.buf.len() < chunk_size {
            self.buf = AlignedBuf::new(chunk_size);
        }
    }

    fn toc(&mut self) -> Result<Option<&Toc>> {
        if self.toc_cache.is_none() {
            self.toc_cache = Some(match self.layout.toc_offset {
                Some(off) => toc::read_toc(self.dev, off)?,
                None => None,
            });
        }
        Ok(self.toc_cache.as_ref().unwrap().as_ref())
    }

    /// The card's TOC if its layout hash matches this manifest, else a
    /// fresh one.
    fn toc_for_update(&mut self) -> Result<Toc> {
        let hash = self.layout.layout_hash();
        Ok(match self.toc()? {
            Some(t) if t.layout_hash == hash => t.clone(),
            _ => Toc::new(hash),
        })
    }

    fn write_toc(&mut self, card_toc: Toc) -> Result<()> {
        if let Some(off) = self.layout.toc_offset {
            toc::write_toc(self.dev, off, &card_toc)?;
            self.toc_cache = Some(Some(card_toc));
        }
        Ok(())
    }

    /// Validate jobs against the layout, device, and drive-type registry
    /// (design §5.2 step 2). Returns non-fatal warnings.
    pub fn validate_writes(&self, jobs: &[WriteJob], opts: &EngineOpts) -> Result<Vec<String>> {
        let mut warnings = Vec::new();
        let capacity = self.dev.capacity_bytes();
        for job in jobs {
            if job.image_len > job.extent.len {
                return Err(Error::Validation(format!(
                    "image {} ({} bytes) exceeds the {} slot {}:{}",
                    job.image.display(),
                    job.image_len,
                    format_bytes(job.extent.len),
                    job.bank,
                    job.unit
                )));
            }
            if !self.dev.growable() && job.extent.end() > capacity {
                return Err(Error::Validation(format!(
                    "slot {}:{} (bytes 0x{:x}..0x{:x}) extends past the device capacity ({})",
                    job.bank,
                    job.unit,
                    job.extent.offset,
                    job.extent.end(),
                    format_bytes(capacity)
                )));
            }
            let bank = self.layout.bank(&job.bank).expect("job bank exists");
            if let Some(dt) = &bank.drive_type {
                if job.image_len != dt.image_bytes {
                    let msg = format!(
                        "image {} is {} bytes but the canonical {} image is {} bytes",
                        job.image.display(),
                        job.image_len,
                        dt.name,
                        dt.image_bytes
                    );
                    if opts.strict_size {
                        return Err(Error::Validation(format!("{msg} (--strict-size)")));
                    }
                    warnings.push(msg);
                }
            }
        }
        if let Some(t) = self.layout.toc_extent() {
            if !self.dev.growable() && t.end() > capacity {
                return Err(Error::Validation(format!(
                    "TOC region (bytes 0x{:x}..0x{:x}) extends past the device capacity",
                    t.offset,
                    t.end()
                )));
            }
        }
        Ok(warnings)
    }

    /// Write all jobs (design §5.2 steps 4–6). The plan/confirmation step
    /// (3) is the frontend's responsibility. On success every image is on
    /// the device, the TOC (if configured) is updated, and the device is
    /// flushed.
    pub fn write_slots(
        &mut self,
        jobs: &[WriteJob],
        opts: &EngineOpts,
        sink: &mut dyn EventSink,
    ) -> Result<()> {
        emit_phase(sink, OpKind::Write, jobs.iter().map(|j| j.image_len).sum());
        let mut hashes = Vec::with_capacity(jobs.len());
        for job in jobs {
            let sha = self.stream_image(job, opts.chunk_size, sink)?;
            hashes.push(sha.clone());
            emit_slot_end(sink, OpKind::Write, &job.bank, job.unit, true, Some(sha));
        }

        let mut card_toc = self.toc_for_update()?;
        for (job, sha) in jobs.iter().zip(&hashes) {
            card_toc.upsert(TocEntry {
                bank: job.bank.clone(),
                unit: job.unit,
                offset: job.extent.offset,
                name: job.slot_name.clone(),
                length: job.image_len,
                sha256: sha.clone(),
                written_unix: toc::now_unix(),
            });
        }
        self.write_toc(card_toc)?;
        self.dev.flush()?;

        if opts.verify {
            self.ensure_buf(opts.chunk_size);
            emit_phase(sink, OpKind::Verify, jobs.iter().map(|j| j.image_len).sum());
            for (job, expect) in jobs.iter().zip(&hashes) {
                emit_slot_start(sink, OpKind::Verify, &job.bank, job.unit, job.image_len);
                let (actual, _) = hash_device_range_detect_zero_with_buf(
                    self.dev,
                    job.extent.offset,
                    job.image_len,
                    opts.chunk_size,
                    &mut self.buf,
                    |done| {
                        sink.emit(&Event::Progress {
                            bank: job.bank.clone(),
                            unit: job.unit,
                            bytes_done: done,
                            bytes_total: job.image_len,
                        })
                    },
                )?;
                let ok = actual == *expect;
                emit_slot_end(
                    sink,
                    OpKind::Verify,
                    &job.bank,
                    job.unit,
                    ok,
                    (!ok).then(|| format!("expected {expect}, read back {actual}")),
                );
                if !ok {
                    return Err(Error::VerifyMismatch(format!(
                        "slot {}:{} read back with SHA-256 {actual}, expected {expect}",
                        job.bank, job.unit
                    )));
                }
            }
        }
        Ok(())
    }

    /// Stream one image into its slot; returns the SHA-256 of the image
    /// bytes.
    fn stream_image(
        &mut self,
        job: &WriteJob,
        chunk_size: usize,
        sink: &mut dyn EventSink,
    ) -> Result<String> {
        self.ensure_buf(chunk_size);
        let sector = u64::from(self.dev.sector_size());
        let mut file = File::open(&job.image)
            .map_err(|e| val_err(format!("cannot open image {}", job.image.display()), e))?;
        let mut hasher = Sha256::new();
        let mut done: u64 = 0;

        emit_slot_start(sink, OpKind::Write, &job.bank, job.unit, job.image_len);

        while done < job.image_len {
            let want = ((job.image_len - done) as usize).min(chunk_size);
            let mut filled = 0;
            while filled < want {
                let n = file
                    .read(&mut self.buf[filled..want])
                    .map_err(|e| val_err(format!("read error in {}", job.image.display()), e))?;
                if n == 0 {
                    return Err(Error::Validation(format!(
                        "image {} shrank while being written (read {} of {} bytes)",
                        job.image.display(),
                        done + filled as u64,
                        job.image_len
                    )));
                }
                filled += n;
            }
            hasher.update(&self.buf[..filled]);
            // Final chunk: zero-pad to a sector boundary (design §5.2 step 4).
            let padded = round_up(filled as u64, sector) as usize;
            self.buf[filled..padded].fill(0);
            self.dev
                .write_at(job.extent.offset + done, &self.buf[..padded])
                .map_err(|e| {
                    Error::Device(format!(
                        "{e}; suspect byte range 0x{:x}..0x{:x} of slot {}:{}",
                        job.extent.offset + done,
                        job.extent.offset + done + padded as u64,
                        job.bank,
                        job.unit
                    ))
                })?;
            done += filled as u64;
            sink.emit(&Event::Progress {
                bank: job.bank.clone(),
                unit: job.unit,
                bytes_done: done,
                bytes_total: job.image_len,
            });
        }

        Ok(hex(&hasher.finalize()))
    }

    /// Resolve the extraction length for a slot (design §4 resolution
    /// order). Uses the cached TOC, so multi-slot reads hit the card once.
    pub fn resolve_length(
        &mut self,
        bank_idx: usize,
        unit: u32,
        mode: Option<LengthMode>,
    ) -> Result<u64> {
        let bank = &self.layout.banks[bank_idx];
        let bank_name = bank.name.clone();
        let drive_type = bank.drive_type.clone();
        let extent = self.layout.slot_extent(bank_idx, unit);
        let toc_len = |s: &mut Self| -> Result<Option<u64>> {
            Ok(s.toc()?
                .and_then(|t| t.find(&bank_name, unit).map(|e| e.length)))
        };
        let len = match mode {
            Some(LengthMode::Bytes(n)) => n,
            Some(LengthMode::Toc) => toc_len(self)?.ok_or_else(|| {
                Error::Validation(format!(
                    "--length toc: no TOC record for slot {bank_name}:{unit}"
                ))
            })?,
            Some(LengthMode::Canonical) => match &drive_type {
                Some(dt) => dt.image_bytes,
                None => {
                    return Err(Error::Validation(format!(
                        "--length canonical: bank {bank_name:?} declares no drive_type"
                    )))
                }
            },
            Some(LengthMode::Slot) => extent.len,
            // Default: canonical when the bank has a drive_type, else the
            // TOC record if one exists, else the full slot.
            None => match &drive_type {
                Some(dt) => dt.image_bytes,
                None => toc_len(self)?.unwrap_or(extent.len),
            },
        };
        if len > extent.len {
            return Err(Error::Validation(format!(
                "requested length {len} exceeds the {} slot",
                format_bytes(extent.len)
            )));
        }
        Ok(len)
    }

    /// Extract a slot into a file.
    pub fn read_slot(
        &mut self,
        bank_idx: usize,
        unit: u32,
        length: u64,
        out_path: &Path,
        opts: &EngineOpts,
        sink: &mut dyn EventSink,
    ) -> Result<()> {
        use std::io::Write;
        self.ensure_buf(opts.chunk_size);
        let bank_name = self.layout.banks[bank_idx].name.clone();
        let extent = self.layout.slot_extent(bank_idx, unit);
        let sector = u64::from(self.dev.sector_size());
        let mut out = File::create(out_path)
            .map_err(|e| val_err(format!("cannot create {}", out_path.display()), e))?;

        emit_slot_start(sink, OpKind::Read, &bank_name, unit, length);
        let mut done: u64 = 0;
        while done < length {
            let want = (length - done).min(opts.chunk_size as u64);
            let aligned = round_up(want, sector) as usize;
            self.dev
                .read_at(extent.offset + done, &mut self.buf[..aligned])?;
            out.write_all(&self.buf[..want as usize])
                .map_err(|e| val_err(format!("write error in {}", out_path.display()), e))?;
            done += want;
            sink.emit(&Event::Progress {
                bank: bank_name.clone(),
                unit,
                bytes_done: done,
                bytes_total: length,
            });
        }
        out.sync_all()
            .map_err(|e| val_err(format!("cannot sync {}", out_path.display()), e))?;
        emit_slot_end(sink, OpKind::Read, &bank_name, unit, true, None);
        Ok(())
    }

    /// Extract several slots, announcing the batch's combined byte total
    /// up front (a `PhaseStart`) so frontends can show aggregate progress
    /// across the whole extraction instead of restarting per slot.
    pub fn read_slots(
        &mut self,
        jobs: &[ReadJob],
        opts: &EngineOpts,
        sink: &mut dyn EventSink,
    ) -> Result<()> {
        emit_phase(sink, OpKind::Read, jobs.iter().map(|j| j.length).sum());
        for job in jobs {
            self.read_slot(
                job.bank_idx,
                job.unit,
                job.length,
                &job.out_path,
                opts,
                sink,
            )?;
        }
        Ok(())
    }

    /// Zero each slot's full extent (present a "blank pack" to the FPGA)
    /// and drop its TOC record.
    pub fn wipe_slots(
        &mut self,
        slots: &[(usize, u32)],
        opts: &EngineOpts,
        sink: &mut dyn EventSink,
    ) -> Result<()> {
        let total = slots
            .iter()
            .map(|&(b, u)| self.layout.slot_extent(b, u).len)
            .sum();
        emit_phase(sink, OpKind::Wipe, total);
        self.ensure_buf(opts.chunk_size);
        self.buf.zero();
        for &(bank_idx, unit) in slots {
            let bank_name = self.layout.banks[bank_idx].name.clone();
            let extent = self.layout.slot_extent(bank_idx, unit);
            emit_slot_start(sink, OpKind::Wipe, &bank_name, unit, extent.len);
            let mut done: u64 = 0;
            while done < extent.len {
                let n = (extent.len - done).min(opts.chunk_size as u64) as usize;
                self.dev.write_at(extent.offset + done, &self.buf[..n])?;
                done += n as u64;
                sink.emit(&Event::Progress {
                    bank: bank_name.clone(),
                    unit,
                    bytes_done: done,
                    bytes_total: extent.len,
                });
            }
            emit_slot_end(sink, OpKind::Wipe, &bank_name, unit, true, None);
        }
        let mut card_toc = self.toc_for_update()?;
        for &(bank_idx, unit) in slots {
            card_toc.remove(&self.layout.banks[bank_idx].name, unit);
        }
        self.write_toc(card_toc)?;
        self.dev.flush()
    }

    /// Re-read slots and compare against manifest images. Returns the list
    /// of mismatching `bank:unit` pairs (empty = all good).
    pub fn verify_slots(
        &mut self,
        jobs: &[WriteJob],
        opts: &EngineOpts,
        sink: &mut dyn EventSink,
    ) -> Result<Vec<String>> {
        self.ensure_buf(opts.chunk_size);
        emit_phase(sink, OpKind::Verify, jobs.iter().map(|j| j.image_len).sum());
        let mut mismatches = Vec::new();
        for job in jobs {
            let expect = file_sha256(&job.image)?;
            emit_slot_start(sink, OpKind::Verify, &job.bank, job.unit, job.image_len);
            let (actual, _) = hash_device_range_detect_zero_with_buf(
                self.dev,
                job.extent.offset,
                job.image_len,
                opts.chunk_size,
                &mut self.buf,
                |done| {
                    sink.emit(&Event::Progress {
                        bank: job.bank.clone(),
                        unit: job.unit,
                        bytes_done: done,
                        bytes_total: job.image_len,
                    })
                },
            )?;
            let ok = actual == expect;
            emit_slot_end(
                sink,
                OpKind::Verify,
                &job.bank,
                job.unit,
                ok,
                (!ok).then(|| format!("expected {expect}, read {actual}")),
            );
            if !ok {
                mismatches.push(format!("{}:{}", job.bank, job.unit));
            }
        }
        Ok(mismatches)
    }

    /// Determine the state of every known slot: TOC-based when a TOC is
    /// present, manifest hash probe otherwise. Emits a `PhaseStart` with
    /// the scan's total byte count for aggregate progress.
    pub fn status(
        &mut self,
        opts: &EngineOpts,
        sink: &mut dyn EventSink,
    ) -> Result<Vec<SlotStatusReport>> {
        self.ensure_buf(opts.chunk_size);
        // Pass 1: decide how each slot will be probed and how many bytes
        // that hashes, without touching slot data yet.
        let mut items: Vec<(usize, u32, StatusProbe)> = Vec::new();
        for (bank_idx, bank) in self.layout.banks.iter().enumerate() {
            for unit in 0..bank.units {
                items.push((bank_idx, unit, StatusProbe::Unknown));
            }
        }
        for (bank_idx, unit, probe) in &mut items {
            let bank = &self.layout.banks[*bank_idx];
            let bank_name = bank.name.clone();
            let slot_size = bank.slot_size;
            let image = bank.slots.get(unit).and_then(|s| s.image.clone());
            let toc_entry = self.toc()?.and_then(|t| t.find(&bank_name, *unit)).cloned();
            *probe = if let Some(entry) = toc_entry {
                StatusProbe::Toc(entry)
            } else if let Some(image) = image {
                match std::fs::metadata(&image) {
                    Ok(meta) => StatusProbe::Image {
                        hash_len: meta.len().min(slot_size),
                        file_len: meta.len(),
                        path: image,
                    },
                    Err(_) => StatusProbe::Unknown,
                }
            } else {
                StatusProbe::Unknown
            };
        }

        let total: u64 = items
            .iter()
            .map(|(_, _, p)| match p {
                StatusProbe::Toc(e) => e.length,
                StatusProbe::Image { hash_len, .. } => *hash_len,
                StatusProbe::Unknown => 0,
            })
            .sum();
        emit_phase(sink, OpKind::Status, total);

        // Pass 2: hash and report.
        let mut reports = Vec::new();
        for (bank_idx, unit, probe) in items {
            let bank = &self.layout.banks[bank_idx];
            let bank_name = bank.name.clone();
            let manifest_name = bank.slots.get(&unit).and_then(|s| s.name.clone());
            let extent = self.layout.slot_extent(bank_idx, unit);
            let report = match probe {
                StatusProbe::Toc(entry) => {
                    let (state, actual) = probe_slot(
                        self.dev,
                        opts,
                        sink,
                        &mut self.buf,
                        &bank_name,
                        unit,
                        extent.offset,
                        entry.length,
                        &entry.sha256,
                        SlotState::Modified,
                    )?;
                    SlotStatusReport {
                        bank: bank_name,
                        unit,
                        state,
                        name: entry.name.clone(),
                        length: Some(entry.length),
                        sha256: Some(actual),
                    }
                }
                StatusProbe::Image {
                    path,
                    file_len,
                    hash_len,
                } => {
                    let expect = file_sha256(&path)?;
                    let (state, actual) = probe_slot(
                        self.dev,
                        opts,
                        sink,
                        &mut self.buf,
                        &bank_name,
                        unit,
                        extent.offset,
                        hash_len,
                        &expect,
                        SlotState::Differs,
                    )?;
                    SlotStatusReport {
                        bank: bank_name,
                        unit,
                        state,
                        name: manifest_name,
                        length: Some(file_len),
                        sha256: Some(actual),
                    }
                }
                StatusProbe::Unknown => SlotStatusReport {
                    bank: bank_name,
                    unit,
                    state: SlotState::Unknown,
                    name: manifest_name,
                    length: None,
                    sha256: None,
                },
            };
            emit_status(sink, &report);
            reports.push(report);
        }
        Ok(reports)
    }

    /// Ensure a file-backed "card" spans the full layout, so the resulting
    /// flat image can be written to a real card with dd or balenaEtcher.
    /// No-op on real devices.
    pub fn extend_to_full_layout(&mut self) -> Result<()> {
        self.dev.ensure_len(self.layout.max_extent_end())
    }
}

/// Describe a card using only its on-card TOC — no manifest required. The
/// TOC records each slot's offset, so slots can be located and re-hashed.
/// Returns `Ok(None)` when no TOC is found on the card.
pub fn status_from_card(
    dev: &mut dyn RawDevice,
    opts: &EngineOpts,
    sink: &mut dyn EventSink,
) -> Result<Option<Vec<SlotStatusReport>>> {
    let Some((_, card_toc)) = toc::probe_toc(dev)? else {
        return Ok(None);
    };
    emit_phase(
        sink,
        OpKind::Status,
        card_toc.entries.iter().map(|e| e.length).sum(),
    );
    let mut buf = AlignedBuf::new(opts.chunk_size);
    let mut reports = Vec::new();
    for entry in &card_toc.entries {
        let (state, actual) = probe_slot(
            dev,
            opts,
            sink,
            &mut buf,
            &entry.bank,
            entry.unit,
            entry.offset,
            entry.length,
            &entry.sha256,
            SlotState::Modified,
        )?;
        let report = SlotStatusReport {
            bank: entry.bank.clone(),
            unit: entry.unit,
            state,
            name: entry.name.clone(),
            length: Some(entry.length),
            sha256: Some(actual),
        };
        emit_status(sink, &report);
        reports.push(report);
    }
    Ok(Some(reports))
}

/// Filename for a batch-extracted slot image (`read --out-dir`): the slot
/// coordinates plus a sanitized slot name, e.g. `rl0_xxdp-rl02.img`. Shared
/// by the CLI and GUI so both predict the same names.
pub fn extract_filename(bank: &str, unit: u32, name: Option<&str>) -> String {
    let mut base = format!("{bank}{unit}");
    if let Some(n) = name {
        let safe: String = n
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        if !safe.is_empty() {
            base.push('_');
            base.push_str(&safe);
        }
    }
    format!("{base}.img")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_buf_is_4k_aligned_and_zeroed() {
        // Windows NO_BUFFERING and Linux O_DIRECT both reject misaligned
        // buffers at runtime; this invariant is load-bearing.
        for len in [1, 511, 512, 4096, DEFAULT_CHUNK_SIZE] {
            let mut buf = AlignedBuf::new(len);
            assert_eq!(buf.as_ptr() as usize % BUFFER_ALIGN, 0, "len {len}");
            assert_eq!(buf.len(), len);
            assert!(buf.iter().all(|&b| b == 0), "len {len} not zeroed");
            buf[0] = 0xa5;
            buf.zero();
            assert_eq!(buf[0], 0);
        }
    }

    #[test]
    fn aligned_buf_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<AlignedBuf>();
    }

    #[test]
    fn round_up_to_sector() {
        assert_eq!(round_up(0, 512), 0);
        assert_eq!(round_up(1, 512), 512);
        assert_eq!(round_up(512, 512), 512);
        assert_eq!(round_up(700_000, 512), 700_416);
    }

    #[test]
    fn extract_filenames_are_safe_and_stable() {
        assert_eq!(extract_filename("rl", 0, None), "rl0.img");
        assert_eq!(
            extract_filename("rl", 0, Some("xxdp-rl02")),
            "rl0_xxdp-rl02.img"
        );
        assert_eq!(
            extract_filename("rp", 7, Some("2.11 BSD!")),
            "rp7_2_11_BSD_.img"
        );
    }

    #[test]
    fn length_mode_parses() {
        assert_eq!(LengthMode::parse("toc", 512).unwrap(), LengthMode::Toc);
        assert_eq!(
            LengthMode::parse("CANONICAL", 512).unwrap(),
            LengthMode::Canonical
        );
        assert_eq!(LengthMode::parse("slot", 512).unwrap(), LengthMode::Slot);
        assert_eq!(
            LengthMode::parse("10MiB", 512).unwrap(),
            LengthMode::Bytes(10 << 20)
        );
        assert!(LengthMode::parse("bogus", 512).is_err());
    }

    #[test]
    fn test_engine_uncovered_branches() {
        use crate::layout::{Layout, SlotRef};

        let manifest = r#"
sector_size = 512
[[bank]]
name = "rl"
base = "0"
slot_size = "16MiB"
units = 2

  [[bank.slot]]
  unit = 0
  name = "test-slot"
"#;
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::from_toml(manifest, dir.path()).unwrap();

        let plan = summarize_plan(&layout, &[]);
        assert_eq!(plan.unmapped_count, 2);
        assert_eq!(plan.items.len(), 0);

        let plan_sel = summarize_plan(&layout, &[("rl".to_string(), 0)]);
        assert_eq!(plan_sel.items.len(), 0);

        let err1 = plan_writes(&layout, &[]).unwrap_err();
        assert!(err1.to_string().contains("manifest names no images"));

        let assign_no_img = SlotAssign::parse("rl:0").unwrap();
        let err2 = plan_writes(&layout, &[assign_no_img]).unwrap_err();
        assert!(err2.to_string().contains("has no image in the manifest"));

        let dir_path = dir.path().to_path_buf();
        let assign_dir = SlotAssign {
            slot: SlotRef::parse("rl:0").unwrap(),
            image: Some(dir_path),
        };
        let err3 = plan_writes(&layout, &[assign_dir]).unwrap_err();
        assert!(err3.to_string().contains("is not a regular file"));

        let jobs = vec![WriteJob {
            bank: "rl".to_string(),
            unit: 0,
            slot_name: Some("test".to_string()),
            image: std::path::PathBuf::from("test.img"),
            image_len: 100,
            extent: crate::layout::Extent {
                offset: 0,
                len: 1000,
            },
        }];
        let ev_wipe = plan_ops("dev", &layout, &jobs, OpKind::Wipe);
        if let Event::Plan { ops, .. } = ev_wipe {
            assert_eq!(ops[0].bytes, 1000);
        } else {
            panic!("expected plan event");
        }
    }
}
