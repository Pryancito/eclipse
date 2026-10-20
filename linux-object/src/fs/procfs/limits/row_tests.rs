//! The sixteen rows of `/proc/<pid>/limits`, each one named and placed as
//! Linux names and places it.
//!
//! `the_rows_are_linux_s_and_say_what_getrlimit_says` reads every row's
//! numbers and pins the widths, but it names only `Max open files`: the name
//! sits in a 25-column field, so any other name of 25 characters or fewer
//! leaves the line exactly as long and the test exactly as green. The unit
//! field is the same, ten columns wide. So fifteen of the sixteen names and
//! fifteen of the units were free, and so was their ORDER -- the values come
//! out in resource order while the names come out of this table, so swapping
//! two rows leaves each one showing the other's limit. A supervisor that
//! greps `Max open files` for a daemon out of descriptors reads the row by
//! its name, and `prlimit`-less scripts read it by its line.

use super::*;
use crate::process::RLIM_NLIMITS;
use rcore_fs_ramfs::RamFS;

/// `lnames` in Linux's `fs/proc/base.c`, in resource order, with the empty
/// unit where Linux has none.
const LINUX_ROWS: [(&str, &str); RLIM_NLIMITS] = [
    ("Max cpu time", "seconds"),
    ("Max file size", "bytes"),
    ("Max data size", "bytes"),
    ("Max stack size", "bytes"),
    ("Max core file size", "bytes"),
    ("Max resident set", "bytes"),
    ("Max processes", "processes"),
    ("Max open files", "files"),
    ("Max locked memory", "bytes"),
    ("Max address space", "bytes"),
    ("Max file locks", "locks"),
    ("Max pending signals", "signals"),
    ("Max msgqueue size", "bytes"),
    ("Max nice priority", ""),
    ("Max realtime priority", ""),
    ("Max realtime timeout", "us"),
];

#[test]
fn each_row_carries_the_name_and_the_unit_linux_gives_that_resource() {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let text = proc_pid_limits(&lp);
    let rows: Vec<&str> = text.lines().skip(1).collect();
    assert_eq!(rows.len(), LINUX_ROWS.len());

    for (resource, (name, unit)) in LINUX_ROWS.iter().enumerate() {
        let row = rows[resource];
        // The name is the first 25 columns, and the unit is the third field
        // of what follows the two values.
        assert_eq!(
            row[..25].trim_end(),
            *name,
            "the row of resource {} reads {:?}",
            resource,
            row
        );
        let fields: Vec<&str> = row[26..].split_whitespace().collect();
        assert_eq!(
            fields.get(2).copied().unwrap_or(""),
            *unit,
            "the unit of {}",
            name
        );
        // And the two values are this resource's own, which is the other half
        // of a swapped pair: the names move and the numbers do not.
        let limit = lp.rlimit(resource, None, false).unwrap();
        let shown = |v: u64| {
            if v == RLIM_INFINITY {
                String::from("unlimited")
            } else {
                v.to_string()
            }
        };
        assert_eq!(
            (fields[0], fields[1]),
            (shown(limit.cur).as_str(), shown(limit.max).as_str()),
            "the values of {}",
            name
        );
    }
}
