// SPDX-License-Identifier: MIT OR Apache-2.0
//! sdslot-core — card layout model, streaming write/read engine, and raw
//! device access for writing vintage disk images to SD cards at fixed LBA
//! offsets. See docs/sdslot-design.md for the full design.
//!
//! A manifest describes the card once; the same description resolves slots to
//! byte extents on the host side and exports the addressing constants the FPGA
//! side needs:
//!
//! ```
//! use sdslot_core::layout::{Layout, SlotRef};
//! use sdslot_core::rtl::{export, RtlFormat};
//! use std::path::Path;
//!
//! let layout = Layout::from_toml(
//!     r#"
//!     sector_size = 512
//!
//!     [[bank]]
//!     name       = "rl"
//!     base       = "0"
//!     slot_size  = "16MiB"
//!     units      = 16
//!     drive_type = "RL02"
//!     "#,
//!     Path::new("."),
//! )?;
//!
//! // Where unit 3 of the "rl" bank lives on the card.
//! let (bank, unit) = layout.resolve_slot(&SlotRef::parse("rl:3")?)?;
//! let extent = layout.slot_extent(bank, unit);
//! assert_eq!(extent.offset, 3 * (16 << 20));
//! assert_eq!(extent.len, 16 << 20);
//!
//! // The same layout as Verilog parameters, so the RTL addresses the slot
//! // with `lba = BASE_LBA | (unit << SLOT_SHIFT) | block_offset`.
//! let vh = export(&layout, RtlFormat::Vh, "card_layout")?;
//! assert!(vh.contains("localparam RL_SLOT_SHIFT = 15;"));
//! # Ok::<(), sdslot_core::Error>(())
//! ```

pub mod device;
pub mod drive_types;
pub mod engine;
pub mod error;
pub mod events;
pub mod layout;
pub mod rtl;
pub mod toc;
pub mod units;

pub use error::{Error, Result};

/// Crate version plus the git revision it was built from, e.g.
/// `0.1.0 (git 4a1322b9aeaa, dirty)` — "dirty" marks uncommitted tracked
/// changes. Plain crate version when built without git (crates.io tarball).
pub const VERSION_FULL: &str = env!("SDSLOT_VERSION_FULL");

/// Copyright line for both frontends' `--version` output and sign-on.
pub const COPYRIGHT: &str = "Copyright (c) 2026 Howard M. Harte";

/// The canonical public repository.
pub const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// Multi-line sign-on: version, copyright, license, and repository link.
pub const LONG_VERSION: &str = concat!(
    env!("SDSLOT_VERSION_FULL"),
    "\nCopyright (c) 2026 Howard M. Harte",
    "\nMIT OR Apache-2.0 license — ",
    env!("CARGO_PKG_REPOSITORY")
);
