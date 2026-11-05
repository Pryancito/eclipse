//! `/proc/diskstats`: what each disk has read and written since boot.
//!
//! One line per disk `/dev` has a node for, in Linux's 14-field form (`iostat`,
//! `vmstat -d` and the desktop's system monitor read it):
//!
//! ```text
//!    3       0 sda 1520 0 98144 0 311 0 6040 0 0 0 0
//! ```
//!
//! major, minor, name, then reads completed, reads merged, sectors read, ms
//! reading, writes completed, writes merged, sectors written, ms writing, I/Os
//! in flight, ms doing I/O, weighted ms. The drivers count requests and
//! sectors ([`IoStats`](zcore_drivers::scheme::IoStats)); merging and time are
//! not measured and read 0, which every reader takes as "not known" (a rate
//! of sectors over a reader's own interval is what a throughput graph needs).
//! Partitions are counted in their disk and get no line of their own.

use alloc::{string::String, vec::Vec};
use core::fmt::Write as _;
use zcore_drivers::scheme::IoSnapshot;

/// The major number `/dev`'s block nodes carry (see `BlockDev::metadata`).
const MAJOR: usize = 3;

/// The file, from `(minor, name, counts)` per disk.
pub(super) fn diskstats_text(disks: &[(usize, String, IoSnapshot)]) -> String {
    let mut s = String::new();
    for (minor, name, io) in disks {
        let _ = writeln!(
            s,
            "{:>4} {:>7} {} {} 0 {} 0 {} 0 {} 0 0 0 0",
            MAJOR, minor, name, io.reads, io.read_sectors, io.writes, io.write_sectors
        );
    }
    s
}

/// `/proc/diskstats` now: every disk with a node, whose driver counts.
pub(super) fn proc_diskstats_content() -> String {
    let mut disks: Vec<(usize, String, IoSnapshot)> =
        crate::fs::devfs::blockhotplug::recorded_disks()
            .into_iter()
            .filter_map(|(block, name, minor)| Some((minor, name, block.io_stats()?)))
            .collect();
    disks.sort_by_key(|d| d.0);
    diskstats_text(&disks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_has_the_fourteen_fields_linux_readers_split_on() {
        let io = IoSnapshot {
            reads: 1520,
            read_sectors: 98144,
            writes: 311,
            write_sectors: 6040,
        };
        let text = diskstats_text(&[
            (0, "sda".into(), io),
            (16, "nvme0n1".into(), IoSnapshot::default()),
        ]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let f: Vec<&str> = lines[0].split_whitespace().collect();
        assert_eq!(f.len(), 14);
        assert_eq!((f[0], f[1], f[2]), ("3", "0", "sda"));
        assert_eq!((f[3], f[5], f[7], f[9]), ("1520", "98144", "311", "6040"));
        assert_eq!(lines[1].split_whitespace().nth(2), Some("nvme0n1"));
    }

    #[test]
    fn no_disks_is_an_empty_file() {
        assert_eq!(diskstats_text(&[]), "");
    }
}
