// SPDX-License-Identifier: MIT OR Apache-2.0
//! Golden layout tests (design §9): the design-doc manifest parses to the
//! expected geometry and RTL constants; deliberately broken layouts
//! (overlaps, oversized images, non-power-of-two) are rejected.

use std::path::Path;

use sdslot_core::layout::{Layout, SlotAssign, SlotRef};
use sdslot_core::rtl::{export, RtlFormat};

const GOLDEN: &str = r#"
sector_size = 512
toc = "496MiB"

[[bank]]
name       = "rl"
base       = "0"
slot_size  = "16MiB"
units      = 8
drive_type = "RL02"

  [[bank.slot]]
  unit  = 0
  name  = "xxdp-rl02"
  image = "images/xxdp25.rl02"

  [[bank.slot]]
  unit  = 1
  name  = "rt11-work"
  image = "images/rt11work.rl02"

[[bank]]
name       = "rp"
base       = "2GiB"
slot_size  = "256MiB"
units      = 8
drive_type = "RP06"

  [[bank.slot]]
  unit  = 7
  name  = "211bsd"
  image = "images/211bsd_rp06.dsk"
"#;

fn load(text: &str) -> Layout {
    Layout::from_toml(text, Path::new(".")).expect("layout should parse")
}

#[test]
fn golden_layout_resolves() {
    let l = load(GOLDEN);
    assert_eq!(l.sector_size, 512);
    assert_eq!(l.toc_offset, Some(496 << 20));
    assert_eq!(l.banks.len(), 2);

    let rl = &l.banks[0];
    assert_eq!(rl.name, "rl");
    assert_eq!(rl.base, 0);
    assert_eq!(rl.slot_size, 16 << 20);
    assert_eq!(rl.units, 8);
    assert_eq!(rl.drive_type.as_ref().unwrap().image_bytes, 10_485_760);
    assert_eq!(rl.slots.len(), 2);

    let rp = &l.banks[1];
    assert_eq!(rp.base, 2 << 30);
    assert_eq!(rp.slot_offset(7), (2 << 30) + 7 * (256 << 20));

    // OR-concatenation equals addition for every unit of every bank.
    for bank in &l.banks {
        for unit in 0..bank.units {
            assert_eq!(
                bank.base | (u64::from(unit) * bank.slot_size),
                bank.base + u64::from(unit) * bank.slot_size,
                "bank {} unit {unit}",
                bank.name
            );
        }
    }

    // Full layout ends after the rp bank.
    assert_eq!(l.max_extent_end(), (2u64 << 30) + 8 * (256u64 << 20));
}

#[test]
fn rejects_base_not_aligned_to_span() {
    // The rev 0.2 example: a 2 GiB-span bank based at 512 MiB. OR-math would
    // silently break for most units, so this is now a hard error (§2.1).
    expect_reject(
        r#"
[[bank]]
name = "rp"
base = "512MiB"
slot_size = "256MiB"
units = 8
drive_type = "RP06"
"#,
        "aligned to the bank's power-of-two span",
    );
    // ...unless the layout opts out of concatenation math entirely.
    let l = load(
        r#"
allow_unaligned = true

[[bank]]
name = "rp"
base = "512MiB"
slot_size = "256MiB"
units = 8
drive_type = "RP06"
"#,
    );
    assert_eq!(l.banks[0].base, 512 << 20);
}

#[test]
fn slot_refs_resolve() {
    let l = load(GOLDEN);
    assert_eq!(
        l.resolve_slot(&SlotRef::parse("rl:1").unwrap()).unwrap(),
        (0, 1)
    );
    assert_eq!(
        l.resolve_slot(&SlotRef::parse("rp:7").unwrap()).unwrap(),
        (1, 7)
    );
    // Bare unit is ambiguous with two banks.
    assert!(l.resolve_slot(&SlotRef::parse("1").unwrap()).is_err());
    // Unknown bank / out-of-range unit.
    assert!(l.resolve_slot(&SlotRef::parse("rk:0").unwrap()).is_err());
    assert!(l.resolve_slot(&SlotRef::parse("rl:8").unwrap()).is_err());

    let assign = SlotAssign::parse("rl:1=foo.rl02").unwrap();
    assert_eq!(assign.slot, SlotRef::parse("rl:1").unwrap());
    assert_eq!(assign.image.unwrap().to_str().unwrap(), "foo.rl02");
}

#[test]
fn single_bank_allows_bare_unit() {
    let l = load(
        r#"
[[bank]]
name = "rl"
base = "0"
slot_size = "16MiB"
units = 4
"#,
    );
    assert_eq!(
        l.resolve_slot(&SlotRef::parse("2").unwrap()).unwrap(),
        (0, 2)
    );
}

