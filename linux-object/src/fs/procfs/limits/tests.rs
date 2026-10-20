//! `/proc/<pid>/limits` laid out as Linux 6.18 prints it, and saying what
//! `getrlimit` says.
//!
//! Fixed pids live in 45_43x here.

use super::*;
use crate::process::{ProcessExt, RLimit, RLIMIT_NICE, RLIMIT_NOFILE};
use rcore_fs_ramfs::RamFS;

#[test]
fn the_rows_are_linux_s_and_say_what_getrlimit_says() {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let files = RLimit { cur: 200, max: 300 };
    lp.rlimit(RLIMIT_NOFILE, Some(files), false).unwrap();
    let text = proc_pid_limits(&lp);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1 + RLIM_NLIMITS);
    assert_eq!(
        lines[0],
        "Limit                     Soft Limit           Hard Limit           Units     "
    );
    assert_eq!(
        lines[1 + RLIMIT_NOFILE],
        "Max open files            200                  300                  files     "
    );
    for (resource, line) in lines[1..].iter().enumerate() {
        let limit = lp.rlimit(resource, None, false).unwrap();
        let shown = |v: u64| {
            if v == RLIM_INFINITY {
                String::from("unlimited")
            } else {
                v.to_string()
            }
        };
        let fields: Vec<&str> = line[26..].split_whitespace().collect();
        assert_eq!(
            (fields[0], fields[1]),
            (shown(limit.cur).as_str(), shown(limit.max).as_str()),
            "{}",
            line
        );
        let width = if resource == RLIMIT_NICE || resource == RLIMIT_NICE + 1 {
            68
        } else {
            78
        };
        assert_eq!(line.len(), width, "{:?}", line);
    }
    assert!(text.ends_with('\n'));
}

#[test]
fn every_process_has_the_file() {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        45_431,
        "limits",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let inode = ProcPidDirINode { pid: proc.id() }.find("limits").unwrap();
    let mut buf = [0u8; 4096];
    let len = inode.read_at(0, &mut buf).unwrap();
    assert_eq!(
        core::str::from_utf8(&buf[..len]).unwrap(),
        proc_pid_limits(proc.linux())
    );
    let listed: Vec<String> = (0..)
        .map_while(|id| ProcPidDirINode { pid: proc.id() }.get_entry(id).ok())
        .collect();
    assert!(listed.iter().any(|n| n == "limits"), "{:?}", listed);
}
