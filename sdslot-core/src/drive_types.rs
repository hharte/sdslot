// SPDX-License-Identifier: MIT OR Apache-2.0
//! Drive type registry (design §2.2): canonical flat-image sizes and
//! recommended slot sizes per drive type, extensible from the manifest.

use serde::Serialize;

/// C/H/S geometry. `bytes()` is the canonical flat-image size for drives
/// whose image is exactly cylinders × heads × sectors × bytes_per_sector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Geometry {
    pub cylinders: u32,
    pub heads: u32,
    pub sectors: u32,
    pub bytes_per_sector: u32,
}

impl Geometry {
    pub const fn bytes(&self) -> u64 {
        self.cylinders as u64
            * self.heads as u64
            * self.sectors as u64
            * self.bytes_per_sector as u64
    }
}

impl std::fmt::Display for Geometry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}c x {}h x {}s x {}B",
            self.cylinders, self.heads, self.sectors, self.bytes_per_sector
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DriveType {
    pub name: String,
    pub geometry: Option<Geometry>,
    /// Canonical flat image size in bytes (what a simulator's attach/mount
    /// command expects). For stream types this is the nominal media capacity
    /// (display/sizing only — stream images have no canonical size).
    pub image_bytes: u64,
    /// Recommended power-of-two slot size in bytes.
    pub recommended_slot: u64,
    /// Byte-stream media (magtape): the slot holds a variable-length
    /// container (e.g. SimH `.tap`), valid at any length up to the slot.
    /// No geometry, no canonical-size validation, no `--length canonical`.
    pub stream: bool,
}

const fn geom(cylinders: u32, heads: u32, sectors: u32, bytes_per_sector: u32) -> Geometry {
    Geometry {
        cylinders,
        heads,
        sectors,
        bytes_per_sector,
    }
}

/// Builtin table. Sizes are the exact canonical flat-image byte counts.
/// RP04 and RP05 share media geometry and are registered as two names.
/// RX01/RX02: 77 tracks x 26 sectors (128 B single / 256 B double density).
/// Physical RX02 media records track 0 single-density by interchange
/// convention, but flat images are uniform streams — the canonical sizes
/// here; mixed-density images (509,184 B for RX02) trip only the
/// non-strict size warning.
const BUILTIN: &[(&str, Geometry, u64)] = &[
    // name, geometry, recommended slot
    ("RX01", geom(77, 1, 26, 128), 512 << 10),
    ("RX02", geom(77, 1, 26, 256), 1 << 20),
    ("RX50", geom(80, 1, 10, 512), 512 << 10),
    ("RX33", geom(80, 2, 15, 512), 2 << 20),
    ("RX23", geom(80, 2, 18, 512), 2 << 20),
    ("RX26", geom(80, 2, 36, 512), 4 << 20),
    ("RL01", geom(256, 2, 40, 256), 8 << 20),
    ("RL02", geom(512, 2, 40, 256), 16 << 20),
    ("RK05", geom(203, 2, 12, 512), 4 << 20),
    ("RP04", geom(411, 19, 22, 512), 128 << 20),
    ("RP05", geom(411, 19, 22, 512), 128 << 20),
    ("RP06", geom(815, 19, 22, 512), 256 << 20),
    ("RM03", geom(823, 5, 32, 512), 128 << 20),
    ("RM05", geom(823, 19, 32, 512), 512 << 20),
    ("RP07", geom(630, 32, 50, 512), 1 << 30),
];

/// Builtin byte-stream (magtape) types: no C/H/S geometry; the slot carries a
/// variable-length `.tap` container. The size column is the NOMINAL media
/// capacity — 2400 ft × 12 in/ft × 1600 B/in = 46,080,000 B for a 9-track
/// reel at 1600 BPI PE (the TM02/TM03 ceiling) — used only for display and
/// slot-sizing suggestions, never for image validation: inter-record gaps
/// only lower the real capacity, while a gapless `.tap` may legally exceed
/// it, so the only hard bound is the slot itself. TU16 and TU45 share media
/// (the RP04/RP05 precedent); they differ in speed, not capacity.
const STREAM_BUILTIN: &[(&str, u64, u64)] = &[
    // name, nominal capacity, recommended slot
    ("TU16", 46_080_000, 64 << 20),
    ("TU45", 46_080_000, 64 << 20),
];

/// Case-insensitive drive type registry.
///
/// ```
/// use sdslot_core::drive_types::Registry;
///
/// let reg = Registry::builtin();
///
/// let rl02 = reg.get("rl02").expect("builtin");        // lookup ignores case
/// assert_eq!(rl02.image_bytes, 10_485_760);            // canonical image size
/// assert_eq!(rl02.recommended_slot, 16 << 20);
/// assert_eq!(rl02.geometry.unwrap().to_string(), "512c x 2h x 40s x 256B");
///
/// // Magtapes are stream media: no geometry and no canonical size, so the
/// // slot is the only bound on a `.tap` image.
/// let tu45 = reg.get("TU45").expect("builtin");
/// assert!(tu45.stream);
/// assert!(tu45.geometry.is_none());
/// ```
#[derive(Debug, Clone)]
pub struct Registry {
    types: Vec<DriveType>,
}