#[test]
fn rtl_export_matches_design_example() {
    let l = load(GOLDEN);
    let vh = export(&l, RtlFormat::Vh, "card_layout").unwrap();
    // The worked example from design §2.4.
    assert!(
        vh.contains("localparam RL_BASE_LBA   = 32'h0000_0000;"),
        "{vh}"
    );
    assert!(vh.contains("localparam RL_SLOT_SHIFT = 15;"), "{vh}");
    assert!(
        vh.contains("localparam RP_BASE_LBA   = 32'h0040_0000;"),
        "{vh}"
    );
    assert!(vh.contains("localparam RP_SLOT_SHIFT = 19;"), "{vh}");
    assert!(vh.contains("localparam RP_UNITS      = 4'd8;"), "{vh}");

    let sv = export(&l, RtlFormat::Sv, "card_layout").unwrap();
    assert!(sv.contains("package card_layout;"), "{sv}");
    assert!(sv.contains("endpackage"), "{sv}");

    let rs = export(&l, RtlFormat::Rust, "card_layout").unwrap();
    assert!(
        rs.contains("pub const RP_BASE_LBA: u32 = 0x0040_0000;"),
        "{rs}"
    );

    let h = export(&l, RtlFormat::CHeader, "card_layout").unwrap();
    assert!(h.contains("#ifndef CARD_LAYOUT_H"), "{h}");
    assert!(h.contains("#define RP_SLOT_SHIFT 19"), "{h}");
}

fn expect_reject(text: &str, needle: &str) {
    match Layout::from_toml(text, Path::new(".")) {
        Ok(_) => panic!("layout unexpectedly accepted (wanted error containing {needle:?})"),
        Err(e) => {
            let msg = e.to_string();
            assert!(msg.contains(needle), "error {msg:?} lacks {needle:?}");
        }
    }
}

#[test]
fn rejects_overlapping_banks() {
    expect_reject(
        r#"
[[bank]]
name = "a"
base = "0"
slot_size = "16MiB"
units = 8

[[bank]]
name = "b"
base = "64MiB"
slot_size = "16MiB"
units = 4
"#,
        "overlaps",
    );
}

#[test]
fn rejects_toc_inside_bank() {
    expect_reject(
        r#"
toc = "16MiB"

[[bank]]
name = "a"
base = "0"
slot_size = "16MiB"
units = 4
"#,
        "overlaps",
    );
}

#[test]
fn rejects_non_power_of_two_slot() {
    expect_reject(
        r#"
[[bank]]
name = "a"
base = "0"
slot_size = "12MiB"
units = 4
"#,
        "power of two",
    );
}

#[test]
fn allow_unaligned_overrides_pow2_rule() {
    let l = load(
        r#"
allow_unaligned = true

[[bank]]
name = "a"
base = "0"
slot_size = "12MiB"
units = 4
"#,
    );
    assert_eq!(l.banks[0].slot_size, 12 << 20);
    // But RTL export cannot produce a SLOT_SHIFT for it.
    assert!(export(&l, RtlFormat::Vh, "x").is_err());
}

#[test]
fn rejects_slot_smaller_than_drive_type() {
    expect_reject(
        r#"
[[bank]]
name = "a"
base = "0"
slot_size = "8MiB"
units = 4
drive_type = "RL02"
"#,
        "smaller than",
    );
}

#[test]
fn rejects_unit_out_of_range_and_duplicates() {
    expect_reject(
        r#"
[[bank]]
name = "a"
base = "0"
slot_size = "16MiB"
units = 2

  [[bank.slot]]
  unit = 2
"#,
        "out of range",
    );
    expect_reject(
        r#"
[[bank]]
name = "a"
base = "0"
slot_size = "16MiB"
units = 2

  [[bank.slot]]
  unit = 0

  [[bank.slot]]
  unit = 0
"#,
        "duplicate",
    );
}

#[test]
fn rejects_unknown_keys_and_types() {
    expect_reject(
        r#"
[[bank]]
name = "a"
base = "0"
slot_size = "16MiB"
units = 2
drive_type = "RX99"
"#,
        "unknown drive type",
    );
    expect_reject(
        r#"
[[bank]]
name = "a"
base = "0"
slot_size = "16MiB"
units = 2
bogus_key = 1
"#,
        "parse error",
    );
}

