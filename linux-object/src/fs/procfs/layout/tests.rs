//! Fixed pids live in 49_40x here.

use super::*;
use crate::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::vm::{MMUFlags, VmObject, PAGE_SIZE};

const LAYOUT: ImageLayout = ImageLayout {
    start_code: 0x40_1000,
    end_code: 0x40_9876,
    start_data: 0x60_c000,
    end_data: 0x60_d0a8,
    start_brk: 0x10_0000_0000,
    start_stack: 0x7fff_ffff_e6f0,
};

const ARGS: ArgArea = ArgArea {
    arg_start: 0x7fff_ffff_ec1d,
    arg_end: 0x7fff_ffff_ec32,
    env_start: 0x7fff_ffff_ec32,
    env_end: 0x7fff_ffff_efe2,
};

fn mm(brk: usize) -> Mm {
    Mm {
        layout: Some(LAYOUT),
        args: Some(ARGS),
        brk,
    }
}

fn filled(mm: Option<Mm>, permitted: bool) -> [i64; 48] {
    let mut rest = [0i64; 48];
    fill_stat(&mut rest, mm, permitted);
    rest
}

fn field(rest: &[i64; 48], n: usize) -> i64 {
    rest[n - 5]
}

fn row(start: usize, end: usize, name: &str) -> MappingDump {
    MappingDump {
        start,
        end,
        flags: MMUFlags::READ | MMUFlags::WRITE,
        vmo_offset: 0,
        file_offset: 0,
        shared: false,
        vmo_id: 1,
        name: String::from(name),
    }
}

/// Each of the ten in its own field, in the order `do_task_stat` writes
/// them: `ps -o start_stack` and every reader that finds a process's code or
/// heap from here read 0 for all of them.
#[test]
fn a_reader_that_may_look_gets_the_ten_addresses_in_their_fields() {
    let rest = filled(Some(mm(LAYOUT.start_brk)), true);
    assert_eq!(field(&rest, 26), 0x40_1000);
    assert_eq!(field(&rest, 27), 0x40_9876);
    assert_eq!(field(&rest, 28), 0x7fff_ffff_e6f0);
    assert_eq!(field(&rest, 45), 0x60_c000);
    assert_eq!(field(&rest, 46), 0x60_d0a8);
    assert_eq!(field(&rest, 47), 0x10_0000_0000);
    assert_eq!(field(&rest, 48), 0x7fff_ffff_ec1d);
    assert_eq!(field(&rest, 49), 0x7fff_ffff_ec32);
    assert_eq!(field(&rest, 50), 0x7fff_ffff_ec32);
    assert_eq!(field(&rest, 51), 0x7fff_ffff_efe2);
    let others: Vec<usize> = (5..=52)
        .filter(|n| !matches!(n, 26..=28 | 45..=51))
        .filter(|&n| field(&rest, n) != 0)
        .collect();
    assert!(others.is_empty(), "wrote fields {:?} too", others);
}

/// `mm ? (permitted ? mm->start_code : 1) : 0`, and the other eight only
/// with both: another user's process says it has code, not where its stack
/// is (which ASLR hides).
#[test]
fn a_reader_that_may_not_look_learns_only_that_there_is_code() {
    let rest = filled(Some(mm(LAYOUT.start_brk)), false);
    assert_eq!(field(&rest, 26), 1);
    assert_eq!(field(&rest, 27), 1);
    for n in [28, 45, 46, 47, 48, 49, 50, 51] {
        assert_eq!(field(&rest, n), 0, "field {}", n);
    }
}

/// A zombie has no `mm`, so all ten are 0 whoever asks.
#[test]
fn without_memory_all_ten_are_zero() {
    assert_eq!(filled(None, true), [0i64; 48]);
    assert_eq!(filled(None, false), [0i64; 48]);
}

/// A process no loader started has an `mm` and no record of it.
#[test]
fn a_process_with_no_record_reads_zero_where_it_may_look() {
    assert_eq!(filled(Some(Mm::default()), true), [0i64; 48]);
}

/// `vma_is_initial_heap`: `vm_start < brk && vm_end > start_brk`, both
/// strict, so a heap that has not grown has no row and the rows on either
/// side of it are not the heap.
#[test]
fn the_heap_is_the_anonymous_row_over_the_breaks_range() {
    let b = LAYOUT.start_brk;
    let grown = mm(b + 0x21000);
    let name = |start, end, mm: &Mm| String::from(row_name(&row(start, end, ""), Some(mm)));
    assert_eq!(name(b, b + 0x22000, &grown), "[heap]");
    // The break's reservation can run past the break; the row is still the heap.
    assert_eq!(name(b, b + 0x100000, &grown), "[heap]");
    // A row ending where the heap starts, and one starting where it ends.
    assert_eq!(name(b - 0x1000, b, &grown), "");
    assert_eq!(name(b + 0x21000, b + 0x22000, &grown), "");
    // Nothing grown yet: no row is the heap.
    assert_eq!(name(b, b + 0x1000, &mm(b)), "");
}

