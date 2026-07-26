// SPDX-License-Identifier: MIT OR Apache-2.0
//! Engine round-trip tests against a file-backed device (design §9):
//! write → TOC → status → read (each length mode) → wipe → verify.

use std::path::{Path, PathBuf};

use sdslot_core::device::{open_device, AccessMode};
use sdslot_core::engine::{self, EngineOpts, LengthMode};
use sdslot_core::events::{NullSink, SlotState};
use sdslot_core::layout::{Layout, SlotAssign};
use sdslot_core::toc;

const MANIFEST: &str = r#"
sector_size = 512
toc = "8MiB"

[[bank]]
name = "rl"
base = "16MiB"
slot_size = "1MiB"
units = 4

  [[bank.slot]]
  unit = 0
  name = "alpha"
  image = "alpha.img"

  [[bank.slot]]
  unit = 2
  name = "beta"
  image = "beta.img"
"#;

struct Fixture {
    dir: tempfile::TempDir,
    layout: Layout,
    card: PathBuf,
}

/// Deterministic pseudo-random content, distinct per seed.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    let mut state = seed as u32 | 0x100;
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1103515245).wrapping_add(12345);
            (state >> 16) as u8
        })
        .collect()
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    // 700_000 bytes: not sector-aligned, exercises tail padding.
    std::fs::write(dir.path().join("alpha.img"), pattern(700_000, 1)).unwrap();
    std::fs::write(dir.path().join("beta.img"), pattern(1 << 20, 2)).unwrap();
    let layout = Layout::from_toml(MANIFEST, dir.path()).expect("layout");
    let card = dir.path().join("card.img");
    Fixture { dir, layout, card }
}

fn small_chunks() -> EngineOpts {
    EngineOpts {
        chunk_size: 128 * 1024, // small so multi-chunk paths are exercised
        ..EngineOpts::default()
    }
}

fn write_all(f: &Fixture, opts: &EngineOpts) {
    let jobs = engine::plan_writes(&f.layout, &[]).expect("plan");
    let mut dev = open_device(f.card.to_str().unwrap(), AccessMode::Write, 512).unwrap();
    let mut session = engine::Session::new(dev.as_mut(), &f.layout).unwrap();
    session.validate_writes(&jobs, opts).expect("validate");
    session
        .write_slots(&jobs, opts, &mut NullSink)
        .expect("write");
}

