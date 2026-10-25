//! `oom_score_adj`, `oom_adj` and `oom_score` as Linux 6.18 reads and
//! writes them, value by value as measured there.
//!
//! Fixed pids live in 49_54x here.

use super::*;
use crate::process::ProcessExt;
use crate::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;

fn a_process(pid: u64) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "oom", LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

fn read(pid: u64, name: &str) -> String {
    let file = ProcPidDirINode { pid }.find(name).unwrap();
    let mut buf = [0u8; 64];
    let len = file.read_at(0, &mut buf).unwrap();
    String::from_utf8(buf[..len].to_vec()).unwrap()
}

#[test]
fn a_number_is_read_as_kstrtoint_reads_it() {
    const EINVAL: Result<i32> = Err(FsError::InvalidParam);
    const ERANGE: Result<i32> = Err(FsError::OutOfRange);
    for (text, want) in [
        (&b"0x10"[..], Ok(16)),
        (b"0X1f", Ok(31)),
        (b"010", Ok(8)),
        (b" 7 ", Ok(7)),
        (b"+9", Ok(9)),
        (b"-0", Ok(0)),
        (b"5\n\n", Ok(5)),
        (b"\x0b12\x0b", Ok(12)),
        (b"\xa013", Ok(13)),
        (b"11\0xyz", Ok(11)),
        (b"-2147483648", Ok(i32::MIN)),
        (b"-+5", EINVAL),
        (b"+-5", EINVAL),
        (b"6 x", EINVAL),
        (b"", EINVAL),
        (b"\n", EINVAL),
        (b"1e3", EINVAL),
        (b"0x", EINVAL),
        (b"08", EINVAL),
        (b"99999999999", ERANGE),
        (b"4294967296", ERANGE),
        (b"2147483648", ERANGE),
        (b"-2147483649", ERANGE),
        // Past a `u64` the overflow is said before the junk after it.
        (b"99999999999999999999999x", ERANGE),
    ] {
        assert_eq!(kstrtoint(text), want, "{:?}", core::str::from_utf8(text));
    }
}

#[test]
fn a_write_reads_its_first_twelve_bytes_and_says_it_took_them() {
    let proc = a_process(49_541);
    let write = |text: &[u8]| write_as(&proc, text, false, true);
    assert_eq!(write(b"14                  "), Ok(12));
    assert_eq!(read(proc.id(), "oom_score_adj"), "14\n");
    assert_eq!(write(b"            15"), Err(FsError::InvalidParam));
    assert_eq!(write(b"500\n"), Ok(4));
    assert_eq!(read(proc.id(), "oom_score_adj"), "500\n");
    for junk in [&b"1001"[..], b"-1001", b"x"] {
        assert_eq!(write(junk), Err(FsError::InvalidParam));
    }
    assert_eq!(
        read(proc.id(), "oom_score_adj"),
        "500\n",
        "and none of those moved it"
    );
}

#[test]
fn the_old_file_is_the_same_value_in_seventeenths() {
    let proc = a_process(49_542);
    for (old, adj) in [
        (0, 0),
        (1, 58),
        (8, 470),
        (14, 823),
        (15, 1000),
        (-16, -941),
        (-17, -1000),
    ] {
        let text = alloc::format!("{}", old);
        assert_eq!(write_as(&proc, text.as_bytes(), true, true), Ok(text.len()));
        assert_eq!(proc.linux().oom_adj().adj, adj, "oom_adj {}", old);
    }
    for out in [&b"16"[..], b"-18"] {
        assert_eq!(write_as(&proc, out, true, true), Err(FsError::InvalidParam));
    }
    for (adj, old) in [
        (58, 0),
        (59, 1),
        (117, 1),
        (941, 15),
        (942, 15),
        (999, 15),
        (1000, 15),
        (-941, -15),
        (-1000, -17),
    ] {
        assert!(proc.linux().set_oom_adj(adj, false, true));
        assert_eq!(
            read(proc.id(), "oom_adj"),
            alloc::format!("{}\n", old),
            "{}",
            adj
        );
    }
}