#[test]
fn rejects_structural_errors() {
    expect_reject("", "defines no banks");
    expect_reject(
        "sector_size = 100\n[[bank]]\nname=\"a\"\nbase=\"0\"\nslot_size=\"16MiB\"\nunits=1\n",
        "power of two",
    );
    expect_reject(
        "[[bank]]\nname=\"a\"\nbase=\"0\"\nslot_size=\"16MiB\"\nunits=0\n",
        "units must be > 0",
    );
    expect_reject(
        "[[bank]]\nname=\"a\"\nbase=\"1000\"\nslot_size=\"16MiB\"\nunits=1\n",
        "not sector-aligned",
    );
    expect_reject(
        "[[bank]]\nname=\"a\"\nbase=\"0\"\nslot_size=\"1000\"\nunits=1\n",
        "nonzero multiple",
    );
    expect_reject(
        "toc = \"1000\"\n[[bank]]\nname=\"a\"\nbase=\"0\"\nslot_size=\"16MiB\"\nunits=1\n",
        "not sector-aligned",
    );
    expect_reject(
        "[[bank]]\nname=\"a\"\nbase=\"0\"\nslot_size=\"16MiB\"\nunits=1\n\
         [[bank]]\nname=\"a\"\nbase=\"64MiB\"\nslot_size=\"16MiB\"\nunits=1\n",
        "duplicate bank name",
    );
}

#[test]
fn rejects_bad_drive_type_definitions() {
    expect_reject(
        "[drive_types.X]\ncylinders = 10\n\
         [[bank]]\nname=\"a\"\nbase=\"0\"\nslot_size=\"16MiB\"\nunits=1\n",
        "give all of",
    );
    expect_reject(
        "[drive_types.X]\nrecommended_slot = \"1MiB\"\n\
         [[bank]]\nname=\"a\"\nbase=\"0\"\nslot_size=\"16MiB\"\nunits=1\n",
        "needs image_size",
    );
    expect_reject(
        "[drive_types.X]\nimage_size = \"10MiB\"\nrecommended_slot = \"1MiB\"\n\
         [[bank]]\nname=\"a\"\nbase=\"0\"\nslot_size=\"16MiB\"\nunits=1\n",
        "exceeds recommended_slot",
    );
}

#[test]
fn slot_reference_parse_errors() {
    assert!(SlotRef::parse(":1").is_err()); // empty bank
    assert!(SlotRef::parse("rl:").is_err()); // empty unit
    assert!(SlotRef::parse("rl:x").is_err()); // non-numeric unit
    assert!(SlotAssign::parse("rl:1=").is_err()); // empty image path
                                                  // Display round-trips both forms.
    assert_eq!(SlotRef::parse("rl:3").unwrap().to_string(), "rl:3");
    assert_eq!(SlotRef::parse("3").unwrap().to_string(), "3");
}

#[test]
fn custom_drive_types_extend_registry() {
    let l = load(
        r#"
[drive_types.RX50]
image_size = "409600"
recommended_slot = "512KiB"

[drive_types.RK07]
cylinders = 815
heads = 3
sectors = 22
bytes_per_sector = 512

[[bank]]
name = "rx"
base = "0"
slot_size = "512KiB"
units = 4
drive_type = "RX50"
"#,
    );
    assert_eq!(l.registry.get("RX50").unwrap().image_bytes, 409_600);
    let rk07 = l.registry.get("rk07").unwrap();
    assert_eq!(rk07.image_bytes, 815 * 3 * 22 * 512);
    assert_eq!(l.banks[0].drive_type.as_ref().unwrap().name, "RX50");
}

#[test]
fn layout_hash_tracks_geometry_not_slots() {
    let a = load(GOLDEN);
    let b = load(&GOLDEN.replace("xxdp-rl02", "renamed-slot"));
    assert_eq!(a.layout_hash(), b.layout_hash());
    let c = load(&GOLDEN.replacen("units      = 8", "units      = 16", 1));
    assert_ne!(a.layout_hash(), c.layout_hash());
}

#[test]
fn test_layout_uncovered_branches() {
    let assign = SlotAssign::parse("rl:1").unwrap();
    assert_eq!(assign.slot, SlotRef::parse("rl:1").unwrap());
    assert!(assign.image.is_none());

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("card.toml");
    std::fs::write(&path, GOLDEN).unwrap();
    let l = Layout::load(&path).unwrap();
    assert_eq!(l.sector_size, 512);

    assert!(Layout::load(&dir.path().join("nonexistent.toml")).is_err());

    let no_toc = r#"
sector_size = 512
[[bank]]
name = "rl"
base = "0"
slot_size = "16MiB"
units = 4
"#;
    let l_no_toc = load(no_toc);
    assert_eq!(l_no_toc.max_extent_end(), 4 * 16 * 1024 * 1024);

    let abs_path = if cfg!(windows) {
        "C:\\test\\image.rl02"
    } else {
        "/test/image.rl02"
    };
    let abs_path_manifest = format!(
        r#"
sector_size = 512
[[bank]]
name = "rl"
base = "0"
slot_size = "16MiB"
units = 1
  [[bank.slot]]
  unit = 0
  image = "{}"
"#,
        abs_path.replace("\\", "\\\\")
    );
    let l_abs = load(&abs_path_manifest);
    let img = l_abs.banks[0]
        .slots
        .get(&0)
        .unwrap()
        .image
        .as_ref()
        .unwrap();
    assert!(img.is_absolute());
}