#[test]
fn write_read_verify_wipe_roundtrip() {
    let f = fixture();
    let opts = EngineOpts {
        verify: true,
        ..small_chunks()
    };
    write_all(&f, &opts);

    // On-card bytes land at the right offsets.
    let card = std::fs::read(&f.card).unwrap();
    let alpha = pattern(700_000, 1);
    let base = 16 << 20;
    assert_eq!(&card[base..base + 700_000], &alpha[..]);
    // Tail of the final sector is zero-padded.
    let padded_end = base + 700_416; // 700_000 rounded up to 512
    assert!(card[base + 700_000..padded_end].iter().all(|&b| b == 0));
    let beta = pattern(1 << 20, 2);
    let beta_base = base + 2 * (1 << 20);
    assert_eq!(&card[beta_base..beta_base + (1 << 20)], &beta[..]);

    // TOC recorded both slots.
    let mut dev = open_device(f.card.to_str().unwrap(), AccessMode::Read, 512).unwrap();
    let card_toc = toc::read_toc(dev.as_mut(), 8 << 20).unwrap().expect("toc");
    assert_eq!(card_toc.layout_hash, f.layout.layout_hash());
    assert_eq!(card_toc.entries.len(), 2);
    let e0 = card_toc.find("rl", 0).expect("rl:0 entry");
    assert_eq!(e0.length, 700_000);
    assert_eq!(e0.offset, 16 << 20);
    assert_eq!(e0.name.as_deref(), Some("alpha"));

    // Status: both match.
    let reports = engine::Session::new(dev.as_mut(), &f.layout)
        .unwrap()
        .status(&opts, &mut NullSink)
        .unwrap();
    let by_key = |b: &str, u: u32| {
        reports
            .iter()
            .find(|r| r.bank == b && r.unit == u)
            .unwrap()
            .state
    };
    assert_eq!(by_key("rl", 0), SlotState::Matches);
    assert_eq!(by_key("rl", 2), SlotState::Matches);
    assert_eq!(by_key("rl", 1), SlotState::Unknown);

    // Manifest-less status via TOC probe agrees.
    let toc_reports = engine::status_from_card(dev.as_mut(), &opts, &mut NullSink)
        .unwrap()
        .expect("probe finds TOC");
    assert_eq!(toc_reports.len(), 2);
    assert!(toc_reports.iter().all(|r| r.state == SlotState::Matches));

    // Read back with each length mode.
    let out = f.dir.path().join("out.img");
    let mut session = engine::Session::new(dev.as_mut(), &f.layout).unwrap();
    let len = session.resolve_length(0, 0, Some(LengthMode::Toc)).unwrap();
    assert_eq!(len, 700_000);
    session
        .read_slot(0, 0, len, &out, &opts, &mut NullSink)
        .unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), alpha);

    let len = session
        .resolve_length(0, 0, Some(LengthMode::Slot))
        .unwrap();
    assert_eq!(len, 1 << 20);
    // No drive_type on this bank: canonical is an error, default falls to TOC.
    assert!(session
        .resolve_length(0, 0, Some(LengthMode::Canonical))
        .is_err());
    assert_eq!(session.resolve_length(0, 0, None).unwrap(), 700_000);
    drop(session);
    drop(dev);

    // Verify passes, then fails (exit-code-3 path) after tampering.
    let jobs = engine::plan_writes(&f.layout, &[]).unwrap();
    let mut dev = open_device(f.card.to_str().unwrap(), AccessMode::Read, 512).unwrap();
    assert!(engine::Session::new(dev.as_mut(), &f.layout)
        .unwrap()
        .verify_slots(&jobs, &opts, &mut NullSink)
        .unwrap()
        .is_empty());
    drop(dev);

    let mut card = std::fs::read(&f.card).unwrap();
    card[(16 << 20) + 100] ^= 0xff;
    std::fs::write(&f.card, &card).unwrap();
    let mut dev = open_device(f.card.to_str().unwrap(), AccessMode::Read, 512).unwrap();
    let mismatches = engine::Session::new(dev.as_mut(), &f.layout)
        .unwrap()
        .verify_slots(&jobs, &opts, &mut NullSink)
        .unwrap();
    assert_eq!(mismatches, vec!["rl:0".to_string()]);
    // ...and status now reports the slot as modified (the FPGA-wrote-media cue).
    let reports = engine::Session::new(dev.as_mut(), &f.layout)
        .unwrap()
        .status(&opts, &mut NullSink)
        .unwrap();
    assert_eq!(
        reports.iter().find(|r| r.unit == 0).unwrap().state,
        SlotState::Modified
    );
    drop(dev);

    // Wipe rl:0: extent zeroed, TOC record dropped, beta untouched.
    let mut dev = open_device(f.card.to_str().unwrap(), AccessMode::Write, 512).unwrap();
    engine::Session::new(dev.as_mut(), &f.layout)
        .unwrap()
        .wipe_slots(&[(0, 0)], &opts, &mut NullSink)
        .unwrap();
    drop(dev);
    let card = std::fs::read(&f.card).unwrap();
    assert!(card[16 << 20..(16 << 20) + (1 << 20)]
        .iter()
        .all(|&b| b == 0));
    let beta_base = (16 << 20) + 2 * (1 << 20);
    assert_eq!(
        &card[beta_base..beta_base + (1 << 20)],
        &pattern(1 << 20, 2)[..]
    );
    let mut dev = open_device(f.card.to_str().unwrap(), AccessMode::Read, 512).unwrap();
    let card_toc = toc::read_toc(dev.as_mut(), 8 << 20).unwrap().unwrap();
    assert!(card_toc.find("rl", 0).is_none());
    assert!(card_toc.find("rl", 2).is_some());

    // The wiped slot (all zeros vs its manifest image) reports as Wiped,
    // distinct from merely differing content.
    let reports = engine::Session::new(dev.as_mut(), &f.layout)
        .unwrap()
        .status(&opts, &mut NullSink)
        .unwrap();
    let state_of = |u: u32| reports.iter().find(|r| r.unit == u).unwrap().state;
    assert_eq!(state_of(0), SlotState::Wiped);
    assert_eq!(state_of(2), SlotState::Matches);
}

