use super::*;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use zircon_object::vm::{MMUFlags, VmObject};

fn seg(exec: bool, vaddr: u64, filesz: u64) -> Segment {
    Segment {
        exec,
        vaddr,
        filesz,
    }
}

/// A position-independent program as `ld` lays it out: headers, code,
/// read-only data, then data. Linux takes the code range from the
/// executable segment alone and the data range from all four, both
/// ending at the file bytes (`p_filesz`), and adds the load bias.
#[test]
fn the_code_is_the_executable_segment_and_the_data_ends_the_highest() {
    let segments = [
        seg(false, 0x0, 0x6a8),
        seg(true, 0x1000, 0x9876),
        seg(false, 0xb000, 0x1d4),
        seg(false, 0xcde8, 0x2c0),
    ];
    let bias = 0x5555_5555_4000;
    let l = ImageLayout::of_segments(
        segments.iter().copied(),
        bias,
        0x5555_5556_8000,
        0x7fff_e6f0,
    );
    assert_eq!(l.start_code, bias + 0x1000);
    assert_eq!(l.end_code, bias + 0xa876);
    assert_eq!(l.start_data, bias + 0xcde8);
    assert_eq!(l.end_data, bias + 0xd0a8);
    assert_eq!(l.start_brk, 0x5555_5556_8000);
    assert_eq!(l.start_stack, 0x7fff_e6f0);
}

/// The segments need not come in address order, and more than one may be
/// executable: the bounds are the lowest start and the highest end of any.
#[test]
fn the_bounds_are_the_extremes_whatever_the_order() {
    let segments = [
        seg(true, 0x8000, 0x100),
        seg(false, 0x9000, 0x2000),
        seg(true, 0x2000, 0x10),
        seg(false, 0x1000, 0x10),
    ];
    let l = ImageLayout::of_segments(segments.iter().copied(), 0, 0, 0);
    assert_eq!((l.start_code, l.end_code), (0x2000, 0x8100));
    assert_eq!((l.start_data, l.end_data), (0x9000, 0xb000));
}

/// The ends are the file's bytes: a segment's `.bss` is past `end_data`.
#[test]
fn the_ends_are_where_the_file_bytes_end() {
    let l = ImageLayout::of_segments([seg(true, 0x40_0000, 0x10)].iter().copied(), 0, 0, 0);
    assert_eq!((l.start_code, l.end_code), (0x40_0000, 0x40_0010));
    assert_eq!((l.start_data, l.end_data), (0x40_0000, 0x40_0010));
}

/// An image with no executable segment cannot run; it has no code range.
#[test]
fn no_executable_segment_is_no_code() {
    let l = ImageLayout::of_segments([seg(false, 0x1000, 0x10)].iter().copied(), 0x10_0000, 0, 0);
    assert_eq!((l.start_code, l.end_code), (0, 0));
    assert_eq!(l.start_data, 0x10_1000);
}

/// `execve` knows the path `/proc/<pid>/exe` shows only after the load:
/// each named row takes the path the resolver gives its name, asked once
/// per name, and the `[vdso]` and anonymous rows are left alone.
#[test]
fn renaming_gives_each_named_row_the_path_its_name_resolves_to() {
    let vmar = VmAddressRegion::new_root();
    let rw = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
    // At fixed addresses and not `None`: a Linux root VMAR starts at 0, so
    // the first free area is page 0, and under libos that is a host
    // `MAP_FIXED` at 0, which only root may do. It passed as root and failed
    // for anyone else (`EPERM`), as CI runs it.
    let next = core::cell::Cell::new(0x1000_0000);
    let map = |vmo: Arc<VmObject>| {
        let at = next.replace(next.get() + 2 * PAGE_SIZE);
        vmar.map(Some(at), vmo, 0, PAGE_SIZE, rw).unwrap()
    };
    let named = |name: &str| {
        let vmo = VmObject::new_paged(1);
        vmo.set_name(name);
        map(vmo)
    };
    let text = named("./survey");
    let data = named("./survey");
    let interp = named("/lib64/ld-linux-x86-64.so.2");
    let same = named("/lib/ld-musl-x86_64.so.1");
    let vdso = named("[vdso]");
    let anon = map(VmObject::new_paged(1));

    let asked = core::cell::RefCell::new(Vec::new());
    rename_images(&vmar, |name| {
        asked.borrow_mut().push(String::from(name));
        String::from(match name {
            "./survey" => "/tmp/survey",
            "/lib64/ld-linux-x86-64.so.2" => "/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
            other => other,
        })
    });

    let names: Vec<(usize, String)> = vmar
        .mappings_dump()
        .into_iter()
        .map(|m| (m.start, m.name))
        .collect();
    let name_at = |at| names.iter().find(|(s, _)| *s == at).unwrap().1.as_str();
    assert_eq!(name_at(text), "/tmp/survey");
    assert_eq!(name_at(data), "/tmp/survey");
    assert_eq!(
        name_at(interp),
        "/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2"
    );
    assert_eq!(name_at(same), "/lib/ld-musl-x86_64.so.1");
    assert_eq!(name_at(vdso), "[vdso]");
    assert_eq!(name_at(anon), "");
    let mut asked = asked.into_inner();
    asked.sort();
    assert_eq!(
        asked,
        [
            "./survey",
            "/lib/ld-musl-x86_64.so.1",
            "/lib64/ld-linux-x86-64.so.2"
        ]
    );
}
