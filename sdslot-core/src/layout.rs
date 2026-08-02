// SPDX-License-Identifier: MIT OR Apache-2.0
//! Card layout model (design §2): TOML manifest parsing, bank/slot
//! resolution, drive-type registry extension, and overlap/bounds validation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::drive_types::{DriveType, Geometry, Registry};
use crate::error::{Error, Result};
use crate::toc::TOC_REGION_LEN;
use crate::units::{format_bytes, parse_size};

pub const DEFAULT_SECTOR_SIZE: u32 = 512;

// ---------------------------------------------------------------------------
// Raw (serde) manifest shape
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    sector_size: Option<u32>,
    /// Byte offset (size expression) of the optional on-card TOC region (§2.6).
    toc: Option<String>,
    /// Permit non-power-of-two bases/slot sizes (§2.1).
    allow_unaligned: Option<bool>,
    #[serde(default)]
    bank: Vec<RawBank>,
    #[serde(default)]
    drive_types: BTreeMap<String, RawDriveType>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBank {
    name: String,
    base: String,
    slot_size: String,
    units: u32,
    drive_type: Option<String>,
    #[serde(default)]
    slot: Vec<RawSlot>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSlot {
    unit: u32,
    name: Option<String>,
    image: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDriveType {
    /// Canonical image size; may be omitted when geometry is given.
    image_size: Option<String>,
    recommended_slot: Option<String>,
    cylinders: Option<u32>,
    heads: Option<u32>,
    sectors: Option<u32>,
    bytes_per_sector: Option<u32>,
    /// Byte-stream media (magtape): variable-length images, no geometry.
    stream: Option<bool>,
}

// ---------------------------------------------------------------------------
// Resolved layout
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SlotEntry {
    pub unit: u32,
    pub name: Option<String>,
    /// Absolute path (manifest-relative paths are resolved at load time).
    pub image: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Bank {
    pub name: String,
    pub base: u64,
    pub slot_size: u64,
    pub units: u32,
    pub drive_type: Option<DriveType>,
    pub slots: BTreeMap<u32, SlotEntry>,
}

impl Bank {
    pub fn span(&self) -> u64 {
        self.slot_size * u64::from(self.units)
    }

    pub fn slot_offset(&self, unit: u32) -> u64 {
        self.base + u64::from(unit) * self.slot_size
    }
}

/// A byte range on the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub offset: u64,
    pub len: u64,
}

impl Extent {
    pub fn end(&self) -> u64 {
        self.offset + self.len
    }
}

#[derive(Debug, Clone)]
pub struct Layout {
    pub sector_size: u32,
    pub allow_unaligned: bool,
    /// Byte offset of the on-card TOC region, if configured.
    pub toc_offset: Option<u64>,
    pub banks: Vec<Bank>,
    pub registry: Registry,
    /// Directory of the manifest file; image paths resolve against it.
    pub base_dir: PathBuf,
}

/// `bank:unit` slot reference; the bank may be omitted with a single bank.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotRef {
    pub bank: Option<String>,
    pub unit: u32,
}

impl SlotRef {
    /// Parse "rl:1" or "1".
    ///
    /// ```
    /// use sdslot_core::layout::SlotRef;
    ///
    /// let r = SlotRef::parse("rl:1")?;
    /// assert_eq!(r.bank.as_deref(), Some("rl"));
    /// assert_eq!(r.unit, 1);
    ///
    /// // The bank prefix may be omitted when the manifest has a single bank.
    /// assert_eq!(SlotRef::parse("7")?.bank, None);
    /// assert!(SlotRef::parse("rl:").is_err());
    /// # Ok::<(), sdslot_core::Error>(())
    /// ```
    pub fn parse(s: &str) -> Result<SlotRef> {
        let bad = || Error::Validation(format!("bad slot reference {s:?}: expected [bank:]unit"));
        match s.rsplit_once(':') {
            Some((bank, unit)) => {
                if bank.is_empty() {
                    return Err(bad());
                }
                Ok(SlotRef {
                    bank: Some(bank.to_string()),
                    unit: unit.parse().map_err(|_| bad())?,
                })
            }
            None => Ok(SlotRef {
                bank: None,
                unit: s.parse().map_err(|_| bad())?,
            }),
        }
    }
}

impl std::fmt::Display for SlotRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.bank {
            Some(b) => write!(f, "{b}:{}", self.unit),
            None => write!(f, "{}", self.unit),
        }
    }
}