#[test]
fn override_writes_only_named_slot() {
    let f = fixture();
    let opts = small_chunks();
    let gamma = pattern(300_000, 3);
    std::fs::write(f.dir.path().join("gamma.img"), &gamma).unwrap();

    let assign = SlotAssign::parse(&format!(
        "rl:1={}",
        f.dir.path().join("gamma.img").display()
    ))
    .unwrap();
    let jobs = engine::plan_writes(&f.layout, &[assign]).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!((jobs[0].bank.as_str(), jobs[0].unit), ("rl", 1));

    let mut dev = open_device(f.card.to_str().unwrap(), AccessMode::Write, 512).unwrap();
    engine::Session::new(dev.as_mut(), &f.layout)
        .unwrap()
        .write_slots(&jobs, &opts, &mut NullSink)
        .unwrap();
    drop(dev);

    let card = std::fs::read(&f.card).unwrap();
    let slot1 = (16 << 20) + (1 << 20);
    assert_eq!(&card[slot1..slot1 + 300_000], &gamma[..]);
    // Slot 0 was never touched: the file is sparse/zero there.
    assert!(card[16 << 20..(16 << 20) + 1000].iter().all(|&b| b == 0));
}

#[test]
fn oversized_image_is_rejected() {
    let f = fixture();
    std::fs::write(f.dir.path().join("big.img"), pattern((1 << 20) + 1, 4)).unwrap();
    let assign =
        SlotAssign::parse(&format!("rl:0={}", f.dir.path().join("big.img").display())).unwrap();
    let jobs = engine::plan_writes(&f.layout, &[assign]).unwrap();
    let mut dev = open_device(f.card.to_str().unwrap(), AccessMode::Write, 512).unwrap();
    let err = engine::Session::new(dev.as_mut(), &f.layout)
        .unwrap()
        .validate_writes(&jobs, &small_chunks())
        .expect_err("oversized image must be rejected");
    assert!(err.to_string().contains("exceeds"), "{err}");
}

#[test]
fn strict_size_enforces_canonical() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("short.rl02"), pattern(1000, 5)).unwrap();
    let layout = Layout::from_toml(
        r#"
[[bank]]
name = "rl"
base = "0"
slot_size = "16MiB"
units = 2
drive_type = "RL02"

  [[bank.slot]]
  unit = 0
  image = "short.rl02"
"#,
        dir.path(),
    )
    .unwrap();
    let card = dir.path().join("card.img");
    let jobs = engine::plan_writes(&layout, &[]).unwrap();
    let mut dev = open_device(card.to_str().unwrap(), AccessMode::Write, 512).unwrap();

    let lax = engine::Session::new(dev.as_mut(), &layout)
        .unwrap()
        .validate_writes(&jobs, &EngineOpts::default())
        .expect("non-strict is a warning");
    assert_eq!(lax.len(), 1);
    assert!(lax[0].contains("canonical"));

    let strict = EngineOpts {
        strict_size: true,
        ..EngineOpts::default()
    };
    let err = engine::Session::new(dev.as_mut(), &layout)
        .unwrap()
        .validate_writes(&jobs, &strict)
        .expect_err("strict must reject");
    assert!(err.to_string().contains("canonical"), "{err}");
}

#[test]
fn flat_image_spans_full_layout() {
    let f = fixture();
    let opts = small_chunks();
    write_all(&f, &opts);
    let mut dev = open_device(f.card.to_str().unwrap(), AccessMode::Write, 512).unwrap();
    engine::Session::new(dev.as_mut(), &f.layout)
        .unwrap()
        .extend_to_full_layout()
        .unwrap();
    drop(dev);
    assert_eq!(
        std::fs::metadata(&f.card).unwrap().len(),
        f.layout.max_extent_end()
    );
}

#[test]
fn toc_survives_corruption_gracefully() {
    let f = fixture();
    write_all(&f, &small_chunks());
    // Corrupt the TOC payload: read_toc must return None, not garbage.
    let mut card = std::fs::read(&f.card).unwrap();
    card[(8 << 20) + 100] ^= 0xff;
    std::fs::write(&f.card, &card).unwrap();
    let mut dev = open_device(f.card.to_str().unwrap(), AccessMode::Read, 512).unwrap();
    assert!(toc::read_toc(dev.as_mut(), 8 << 20).unwrap().is_none());
}

