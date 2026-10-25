//! Fixed pids live in 45_45x here.

use super::*;
use crate::process::{wait_child, LinuxProcess, ProcessExt};
use crate::thread::ThreadExt;
use async_std::task::block_on;
use rcore_fs_ramfs::RamFS;
use zircon_object::vm::{MMUFlags, VmObject, PAGE_SIZE};

const BASE: usize = 0x10_0000;
const ARGS: &[u8] = b"sh\0-c\0echo hola\0";
const ENVS: &[u8] = b"PATH=/bin\0HOME=/\0";

/// A stretch of memory at [`BASE`], the arguments and then the environment,
/// as the loader lays them out.
struct Memory(Vec<u8>);

impl Memory {
    fn new() -> Self {
        Memory([ARGS, ENVS].concat())
    }

    fn read(&self, addr: usize, buf: &mut [u8]) -> usize {
        let Some(off) = addr.checked_sub(BASE).filter(|&off| off < self.0.len()) else {
            return 0;
        };
        let n = buf.len().min(self.0.len() - off);
        buf[..n].copy_from_slice(&self.0[off..off + n]);
        n
    }

    fn write(&mut self, at: usize, bytes: &[u8]) {
        self.0[at..at + bytes.len()].copy_from_slice(bytes);
    }
}

fn area() -> ArgArea {
    ArgArea {
        arg_start: BASE,
        arg_end: BASE + ARGS.len(),
        env_start: BASE + ARGS.len(),
        env_end: BASE + ARGS.len() + ENVS.len(),
    }
}

fn cmdline(mem: &Memory, area: ArgArea, pos: usize) -> Vec<u8> {
    let mut buf = vec![0u8; 8192];
    let n = cmdline_at(&|a, b| mem.read(a, b), area, pos, &mut buf);
    buf.truncate(n);
    buf
}

fn environ(mem: &Memory, area: ArgArea, pos: usize) -> Vec<u8> {
    let mut buf = vec![0u8; 8192];
    let n = environ_at(&|a, b| mem.read(a, b), area, pos, &mut buf);
    buf.truncate(n);
    buf
}

#[test]
fn the_arguments_and_the_environment_are_read_where_they_are() {
    let mem = Memory::new();
    assert_eq!(cmdline(&mem, area(), 0), ARGS);
    assert_eq!(environ(&mem, area(), 0), ENVS);
    assert_eq!(cmdline(&mem, area(), 3), b"-c\0echo hola\0");
    assert_eq!(environ(&mem, area(), 10), b"HOME=/\0");
    assert_eq!(cmdline(&mem, area(), ARGS.len()), b"");
    assert_eq!(cmdline(&mem, area(), ARGS.len() + 1), b"");
    assert_eq!(environ(&mem, area(), ENVS.len()), b"");
    let mut short = [0u8; 4];
    assert_eq!(cmdline_at(&|a, b| mem.read(a, b), area(), 0, &mut short), 4);
    assert_eq!(&short, b"sh\0-");
}

#[test]
fn an_argument_written_over_in_place_is_shown_as_it_is_now() {
    let mut mem = Memory::new();
    mem.write(0, b"ZZ");
    assert_eq!(cmdline(&mem, area(), 0), b"ZZ\0-c\0echo hola\0");
}

#[test]
fn a_title_over_the_last_nul_runs_on_into_the_environment_up_to_its_own() {
    // `setproctitle`: the title is longer than the arguments were, so it
    // covers their last NUL and goes on into the environment.
    let mut mem = Memory::new();
    mem.write(0, b"a much longer title\0");
    assert_eq!(cmdline(&mem, area(), 0), b"a much longer title\0");
    assert_eq!(cmdline(&mem, area(), 7), b"longer title\0");
    assert_eq!(cmdline(&mem, area(), 20), b"");
}

#[test]
fn a_title_stays_within_the_arguments_when_the_environment_is_elsewhere() {
    let mut mem = Memory::new();
    mem.write(0, b"a much longer title\0");
    let elsewhere = ArgArea {
        env_start: BASE + 0x800,
        env_end: BASE + 0x900,
        ..area()
    };
    assert_eq!(cmdline(&mem, elsewhere, 0), b"a much longer ti");
    let backwards = ArgArea {
        env_end: BASE,
        ..area()
    };
    assert_eq!(cmdline(&mem, backwards, 0), b"a much longer ti");
}

#[test]
fn a_title_with_no_nul_is_one_page_of_it() {
    let mut mem = Memory::new();
    mem.0.resize(ARGS.len() + 6000, b'e');
    let long = ArgArea {
        env_end: BASE + ARGS.len() + 6000,
        ..area()
    };
    let title = vec![b'T'; mem.0.len()];
    mem.write(0, &title);
    assert_eq!(cmdline(&mem, long, 0), vec![b'T'; TITLE_PAGE]);
    assert_eq!(cmdline(&mem, long, TITLE_PAGE - 1), b"T");
    assert_eq!(cmdline(&mem, long, TITLE_PAGE), b"");
}

#[test]
fn no_arguments_is_an_empty_file() {
    let mem = Memory::new();
    // Past `sh`, whose last byte is not a NUL, so a missing check would
    // take the empty arguments for a title.
    let none = ArgArea {
        arg_start: BASE + 2,
        arg_end: BASE + 2,
        env_start: BASE + 2,
        ..area()
    };
    assert_eq!(cmdline(&mem, none, 0), b"");
}