#[test]
fn a_writer_without_the_capability_is_refused_below_what_it_may_reach() {
    let proc = a_process(49_543);
    assert_eq!(write_as(&proc, b"100", false, false), Ok(3));
    assert_eq!(write_as(&proc, b"50", false, false), Ok(2));
    assert_eq!(
        write_as(&proc, b"-1", false, false),
        Err(FsError::NoPermission)
    );
    // 0 is above the floor, and the old file still may not go below 50.
    assert_eq!(
        write_as(&proc, b"0", true, false),
        Err(FsError::NoPermission)
    );
    assert_eq!(read(proc.id(), "oom_score_adj"), "50\n");
}

#[test]
fn the_score_is_the_share_of_memory_held_plus_the_adjustment() {
    let total = 131_072; // 512 MiB of 4 KiB pages
    for (adj, held, want) in [
        (0, 0, 666),
        (500, 0, 999),
        (1000, 0, 1332),
        (0, total, 1333),
        // Divided toward zero, as C divides.
        (-500, 0, 334),
        (-999, 0, 1),
        (-1000, total, 0),
    ] {
        assert_eq!(
            score(2, adj, held, total),
            want,
            "adj {} held {}",
            adj,
            held
        );
    }
    assert_eq!(
        score(INIT_PID, 1000, total, total),
        0,
        "init is never picked"
    );
    assert_eq!(score(2, 0, 3, 0), (1000 + 3000) * 2 / 3, "no memory at all");
}

#[test]
fn the_three_files_are_there_and_a_zombie_scores_nothing() {
    let parent = a_process(49_544);
    Thread::create_linux(&parent).unwrap();
    let child = Process::fork_from(&parent).unwrap();
    let pid = child.id();
    assert!(child.linux().set_oom_adj(1000, false, true));
    assert_eq!(read(pid, "oom_score_adj"), "1000\n");
    assert_eq!(read(pid, "oom_adj"), "15\n");
    let alive: u64 = read(pid, "oom_score").trim().parse().unwrap();
    assert!((1332..=2000).contains(&alive), "{}", alive);
    child.exit(0);
    assert_eq!(read(pid, "oom_score"), "0\n", "no memory left to count");
    assert_eq!(
        read(pid, "oom_score_adj"),
        "1000\n",
        "but `signal` is still there"
    );
}

#[test]
fn the_files_are_written_through_the_directory() {
    let proc = a_process(49_545);
    let dir = ProcPidDirINode { pid: proc.id() };
    let file = dir.find("oom_score_adj").unwrap();
    file.resize(0).unwrap();
    assert_eq!(file.write_at(0, b"-250\n"), Ok(5));
    assert_eq!(read(proc.id(), "oom_score_adj"), "-250\n");
    let old = dir.find("oom_adj").unwrap();
    assert_eq!(old.write_at(0, b"15"), Ok(2));
    assert_eq!(read(proc.id(), "oom_score_adj"), "1000\n");
    let score = dir.find("oom_score").unwrap();
    assert_eq!(score.write_at(0, b"1"), Err(FsError::NotSupported));
    assert!(score.resize(0).is_err());
    for name in ["auxv", "oom_score_adj", "oom_adj", "oom_score"] {
        assert!(ProcPidDirINode::entries().contains(&name), "{}", name);
    }
    for (name, mode, write) in [
        ("oom_score_adj", 0o644, true),
        ("oom_adj", 0o644, true),
        ("oom_score", 0o444, false),
        ("auxv", 0o400, false),
    ] {
        let file = dir.find(name).unwrap();
        assert_eq!(file.metadata().unwrap().mode, mode, "{}", name);
        assert_eq!(file.poll().unwrap().write, write, "{}", name);
    }
}

#[test]
fn the_memory_a_process_holds_counts() {
    // 2 MiB of the host's 1 GiB is one thousandth of a point and a bit:
    // enough to lift a process of no adjustment off 666.
    use zircon_object::vm::{MMUFlags, VmObject, PAGE_SIZE};
    let proc = a_process(49_546);
    let pages = 512;
    let flags = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
    let at = proc
        .vmar()
        .map(
            None,
            VmObject::new_paged(pages),
            0,
            pages * PAGE_SIZE,
            flags,
        )
        .unwrap();
    for page in 0..pages {
        proc.vmar()
            .write_memory(at + page * PAGE_SIZE, &[1])
            .unwrap();
    }
    let held: u64 = read(proc.id(), "oom_score").trim().parse().unwrap();
    assert!(held > 666, "{}", held);
}
