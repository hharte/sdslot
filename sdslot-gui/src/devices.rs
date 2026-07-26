// SPDX-License-Identifier: MIT OR Apache-2.0
//! The GUI's view of one enumerated block device: display label plus the
//! predicates the device picker and safety rails need.

use sdslot_core::device::DeviceInfo;
use sdslot_core::units::format_bytes;

#[derive(Clone)]
pub struct DeviceEntry {
    pub path: String,
    pub model: String,
    pub bus: String,
    pub size_bytes: Option<u64>,
    pub removable: Option<bool>,
    pub system: bool,
}

impl DeviceEntry {
    pub fn label(&self) -> String {
        let size = self
            .size_bytes
            .map(format_bytes)
            .unwrap_or_else(|| "?".into());
        let mut s = format!("{} — {size} ({})", self.model, self.bus);
        if self.removable == Some(true) {
            s.push_str(", removable");
        }
        if self.system {
            s.push_str(" [SYSTEM DISK]");
        }
        s
    }

    /// A card reader with no card reports no (or zero) capacity.
    pub fn has_media(&self) -> bool {
        self.size_bytes.is_some_and(|s| s > 0)
    }

    /// Anything not positively known to be removable is treated as
    /// non-removable, matching the CLI's safety rail.
    pub fn is_removable(&self) -> bool {
        self.removable == Some(true)
    }
}

impl From<DeviceInfo> for DeviceEntry {
    fn from(d: DeviceInfo) -> DeviceEntry {
        DeviceEntry {
            path: d.path,
            model: d.model,
            bus: d.bus,
            size_bytes: d.size_bytes,
            removable: d.removable,
            system: d.system,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> DeviceEntry {
        DeviceEntry {
            path: "/dev/sdb".into(),
            model: "SD Card".into(),
            bus: "USB".into(),
            size_bytes: Some(16_000_000_000),
            removable: Some(true),
            system: false,
        }
    }

    #[test]
    fn label_notes_removable_and_system() {
        assert!(entry().label().contains("removable"));
        let sys = DeviceEntry {
            system: true,
            ..entry()
        };
        assert!(sys.label().contains("SYSTEM DISK"));
    }

    #[test]
    fn media_and_removability_predicates() {
        assert!(entry().has_media());
        assert!(entry().is_removable());
        let empty = DeviceEntry {
            size_bytes: Some(0),
            removable: None,
            ..entry()
        };
        assert!(!empty.has_media());
        assert!(!empty.is_removable());
    }
}
