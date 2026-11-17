//! Free space on the state directory's volume, and when it counts as low (DESIGN.md, "Disk"). Pure rules over a
//! measured `Space`: the daemon decides what to do about them.

use rustix::fs::statvfs;
use std::path::Path;

/// Decimal gigabytes, as disk vendors and the manifest's `min_free` count them.
pub const GB: u64 = 1_000_000_000;

/// How much more than the low line must be free before a volume that was low counts as recovered, so a volume
/// hovering at the line does not log a warning and a recovery every few seconds.
const HYSTERESIS: u64 = GB;

/// What a volume has, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    pub free: u64,
    pub total: u64,
}

impl Space {
    /// The volume of `path`. Free is what an unprivileged process may use (`f_bavail`), so space the system could
    /// purge (APFS) is not counted. None when the volume cannot be measured: callers treat that as "no information",
    /// never as a full disk.
    pub fn measure(path: &Path) -> Option<Space> {
        let found = statvfs(path).ok()?;
        Some(Space {
            free: found.f_bavail.saturating_mul(found.f_frsize),
            total: found.f_blocks.saturating_mul(found.f_frsize),
        })
    }

    /// `12.0 GB free of 245.0 GB`.
    pub fn describe(self) -> String {
        format!("{} free of {}", gigabytes(self.free), gigabytes(self.total))
    }
}

/// `bytes` as decimal gigabytes with one decimal, rounded down so that a volume just under a threshold never reads
/// as the threshold itself.
pub fn gigabytes(bytes: u64) -> String {
    let tenths = bytes / (GB / 10);
    format!("{}.{} GB", tenths / 10, tenths % 10)
}

/// The manifest's `min_free` (gigabytes, finite and not negative: `Manifest::load` checked) in bytes. A float cast
/// saturates, so a huge value stays huge instead of wrapping.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a saturating cast of a value `Manifest::load` checked to be finite and not negative"
)]
pub fn gigabytes_to_bytes(gigabytes: f64) -> u64 {
    (gigabytes * 1e9).round() as u64
}

/// Whether the volume is low, for `min_free` bytes (0 turns the watching off): free below the larger of `min_free`
/// and 5 % of the volume, so a big volume warns while there is still room to act. A volume that was low stays so
/// until it has `HYSTERESIS` more than that line.
pub fn low(space: Space, min_free: u64, was_low: bool) -> bool {
    if min_free == 0 {
        return false;
    }
    let line = min_free.max(space.total / 20);
    let clear = if was_low {
        line.saturating_add(HYSTERESIS)
    } else {
        line
    };
    space.free < clear
}

/// Whether `stage` refuses: free below `min_free` itself, with no share of the volume and no hysteresis, so on a
/// large volume the warning (`low`) comes first.
pub fn refuses(space: Space, min_free: u64) -> bool {
    min_free > 0 && space.free < min_free
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A volume of `free` and `total` megabytes.
    fn space(free: u64, total: u64) -> Space {
        Space {
            free: free * 1_000_000,
            total: total * 1_000_000,
        }
    }

    #[test]
    fn low_is_the_larger_of_min_free_and_five_percent() {
        let ten = 10 * GB;
        for (free, total, expected) in [
            (18_000, 245_000, false),
            (12_000, 245_000, true),
            (60_000, 1_000_000, false),
            (40_000, 1_000_000, true),
            (9_000, 100_000, true),
            (10_000, 100_000, false),
        ] {
            assert_eq!(
                low(space(free, total), ten, false),
                expected,
                "{free}/{total}"
            );
        }
    }

    #[test]
    fn a_low_volume_stays_low_until_a_gigabyte_above_the_line() {
        let ten = 10 * GB;
        // The line of a 245 GB volume is 12.25 GB.
        assert!(low(space(12_500, 245_000), ten, true));
        assert!(!low(space(12_500, 245_000), ten, false));
        assert!(low(space(13_200, 245_000), ten, true));
        assert!(!low(space(13_300, 245_000), ten, true));
    }

    #[test]
    fn no_min_free_turns_the_watching_off() {
        assert!(!low(space(0, 245_000), 0, false));
        assert!(!low(space(0, 245_000), 0, true));
        assert!(!refuses(space(0, 245_000), 0));
    }

    #[test]
    fn stage_refuses_below_min_free_only() {
        let ten = 10 * GB;
        assert!(refuses(space(9_200, 245_000), ten));
        assert!(!refuses(space(10_000, 245_000), ten));
        // Low, with the warning out, but stage still works.
        assert!(low(space(40_000, 1_000_000), ten, false));
        assert!(!refuses(space(40_000, 1_000_000), ten));
    }

    #[test]
    fn sizes_read_in_decimal_gigabytes_rounded_down() {
        assert_eq!(
            space(12_000, 245_000).describe(),
            "12.0 GB free of 245.0 GB"
        );
        assert_eq!(gigabytes(9_999_999_999), "9.9 GB");
        assert_eq!(gigabytes(0), "0.0 GB");
        assert_eq!(gigabytes_to_bytes(10.0), 10 * GB);
        assert_eq!(gigabytes_to_bytes(0.5), GB / 2);
        assert_eq!(gigabytes_to_bytes(1e300), u64::MAX);
    }

    #[test]
    fn the_volume_of_a_directory_is_measured() {
        let temp = tempfile::tempdir().unwrap();
        let measured = Space::measure(temp.path()).unwrap();
        assert!(measured.total > 0 && measured.free <= measured.total);
        assert!(Space::measure(&temp.path().join("missing")).is_none());
    }
}