#[test]
fn plan_and_length_error_branches() {
    let f = fixture();
    let opts = small_chunks();
    write_all(&f, &opts);

    // The same slot named twice is order-dependent and refused.
    let a = SlotAssign::parse(&format!(
        "rl:1={}",
        f.dir.path().join("alpha.img").display()
    ))
    .unwrap();
    let b =
        SlotAssign::parse(&format!("rl:1={}", f.dir.path().join("beta.img").display())).unwrap();
    let err = engine::plan_writes(&f.layout, &[a, b]).expect_err("duplicate slot");
    assert!(err.to_string().contains("more than once"), "{err}");

    // A missing image file fails at planning, not mid-write.
    let missing = SlotAssign::parse("rl:1=no-such-file.img").unwrap();
    let err = engine::plan_writes(&f.layout, &[missing]).expect_err("missing image");
    assert!(err.to_string().contains("cannot open image"), "{err}");

    // An explicit --length larger than the slot is refused.
    let mut dev = open_device(f.card.to_str().unwrap(), AccessMode::Read, 512).unwrap();
    let err = engine::Session::new(dev.as_mut(), &f.layout)
        .unwrap()
        .resolve_length(0, 0, Some(LengthMode::Bytes(2 << 20)))
        .expect_err("length exceeds slot");
    assert!(err.to_string().contains("exceeds"), "{err}");
}

#[test]
fn zero_detection_distinguishes_blank_from_data() {
    let f = fixture();
    let opts = small_chunks();
    write_all(&f, &opts);
    let mut dev = open_device(f.card.to_str().unwrap(), AccessMode::Read, 512).unwrap();
    let (_, zeroed) = engine::hash_device_range_detect_zero(
        dev.as_mut(),
        16 << 20,
        700_000,
        opts.chunk_size,
        |_| {},
    )
    .unwrap();
    assert!(!zeroed);
    // The slot after rl:0's content (unit 1) was never written: zeros.
    let (_, zeroed) = engine::hash_device_range_detect_zero(
        dev.as_mut(),
        (16 << 20) + (1 << 20),
        1 << 20,
        opts.chunk_size,
        |_| {},
    )
    .unwrap();
    assert!(zeroed);
}

#[test]
fn toc_header_validation_rejects_corruption() {
    let f = fixture();
    write_all(&f, &small_chunks());
    let toc_off: u64 = 8 << 20;
    let valid = std::fs::read(&f.card).unwrap();

    // Wrong version.
    let mut bad = valid.clone();
    bad[toc_off as usize + 8] = 0x7f;
    std::fs::write(&f.card, &bad).unwrap();
    let mut dev = open_device(f.card.to_str().unwrap(), AccessMode::Read, 512).unwrap();
    assert!(toc::read_toc(dev.as_mut(), toc_off).unwrap().is_none());
    drop(dev);

    // Absurd payload length.
    let mut bad = valid.clone();
    bad[toc_off as usize + 12..toc_off as usize + 16].copy_from_slice(&u32::MAX.to_le_bytes());
    std::fs::write(&f.card, &bad).unwrap();
    let mut dev = open_device(f.card.to_str().unwrap(), AccessMode::Read, 512).unwrap();
    assert!(toc::read_toc(dev.as_mut(), toc_off).unwrap().is_none());
    drop(dev);

    // Wrong magic.
    let mut bad = valid.clone();
    bad[toc_off as usize] = b'X';
    std::fs::write(&f.card, &bad).unwrap();
    let mut dev = open_device(f.card.to_str().unwrap(), AccessMode::Read, 512).unwrap();
    assert!(toc::read_toc(dev.as_mut(), toc_off).unwrap().is_none());
}

#[test]
fn toc_rejects_oversize_payload_on_write() {
    let f = fixture();
    let mut giant = toc::Toc::new("h".into());
    giant.upsert(toc::TocEntry {
        bank: "rl".into(),
        unit: 0,
        offset: 0,
        name: Some("x".repeat(200_000)),
        length: 1,
        sha256: String::new(),
        written_unix: 0,
    });
    let mut dev = open_device(f.card.to_str().unwrap(), AccessMode::Write, 512).unwrap();
    let err = toc::write_toc(dev.as_mut(), 8 << 20, &giant).expect_err("oversize TOC");
    assert!(err.to_string().contains("exceeds"), "{err}");
}

#[test]
fn file_device_path_detection() {
    use sdslot_core::device::is_platform_device_path;
    assert!(is_platform_device_path("\\\\.\\PhysicalDrive2"));
    assert!(is_platform_device_path("/dev/sdb"));
    assert!(is_platform_device_path("/dev/rdisk4"));
    assert!(!is_platform_device_path("card.img"));
    assert!(!is_platform_device_path(
        Path::new("C:\\temp\\card.img").to_str().unwrap()
    ));
}