impl Registry {
    pub fn builtin() -> Self {
        Registry {
            types: BUILTIN
                .iter()
                .map(|&(name, g, slot)| DriveType {
                    name: name.to_string(),
                    geometry: Some(g),
                    image_bytes: g.bytes(),
                    recommended_slot: slot,
                    stream: false,
                })
                .chain(STREAM_BUILTIN.iter().map(|&(name, bytes, slot)| DriveType {
                    name: name.to_string(),
                    geometry: None,
                    image_bytes: bytes,
                    recommended_slot: slot,
                    stream: true,
                }))
                .collect(),
        }
    }

    pub fn get(&self, name: &str) -> Option<&DriveType> {
        self.types
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(name))
    }

    /// Add or replace a type (manifest `[drive_types]` entries override builtins).
    pub fn add(&mut self, t: DriveType) {
        if let Some(existing) = self
            .types
            .iter_mut()
            .find(|e| e.name.eq_ignore_ascii_case(&t.name))
        {
            *existing = t;
        } else {
            self.types.push(t);
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &DriveType> {
        self.types.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_sizes_are_exact() {
        let r = Registry::builtin();
        let expect = [
            ("RX01", 256_256u64),
            ("RX02", 512_512),
            ("RX50", 409_600),
            ("RX33", 1_228_800),
            ("RX23", 1_474_560),
            ("RX26", 2_949_120),
            ("RL01", 5_242_880),
            ("RL02", 10_485_760),
            ("RK05", 2_494_464),
            ("RP04", 87_960_576),
            ("RP05", 87_960_576),
            ("RP06", 174_423_040),
            ("RM03", 67_420_160),
            ("RM05", 256_196_608),
            ("RP07", 516_096_000),
        ];
        for (name, bytes) in expect {
            let t = r.get(name).unwrap_or_else(|| panic!("missing {name}"));
            assert_eq!(t.image_bytes, bytes, "{name}");
            assert!(
                t.image_bytes <= t.recommended_slot,
                "{name} image exceeds recommended slot"
            );
            assert!(t.recommended_slot.is_power_of_two(), "{name} slot not pow2");
            assert!(!t.stream, "{name} is a disk type");
        }
    }

    #[test]
    fn stream_types_have_no_geometry() {
        let r = Registry::builtin();
        for name in ["TU16", "TU45"] {
            let t = r.get(name).unwrap_or_else(|| panic!("missing {name}"));
            assert!(t.stream, "{name} is stream media");
            assert!(t.geometry.is_none(), "{name} has no C/H/S geometry");
            assert_eq!(t.image_bytes, 46_080_000, "{name} nominal capacity");
            assert!(t.recommended_slot.is_power_of_two(), "{name} slot not pow2");
            assert!(t.image_bytes <= t.recommended_slot, "{name} slot too small");
        }
        assert!(r.get("tu45").is_some(), "case-insensitive");
    }

    #[test]
    fn lookup_is_case_insensitive() {
        let r = Registry::builtin();
        assert!(r.get("rl02").is_some());
        assert!(r.get("Rp06").is_some());
        assert!(r.get("rx50").is_some());
        assert!(r.get("RX99").is_none());
    }

    #[test]
    fn add_overrides_and_extends() {
        let mut r = Registry::builtin();
        let builtin_count = r.iter().count();
        r.add(DriveType {
            name: "rl02".into(),
            geometry: None,
            image_bytes: 42,
            recommended_slot: 64,
            stream: false,
        });
        assert_eq!(r.get("RL02").unwrap().image_bytes, 42);
        assert_eq!(r.iter().count(), builtin_count); // replaced, not added
        r.add(DriveType {
            name: "RK07".into(),
            geometry: None,
            image_bytes: 100,
            recommended_slot: 128,
            stream: false,
        });
        assert_eq!(r.iter().count(), builtin_count + 1);
        assert_eq!(r.get("rk07").unwrap().image_bytes, 100);
    }

    #[test]
    fn geometry_displays_and_computes() {
        let r = Registry::builtin();
        let rx01 = r.get("RX01").unwrap().geometry.unwrap();
        assert_eq!(rx01.to_string(), "77c x 1h x 26s x 128B");
        assert_eq!(rx01.bytes(), 256_256);

        // Call geom at runtime to cover it
        let g = geom(10, 2, 8, 512);
        assert_eq!(g.cylinders, 10);
    }
}