/// `vma_is_initial_stack`: the row holding `start_stack`, ends included.
#[test]
fn the_stack_is_the_anonymous_row_holding_the_first_stack_pointer() {
    let s = LAYOUT.start_stack;
    let m = mm(LAYOUT.start_brk);
    let name = |start, end| String::from(row_name(&row(start, end, ""), Some(&m)));
    assert_eq!(name(s - 0x80_0000, s + 0x1910), "[stack]");
    assert_eq!(name(s - 0x1000, s), "[stack]");
    assert_eq!(name(s, s + 0x1000), "[stack]");
    assert_eq!(name(s + 1, s + 0x1000), "");
    assert_eq!(name(s - 0x2000, s - 1), "");
}

/// A file mapping keeps its path, wherever it is: only anonymous rows are
/// named after what they hold.
#[test]
fn a_named_row_keeps_its_name() {
    let m = mm(LAYOUT.start_brk + 0x1000);
    let s = LAYOUT.start_stack;
    let b = LAYOUT.start_brk;
    assert_eq!(
        row_name(&row(b, b + 0x1000, "/memfd:x"), Some(&m)),
        "/memfd:x"
    );
    assert_eq!(row_name(&row(s - 0x1000, s + 0x1000, "/x"), Some(&m)), "/x");
}

/// No record, no `mm`: nothing to call a row after.
#[test]
fn without_a_record_no_row_is_named() {
    let s = LAYOUT.start_stack;
    let b = LAYOUT.start_brk;
    let none = Mm {
        brk: b + 0x1000,
        ..Mm::default()
    };
    assert_eq!(row_name(&row(b, b + 0x1000, ""), Some(&none)), "");
    assert_eq!(row_name(&row(s - 0x1000, s + 0x1000, ""), Some(&none)), "");
    assert_eq!(row_name(&row(b, b + 0x1000, ""), None), "");
}

fn a_process(pid: u64) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "layout", LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

fn stat_field(stat: &str, n: usize) -> i64 {
    let after = &stat[stat.rfind(')').unwrap() + 2..];
    after.split(' ').nth(n - 3).unwrap().trim().parse().unwrap()
}

/// The whole way through: what the loader recorded is what `stat` prints.
#[test]
fn stat_prints_what_the_loader_recorded() {
    let proc = a_process(49_401);
    proc.linux().set_image_layout(Some(LAYOUT));
    proc.linux().set_arg_area(Some(ARGS));
    let stat = proc_pid_stat(&proc);
    assert_eq!(stat_field(&stat, 26), 0x40_1000, "{}", stat);
    assert_eq!(stat_field(&stat, 28), 0x7fff_ffff_e6f0, "{}", stat);
    assert_eq!(stat_field(&stat, 47), 0x10_0000_0000, "{}", stat);
    assert_eq!(stat_field(&stat, 51), 0x7fff_ffff_efe2, "{}", stat);
    proc.exit(0);
    let stat = proc_pid_stat(&proc);
    assert_eq!(
        stat_field(&stat, 26),
        0,
        "a zombie still had code: {}",
        stat
    );
}

/// And what `maps` names: an anonymous row over the break and one over the
/// first stack pointer, in a real address space.
#[test]
fn maps_names_the_heap_and_the_stack() {
    let proc = a_process(49_402);
    let rw = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
    let map = |pages: usize| {
        proc.vmar()
            .map(None, VmObject::new_paged(pages), 0, pages * PAGE_SIZE, rw)
            .unwrap()
    };
    let heap = map(4);
    let stack = map(2);
    let other = map(1);
    proc.linux().set_image_layout(Some(ImageLayout {
        start_brk: heap,
        start_stack: stack + 0x10f0,
        ..ImageLayout::default()
    }));
    proc.linux().set_brk(heap + 0x2010);
    let maps = proc_pid_maps(&proc);
    let name_of = |at: usize| {
        let line = maps
            .lines()
            .find(|l| l.starts_with(&alloc::format!("{:08x}-", at)))
            .unwrap_or_else(|| panic!("no row at {:#x} in\n{}", at, maps));
        String::from(line.split_whitespace().nth(5).unwrap_or(""))
    };
    assert_eq!(name_of(heap), "[heap]", "{}", maps);
    assert_eq!(name_of(stack), "[stack]", "{}", maps);
    assert_eq!(name_of(other), "", "{}", maps);
}
