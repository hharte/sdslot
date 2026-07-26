// SPDX-License-Identifier: MIT OR Apache-2.0
//! sdslot-core — card layout model, streaming write/read engine, and raw
//! device access for writing vintage disk images to SD cards at fixed LBA
//! offsets. See docs/sdslot-design.md for the full design.

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