/// A process with [`ARGS`] and [`ENVS`] in its memory and [`area`] saying
/// where, and a different copy kept aside as if from `execve`.
fn a_process(pid: u64) -> Arc<Process> {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "argv",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    Thread::create_linux(&proc).unwrap();
    let flags = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
    let page = proc
        .vmar()
        .map(Some(BASE), VmObject::new_paged(1), 0, PAGE_SIZE, flags);
    assert_eq!(page, Ok(BASE));
    let strings = Memory::new().0;
    assert_eq!(proc.vmar().write_memory(BASE, &strings), Ok(strings.len()));
    let lp = proc.linux();
    lp.set_cmdline(alloc::vec![String::from("kept")]);
    lp.set_environ(alloc::vec![String::from("KEPT=1")]);
    lp.set_arg_area(Some(area()));
    proc
}

fn read_file(pid: u64, name: &str, pos: usize) -> Vec<u8> {
    let inode = ProcPidDirINode { pid }.find(name).unwrap();
    let mut buf = vec![0u8; 8192];
    let n = inode.read_at(pos, &mut buf).unwrap();
    buf.truncate(n);
    buf
}

#[test]
fn cmdline_and_environ_are_the_process_s_memory_not_the_copy() {
    let proc = a_process(45_451);
    let pid = proc.id();
    assert_eq!(read_file(pid, "cmdline", 0), ARGS);
    assert_eq!(read_file(pid, "cmdline", 3), b"-c\0echo hola\0");
    assert_eq!(read_file(pid, "environ", 0), ENVS);
    proc.vmar().write_memory(BASE, b"ZZ").unwrap();
    assert_eq!(read_file(pid, "cmdline", 0), b"ZZ\0-c\0echo hola\0");
    let kind = ProcPidFileKind::Cmdline;
    assert_eq!(
        ProcPidFileINode { pid, kind }.bytes().unwrap(),
        b"ZZ\0-c\0echo hola\0"
    );
    let kind = ProcPidFileKind::Environ;
    assert_eq!(ProcPidFileINode { pid, kind }.bytes().unwrap(), ENVS);

    // No area recorded: the copy kept at `execve` stands in.
    proc.linux().set_arg_area(None);
    assert_eq!(read_file(pid, "cmdline", 0), b"kept\0");
    assert_eq!(read_file(pid, "cmdline", 2), b"pt\0");
    assert_eq!(read_file(pid, "environ", 0), b"KEPT=1\0");
    proc.exit(0);
}

#[test]
fn a_zombie_s_cmdline_and_environ_are_empty() {
    let parent = a_process(45_452);
    let child = Process::fork_from(&parent).unwrap();
    let pid = child.id();
    assert_eq!(child.linux().arg_area(), Some(area()));
    // With a thread still on its way out the address space is still there
    // (it goes with the last thread), so the file is empty because the
    // process is a zombie, not because there is nothing left to read.
    let _thread = Thread::create_linux(&child).unwrap();
    child.exit(0);
    let mut first = [0u8; 2];
    assert_eq!(child.vmar().read_memory(BASE, &mut first), Ok(2));
    assert_eq!(&first, b"sh");
    drop(child);
    assert_eq!(read_file(pid, "cmdline", 0), b"");
    assert_eq!(read_file(pid, "environ", 0), b"");
    block_on(wait_child(&parent, pid, true, true)).unwrap();
    parent.exit(0);
}

/// What a loader hands a process: two pairs, `AT_NULL`'s among them.
const AUXV: [usize; 6] = [6, 4096, 25, 0x7fff_e800, 0, 0];

fn auxv_bytes() -> Vec<u8> {
    AUXV.iter().flat_map(|w| w.to_ne_bytes()).collect()
}

#[test]
fn auxv_is_the_vector_the_image_was_handed_and_a_fork_s_too() {
    let parent = a_process(49_551);
    let pid = parent.id();
    assert_eq!(read_file(pid, "auxv", 0), b"", "no loader said");
    parent.linux().set_auxv(AUXV[..].into());
    assert_eq!(read_file(pid, "auxv", 0), auxv_bytes());
    assert_eq!(read_file(pid, "auxv", 40), &auxv_bytes()[40..]);
    let child = Process::fork_from(&parent).unwrap();
    assert_eq!(read_file(child.id(), "auxv", 0), auxv_bytes());
    child.exit(0);
    parent.exit(0);
}

#[test]
fn a_zombie_and_a_leader_gone_first_have_no_auxv() {
    let parent = a_process(49_552);
    parent.linux().set_auxv(AUXV[..].into());
    let child = Process::fork_from(&parent).unwrap();
    let pid = child.id();
    let _thread = Thread::create_linux(&child).unwrap();
    child.exit(0);
    assert_eq!(read_file(pid, "auxv", 0), b"", "a zombie");

    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        49_553,
        "lider",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    proc.linux().set_auxv(AUXV[..].into());
    let leader = Thread::create_linux(&proc).unwrap();
    let _worker = Thread::create_linux(&proc).unwrap();
    assert_eq!(read_file(proc.id(), "auxv", 0), auxv_bytes());
    proc.linux().keep_exited_leader(leader.clone());
    leader.terminate_abandoned();
    assert_eq!(read_file(proc.id(), "auxv", 0), b"", "its leader exited");
    drop(proc.linux().release_exited_leader());
    proc.exit(0);
    parent.exit(0);
}