/// `bank:unit[=image]` as accepted by `--slot`.
#[derive(Debug, Clone)]
pub struct SlotAssign {
    pub slot: SlotRef,
    pub image: Option<PathBuf>,
}

impl SlotAssign {
    pub fn parse(s: &str) -> Result<SlotAssign> {
        match s.split_once('=') {
            Some((slot, image)) => {
                if image.is_empty() {
                    return Err(Error::Validation(format!(
                        "bad slot assignment {s:?}: empty image path"
                    )));
                }
                Ok(SlotAssign {
                    slot: SlotRef::parse(slot)?,
                    image: Some(PathBuf::from(image)),
                })
            }
            None => Ok(SlotAssign {
                slot: SlotRef::parse(s)?,
                image: None,
            }),
        }
    }
}

impl Layout {
    pub fn load(path: &Path) -> Result<Layout> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            Error::Validation(format!("cannot read manifest {}: {e}", path.display()))
        })?;
        let base_dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        Layout::from_toml(&text, &base_dir)
    }

    /// Parse and validate a manifest from TOML text; relative image paths in
    /// it resolve against `base_dir`.
    ///
    /// ```
    /// use sdslot_core::layout::Layout;
    /// use std::path::Path;
    ///
    /// // A bank's base must be aligned to its power-of-two span unless the
    /// // manifest sets `allow_unaligned`.
    /// let err = Layout::from_toml(
    ///     r#"
    ///     [[bank]]
    ///     name      = "rp"
    ///     base      = "1MiB"
    ///     slot_size = "256MiB"
    ///     units     = 8
    ///     "#,
    ///     Path::new("."),
    /// )
    /// .expect_err("1 MiB is not a 2 GiB boundary");
    /// assert!(err.to_string().contains("align"));
    /// ```
    pub fn from_toml(text: &str, base_dir: &Path) -> Result<Layout> {
        let raw: RawManifest = toml::from_str(text)
            .map_err(|e| Error::Validation(format!("manifest parse error: {e}")))?;
        resolve(raw, base_dir)
    }

    pub fn bank(&self, name: &str) -> Option<&Bank> {
        self.banks.iter().find(|b| b.name == name)
    }

    /// Resolve a `SlotRef` to (bank index, unit), validating bounds.
    pub fn resolve_slot(&self, r: &SlotRef) -> Result<(usize, u32)> {
        let idx = match &r.bank {
            Some(name) => self
                .banks
                .iter()
                .position(|b| &b.name == name)
                .ok_or_else(|| Error::Validation(format!("no bank named {name:?}")))?,
            None => {
                if self.banks.len() == 1 {
                    0
                } else {
                    return Err(Error::Validation(format!(
                        "slot reference {r} needs a bank prefix: manifest has {} banks",
                        self.banks.len()
                    )));
                }
            }
        };
        let bank = &self.banks[idx];
        if r.unit >= bank.units {
            return Err(Error::Validation(format!(
                "unit {} out of range for bank {:?} ({} units)",
                r.unit, bank.name, bank.units
            )));
        }
        Ok((idx, r.unit))
    }

    pub fn slot_extent(&self, bank_idx: usize, unit: u32) -> Extent {
        let b = &self.banks[bank_idx];
        Extent {
            offset: b.slot_offset(unit),
            len: b.slot_size,
        }
    }

    /// TOC region extent, if configured.
    pub fn toc_extent(&self) -> Option<Extent> {
        self.toc_offset.map(|offset| Extent {
            offset,
            len: TOC_REGION_LEN,
        })
    }

    /// Stable hash of the geometry-defining parts of the layout. Recorded in
    /// the TOC so a card written with a different layout is detected.
    pub fn layout_hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(format!("sector={}\n", self.sector_size));
        for b in &self.banks {
            h.update(format!(
                "bank={} base={} slot_size={} units={}\n",
                b.name, b.base, b.slot_size, b.units
            ));
        }
        hex(&h.finalize())
    }

    /// Total extent that any configured bank or TOC region may touch.
    pub fn max_extent_end(&self) -> u64 {
        let banks = self
            .banks
            .iter()
            .map(|b| b.base + b.span())
            .max()
            .unwrap_or(0);
        match self.toc_extent() {
            Some(t) => banks.max(t.end()),
            None => banks,
        }
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Resolution + validation
// ---------------------------------------------------------------------------

fn resolve(raw: RawManifest, base_dir: &Path) -> Result<Layout> {
    let sector_size = raw.sector_size.unwrap_or(DEFAULT_SECTOR_SIZE);
    if !sector_size.is_power_of_two() || sector_size < 256 {
        return Err(Error::Validation(format!(
            "sector_size {sector_size} must be a power of two >= 256"
        )));
    }
    let allow_unaligned = raw.allow_unaligned.unwrap_or(false);

    let mut registry = Registry::builtin();
    for (name, rt) in &raw.drive_types {
        let stream = rt.stream.unwrap_or(false);
        let geometry = match (rt.cylinders, rt.heads, rt.sectors, rt.bytes_per_sector) {
            (Some(c), Some(h), Some(s), Some(b)) => Some(Geometry {
                cylinders: c,
                heads: h,
                sectors: s,
                bytes_per_sector: b,
            }),
            (None, None, None, None) => None,
            _ => {
                return Err(Error::Validation(format!(
                "drive type {name:?}: give all of cylinders/heads/sectors/bytes_per_sector or none"
            )))
            }
        };
        if stream && geometry.is_some() {
            return Err(Error::Validation(format!(
                "drive type {name:?}: stream media has no C/H/S geometry"
            )));
        }
        let image_bytes = match (&rt.image_size, geometry) {
            (Some(sz), _) => parse_size(sz, sector_size)?,
            (None, Some(g)) => g.bytes(),
            // A stream type's image_size is only a nominal capacity; default
            // it to the recommended slot when only that is given.
            (None, None) if stream => match &rt.recommended_slot {
                Some(sz) => parse_size(sz, sector_size)?,
                None => {
                    return Err(Error::Validation(format!(
                        "stream drive type {name:?} needs image_size or recommended_slot"
                    )))
                }
            },
            (None, None) => {
                return Err(Error::Validation(format!(
                    "drive type {name:?} needs image_size or a full geometry"
                )))
            }
        };
        let recommended_slot = match &rt.recommended_slot {
            Some(sz) => parse_size(sz, sector_size)?,
            None => image_bytes.next_power_of_two(),
        };
        if image_bytes > recommended_slot {
            return Err(Error::Validation(format!(
                "drive type {name:?}: image_size {} exceeds recommended_slot {}",
                format_bytes(image_bytes),
                format_bytes(recommended_slot)
            )));
        }
        registry.add(DriveType {
            name: name.clone(),
            geometry,
            image_bytes,
            recommended_slot,
            stream,
        });
    }

    if raw.bank.is_empty() {
        return Err(Error::Validation("manifest defines no banks".into()));
    }

    let mut banks = Vec::new();
    for rb in raw.bank {
        let base = parse_size(&rb.base, sector_size)?;
        let slot_size = parse_size(&rb.slot_size, sector_size)?;
        let ctx = format!("bank {:?}", rb.name);
        if rb.units == 0 {
            return Err(Error::Validation(format!("{ctx}: units must be > 0")));
        }
        if slot_size == 0 || slot_size % u64::from(sector_size) != 0 {
            return Err(Error::Validation(format!(
                "{ctx}: slot_size {} must be a nonzero multiple of the {sector_size}-byte sector",
                format_bytes(slot_size)
            )));
        }
        if base % u64::from(sector_size) != 0 {
            return Err(Error::Validation(format!(
                "{ctx}: base {} is not sector-aligned",
                format_bytes(base)
            )));
        }
        if !allow_unaligned {
            if !slot_size.is_power_of_two() {
                return Err(Error::Validation(format!(
                    "{ctx}: slot_size {} is not a power of two (set allow_unaligned = true to override)",
                    format_bytes(slot_size)
                )));
            }
            // BANK_BASE | (unit << SLOT_SHIFT) equals addition for every unit
            // only when the base is aligned to the bank's power-of-two span:
            // a bank spanning 2 GiB must sit on a 2 GiB boundary.
            let span_pow2 = (slot_size * u64::from(rb.units)).next_power_of_two();
            if !base.is_multiple_of(span_pow2) {
                return Err(Error::Validation(format!(
                    "{ctx}: base {} is not aligned to the bank's power-of-two span {}; \
                     OR-concatenation address math needs a {} boundary \
                     (set allow_unaligned = true to override)",
                    format_bytes(base),
                    format_bytes(span_pow2),
                    format_bytes(span_pow2)
                )));
            }
        }

        let drive_type =
            match &rb.drive_type {
                Some(name) => Some(registry.get(name).cloned().ok_or_else(|| {
                    Error::Validation(format!("{ctx}: unknown drive type {name:?}"))
                })?),
                None => None,
            };
        if let Some(dt) = &drive_type {
            // Stream media has no canonical image size — the slot itself is
            // the only bound, so any slot_size is legal for a stream bank.
            if !dt.stream && dt.image_bytes > slot_size {
                return Err(Error::Validation(format!(
                    "{ctx}: slot_size {} is smaller than the {} canonical image ({})",
                    format_bytes(slot_size),
                    dt.name,
                    format_bytes(dt.image_bytes)
                )));
            }
        }

        let mut slots = BTreeMap::new();
        for rs in rb.slot {
            if rs.unit >= rb.units {
                return Err(Error::Validation(format!(
                    "{ctx}: slot unit {} out of range (bank has {} units)",
                    rs.unit, rb.units
                )));
            }
            if slots.contains_key(&rs.unit) {
                return Err(Error::Validation(format!(
                    "{ctx}: duplicate slot for unit {}",
                    rs.unit
                )));
            }
            let image = rs.image.map(|p| {
                let pb = PathBuf::from(&p);
                if pb.is_absolute() {
                    pb
                } else {
                    base_dir.join(pb)
                }
            });
            slots.insert(
                rs.unit,
                SlotEntry {
                    unit: rs.unit,
                    name: rs.name,
                    image,
                },
            );
        }

        banks.push(Bank {
            name: rb.name,
            base,
            slot_size,
            units: rb.units,
            drive_type,
            slots,
        });
    }

    // Duplicate bank names.
    for i in 0..banks.len() {
        for j in i + 1..banks.len() {
            if banks[i].name == banks[j].name {
                return Err(Error::Validation(format!(
                    "duplicate bank name {:?}",
                    banks[i].name
                )));
            }
        }
    }

    let toc_offset = match raw.toc {
        Some(t) => {
            let off = parse_size(&t, sector_size)?;
            if off % u64::from(sector_size) != 0 {
                return Err(Error::Validation(format!(
                    "toc offset {} is not sector-aligned",
                    format_bytes(off)
                )));
            }
            Some(off)
        }
        None => None,
    };

    // Overlap detection across banks and the TOC region.
    let mut extents: Vec<(String, Extent)> = banks
        .iter()
        .map(|b| {
            (
                format!("bank {:?}", b.name),
                Extent {
                    offset: b.base,
                    len: b.span(),
                },
            )
        })
        .collect();
    if let Some(off) = toc_offset {
        extents.push((
            "TOC region".to_string(),
            Extent {
                offset: off,
                len: TOC_REGION_LEN,
            },
        ));
    }
    extents.sort_by_key(|(_, e)| e.offset);
    for pair in extents.windows(2) {
        let (a_name, a) = &pair[0];
        let (b_name, b) = &pair[1];
        if a.end() > b.offset {
            return Err(Error::Validation(format!(
                "{a_name} (0x{:x}..0x{:x}) overlaps {b_name} (0x{:x}..0x{:x})",
                a.offset,
                a.end(),
                b.offset,
                b.end()
            )));
        }
    }

    Ok(Layout {
        sector_size,
        allow_unaligned,
        toc_offset,
        banks,
        registry,
        base_dir: base_dir.to_path_buf(),
    })
}
