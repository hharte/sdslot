// SPDX-License-Identifier: MIT OR Apache-2.0
//! Size parsing for manifest fields: plain bytes, binary suffixes
//! (KiB/MiB/GiB/TiB, with bare K/M/G/T accepted as shorthand), or an
//! `NNNs` LBA count scaled by the layout's sector size.

use crate::error::{Error, Result};

/// Parse a size expression. `sector_size` scales the `s` (LBA) suffix.
pub fn parse_size(s: &str, sector_size: u32) -> Result<u64> {
    let t = s.trim();
    if t.is_empty() {
        return Err(Error::Validation("empty size expression".into()));
    }
    let bad = || {
        Error::Validation(format!(
            "cannot parse size {t:?}: expected bytes, a KiB/MiB/GiB/TiB suffix, or \"NNNs\" LBAs"
        ))
    };

    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (digits, suffix) = t.split_at(split);
    if digits.is_empty() {
        return Err(bad());
    }
    let n: u64 = digits.parse().map_err(|_| bad())?;
    let mult: u64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "s" => u64::from(sector_size),
        "k" | "kib" => 1 << 10,
        "m" | "mib" => 1 << 20,
        "g" | "gib" => 1 << 30,
        "t" | "tib" => 1 << 40,
        _ => return Err(bad()),
    };
    n.checked_mul(mult)
        .ok_or_else(|| Error::Validation(format!("size {t:?} overflows u64")))
}

/// Render a byte count in the most compact exact binary unit.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (1 << 40, "TiB"),
        (1 << 30, "GiB"),
        (1 << 20, "MiB"),
        (1 << 10, "KiB"),
    ];
    for (m, name) in UNITS {
        if bytes >= m && bytes.is_multiple_of(m) && bytes / m < 10_000 {
            return format!("{} {name}", bytes / m);
        }
    }
    // Not a tidy exact multiple: approximate in the largest applicable unit.
    for (m, name) in UNITS {
        if bytes >= m {
            return format!("~{:.2} {name}", bytes as f64 / m as f64);
        }
    }
    format!("{bytes} B")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_suffixed() {
        assert_eq!(parse_size("0", 512).unwrap(), 0);
        assert_eq!(parse_size("512", 512).unwrap(), 512);
        assert_eq!(parse_size("16MiB", 512).unwrap(), 16 << 20);
        assert_eq!(parse_size("512MiB", 512).unwrap(), 512 << 20);
        assert_eq!(parse_size("2GiB", 512).unwrap(), 2 << 30);
        assert_eq!(parse_size("1TiB", 512).unwrap(), 1 << 40);
        assert_eq!(parse_size("8KiB", 512).unwrap(), 8 << 10);
        assert_eq!(parse_size(" 4 MiB ", 512).unwrap(), 4 << 20);
        assert_eq!(parse_size("16mib", 512).unwrap(), 16 << 20);
        assert_eq!(parse_size("3M", 512).unwrap(), 3 << 20);
    }

    #[test]
    fn parses_lba_suffix() {
        assert_eq!(parse_size("2048s", 512).unwrap(), 2048 * 512);
        assert_eq!(parse_size("100s", 4096).unwrap(), 100 * 4096);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_size("", 512).is_err());
        assert!(parse_size("MiB", 512).is_err());
        assert!(parse_size("12XB", 512).is_err());
        assert!(parse_size("-5", 512).is_err());
        assert!(parse_size("99999999999999999999", 512).is_err());
    }

    #[test]
    fn formats() {
        assert_eq!(format_bytes(16 << 20), "16 MiB");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(10_485_760), "10 MiB");
        // Untidy sizes approximate in the largest applicable unit, and
        // huge exact multiples of small units don't render as "1953514584
        // KiB".
        assert_eq!(format_bytes(31_914_983_424), "~29.72 GiB");
        assert_eq!(format_bytes(2_000_398_934_016), "~1.82 TiB");
        assert_eq!(format_bytes(1_000), "1000 B");
    }
}
