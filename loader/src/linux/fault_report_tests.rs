//! The fault report: what the kernel says about the address a process died
//! on, and about the stack it died with.
//!
//! The four helpers that build it -- [`describe_addr`],
//! [`describe_fault_perms`], [`addr_is_executable`] and
//! [`user_stack_backtrace`] -- had no test. What did have tests were the
//! three CONSTANTS that bound the scan (`BACKTRACE_SCAN_WORDS`,
//! `BACKTRACE_SCAN_BYTES`, `BACKTRACE_MAX_FRAMES`), and a bound that holds
//! says nothing about the walk that is supposed to respect it.
//!
//! All four take an address space and an address, so a bare
//! `VmAddressRegion` with a page or two mapped into it is the whole
//! fixture: no process, no thread, no scheduler. That is also why the
//! report is worth a test at all -- it is only ever built on the path where
//! the process is already dying, so a wrong line here is never noticed by
//! anything but the person reading the crash.

use super::*;
use kernel_hal::MMUFlags;
use zircon_object::vm::{VmObject, PAGE_SIZE};

/// A user mapping of data: readable, writable, not executable.
const RW: MMUFlags = MMUFlags::READ.union(MMUFlags::WRITE).union(MMUFlags::USER);

/// A user mapping of code: readable, executable, not writable.
const RX: MMUFlags = MMUFlags::READ
    .union(MMUFlags::EXECUTE)
    .union(MMUFlags::USER);

/// Width of the words the backtrace scan walks in.
const WORD: usize = core::mem::size_of::<usize>();

/// A root address space that takes its mappings down before it is dropped.
///
/// A root VMAR hands its hosted address-space window back in its `Drop`,
/// and `Drop::drop` runs BEFORE the fields it holds -- the mappings -- are
/// dropped. So the window can be handed to another thread while this one's
/// pages are still mapped in the host, and the new owner's `mmap` of them
/// races our `munmap`: the mock memory then panics with
/// `mprotect(EXECUTE) right after mmap found no mapping`. One red round in
/// twelve at `--test-threads=32`, a different test each time.
///
/// Tearing the address space down inside the test closes that window, and
/// it is what the rest of the tree's tests do already -- by way of
/// `Process::exit`, which every test that borrows a process ends with. Here
/// there is no process, so the teardown is the fixture's own.
struct Aspace(Arc<VmAddressRegion>);

impl Drop for Aspace {
    fn drop(&mut self) {
        self.0.destroy().expect("the root is ours and still usable");
    }
}

impl core::ops::Deref for Aspace {
    type Target = Arc<VmAddressRegion>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// An empty address space of its own, for one test.
fn aspace() -> Aspace {
    Aspace(VmAddressRegion::new_root())
}

/// Map `pages` pages of a VMO `vmo_pages` long, starting `vmo_offset` bytes
/// into it, naming the VMO unless `name` is empty. Returns where it landed.
///
/// `vmo_offset` is what makes the file offset of the mapping differ from the
/// offset of a byte within it, which is the distinction the report is for.
fn map(
    vmar: &Arc<VmAddressRegion>,
    name: &str,
    vmo_pages: usize,
    vmo_offset: usize,
    pages: usize,
    flags: MMUFlags,
) -> usize {
    let vmo = VmObject::new_paged(vmo_pages);
    if !name.is_empty() {
        vmo.set_name(name);
    }
    vmar.map(None, vmo, vmo_offset, pages * PAGE_SIZE, flags)
        .expect("the mapping fits in a fresh address space")
}

/// One page of anonymous read-write memory, zeroed, to stand in for a stack.
fn a_stack(vmar: &Arc<VmAddressRegion>, pages: usize) -> usize {
    let at = map(vmar, "", pages, 0, pages, RW);
    let zeros = alloc::vec![0u8; pages * PAGE_SIZE];
    let n = vmar
        .write_memory(at, &zeros)
        .expect("the stack we just mapped is there");
    assert_eq!(n, zeros.len(), "the whole stack was cleared");
    at
}

/// Write `value` into the word at index `i` of the stack at `stack`.
fn put_word(vmar: &Arc<VmAddressRegion>, stack: usize, i: usize, value: usize) {
    let n = vmar
        .write_memory(stack + i * WORD, &value.to_ne_bytes())
        .expect("the stack is mapped");
    assert_eq!(n, WORD, "word {} landed whole", i);
}

/// Split a `perm=... pte=...` line into its two halves.
fn halves(line: &str) -> (&str, &str) {
    let rest = line
        .strip_prefix("perm=")
        .unwrap_or_else(|| panic!("the line opens with the record's permissions"));
    let mut it = rest.split(" pte=");
    let perm = it.next().expect("a record half");
    let pte = it.next().expect("a page-table half");
    assert_eq!(it.next(), None, "exactly two halves");
    (perm, pte)
}

/// A null dereference is the commonest fault there is, and the report names
/// it rather than calling it an address nobody mapped.
#[test]
fn a_null_fault_is_named_null_and_not_unmapped() {
    let vmar = aspace();
    let at = map(&vmar, "libc.so", 1, 0, 1, RX);
    assert_eq!(describe_addr(&vmar, 0), "null");
    assert_ne!(
        describe_addr(&vmar, at),
        "null",
        "and an address that is mapped is not null"
    );
}

/// The offset in the report is the offset into the mapping's FILE, not into
/// the mapping: it is handed to `addr2line -e`, and a mapping that does not
/// start at offset zero in its file -- every PT_LOAD of every shared library
/// but the first -- is the whole reason the column exists.
#[test]
fn an_address_in_a_mapping_is_named_by_its_file_offset() {
    let vmar = aspace();
    let at = map(&vmar, "libc.so", 2, PAGE_SIZE, 1, RX);
    assert_eq!(
        describe_addr(&vmar, at + 0x40),
        alloc::format!(
            "libc.so+{:#x} in map {:#x}-{:#x}",
            PAGE_SIZE + 0x40,
            at,
            at + PAGE_SIZE
        ),
        "the byte is 0x40 into the mapping and PAGE_SIZE+0x40 into the file"
    );
}

/// Anonymous memory has no name, and the report says so instead of leaving
/// the column empty.
#[test]
fn an_address_in_anonymous_memory_is_named_anon() {
    let vmar = aspace();
    let at = a_stack(&vmar, 1);
    assert_eq!(
        describe_addr(&vmar, at + 8),
        alloc::format!("anon+{:#x} in map {:#x}-{:#x}", 8, at, at + PAGE_SIZE)
    );
}

/// The mapping's end is one past its last byte, so the last byte is in it
/// and the byte after is not. An off-by-one either way sends the reader of a
/// crash to the wrong library.
#[test]
fn the_last_byte_of_a_mapping_is_in_it_and_the_next_one_is_not() {
    let vmar = aspace();
    let at = map(&vmar, "libc.so", 1, 0, 1, RX);
    assert!(
        describe_addr(&vmar, at + PAGE_SIZE - 1).starts_with("libc.so+"),
        "the last byte is inside: {}",
        describe_addr(&vmar, at + PAGE_SIZE - 1)
    );
    assert_eq!(describe_addr(&vmar, at + PAGE_SIZE), "unmapped");
    assert_eq!(describe_addr(&vmar, at - 1), "unmapped");
}

/// A fault on a page nothing mapped is reported as such, which is what
/// separates a wild pointer from a permission the mapping never carried.
#[test]
fn an_address_nobody_mapped_is_named_unmapped() {
    let vmar = aspace();
    let at = map(&vmar, "libc.so", 1, 0, 1, RX);
    assert_eq!(describe_addr(&vmar, at + 0x1000_0000), "unmapped");
}

/// The line carries the record's permissions and the page table's as two
/// separate halves. That is the point of it: an `ACCESS_DENIED` fault where
/// the record is missing the bit is a question about the `mmap` that built
/// the mapping, and one where the record allows it and the page table does
/// not is ours. The two have opposite fixes.
#[test]
fn a_mapped_page_reports_the_record_and_the_page_table_separately() {
    let vmar = aspace();
    let at = a_stack(&vmar, 1);
    let line = describe_fault_perms(&vmar, at);
    let (perm, pte) = halves(&line);
    assert!(
        perm.contains("WRITE"),
        "the record carries the write this mapping was made with: {}",
        line
    );
    assert!(
        !pte.is_empty(),
        "and the page table half is reported too: {}",
        line
    );
}

/// The two halves have to be able to DISAGREE, which is the only reason
/// there are two of them.
///
/// `a_mapped_page_reports_the_record_and_the_page_table_separately` above
/// reads a page that is mapped and present, so the record and the page
/// table agree and reading the record twice passes it -- which is what
/// mutation says: swapping the page-table half for a second read of the
/// record kept the whole file green.
///
/// A page the VMO has dropped is the case the line was added for. After a
/// `MADV_DONTNEED` or an `ftruncate` the record still promises the write the
/// `mmap` asked for and the page table has nothing, because the next access
/// is meant to fault a fresh page in. That is the shape of the bug the line
/// tells apart from a wrong `mprotect` -- a fault path that did not install
/// what the record promised -- and a reader who saw the record printed in
/// both halves would conclude the page table was right and go looking in the
/// wrong place.
#[test]
fn a_page_the_object_dropped_reports_a_record_the_page_table_does_not_have() {
    let vmar = aspace();
    let vmo = VmObject::new_paged(1);
    let at = vmar
        .map(None, vmo.clone(), 0, PAGE_SIZE, RW)
        .expect("the mapping fits in a fresh address space");
    let before = describe_fault_perms(&vmar, at);
    let (perm, pte) = halves(&before);
    assert_eq!(
        perm, pte,
        "while the page is there the two agree: {}",
        before
    );

    vmo.decommit(0, PAGE_SIZE)
        .expect("the page is ours to drop");

    let line = describe_fault_perms(&vmar, at);
    let (perm, pte) = halves(&line);
    assert!(
        perm.contains("WRITE"),
        "the record still carries the write the mapping was made with: {}",
        line
    );
    assert_ne!(
        perm, pte,
        "the page table half is the record read a second time: {}",
        line
    );
    assert!(
        !pte.contains("WRITE"),
        "the page is gone, so the page table grants nothing on it: {}",
        line
    );
}

/// On a page nobody mapped, neither half may claim an access right: a
/// reader who saw `perm=READ` there would go looking for a fault path that
/// dropped a page, and there is no page.
#[test]
fn a_hole_claims_no_access_right_in_either_half() {
    let vmar = aspace();
    let at = a_stack(&vmar, 1);
    let line = describe_fault_perms(&vmar, at + 0x1000_0000);
    for right in ["READ", "WRITE", "EXECUTE"] {
        assert!(
            !line.contains(right),
            "{} is not an access a hole carries: {}",
            right,
            line
        );
    }
    let (perm, pte) = halves(&line);
    assert_eq!(perm, pte, "both halves failed the same way: {}", line);
}

/// Executability is the test for whether a stack word can be a return
/// address, so writable data and unmapped holes have to answer no.
#[test]
fn only_a_mapped_executable_page_counts_as_executable() {
    let vmar = aspace();
    let code = map(&vmar, "libc.so", 1, 0, 1, RX);
    let stack = a_stack(&vmar, 1);
    assert!(addr_is_executable(&vmar, code), "code is executable");
    assert!(!addr_is_executable(&vmar, stack), "a stack is not");
    assert!(
        !addr_is_executable(&vmar, code + 0x1000_0000),
        "and neither is a hole"
    );
}

/// Per PAGE, not per mapping: a partial `mprotect` leaves mixed permissions
/// inside one mapping, and the first page's flags are not the mapping's.
/// This is the shape musl builds every thread stack in.
#[test]
fn a_partial_mprotect_leaves_only_one_of_the_two_pages_executable() {
    let vmar = aspace();
    let code = map(&vmar, "libc.so", 2, 0, 2, RX);
    vmar.protect(code, PAGE_SIZE, RW)
        .expect("the mapping was made with RXW permissions");
    assert!(
        !addr_is_executable(&vmar, code),
        "the page mprotect took the execute bit off is not executable"
    );
    assert!(
        addr_is_executable(&vmar, code + PAGE_SIZE),
        "the one it did not touch still is"
    );
    let rows = vmar.mappings_dump();
    assert_eq!(
        rows.len(),
        2,
        "and the dump reports it as two rows, one per run of permissions"
    );
}

/// A frame is rendered with the word's position in the scan, the address
/// itself, and the name and FILE offset of the mapping it points into.
#[test]
fn a_frame_names_the_word_the_address_and_the_file_offset() {
    let vmar = aspace();
    let code = map(&vmar, "libc.so", 2, PAGE_SIZE, 1, RX);
    let stack = a_stack(&vmar, 1);
    put_word(&vmar, stack, 3, code + 0x40);
    assert_eq!(
        user_stack_backtrace(&vmar, stack),
        alloc::vec![alloc::format!(
            "stack[+{:#x}] = {:#x} [libc.so+{:#x}]",
            3 * WORD,
            code + 0x40,
            PAGE_SIZE + 0x40
        )],
        "one frame, at word three of the scan"
    );
}

/// An executable mapping with no name is rendered `anon`, like a data one.
#[test]
fn a_frame_in_unnamed_executable_memory_is_rendered_as_anon() {
    let vmar = aspace();
    let code = map(&vmar, "", 1, 0, 1, RX);
    let stack = a_stack(&vmar, 1);
    put_word(&vmar, stack, 0, code + 0x10);
    let trace = user_stack_backtrace(&vmar, stack);
    assert_eq!(trace.len(), 1, "the frame was found: {:?}", trace);
    assert!(
        trace[0].ends_with(alloc::format!("[anon+{:#x}]", 0x10).as_str()),
        "and named anon: {}",
        trace[0]
    );
}

/// A stack is full of data, not just return addresses. A word that points
/// at nothing, and a word that points at the stack itself, are both only
/// data: rendering either would bury the frames that are real.
#[test]
fn a_word_that_is_not_a_code_address_is_not_a_frame() {
    let vmar = aspace();
    let code = map(&vmar, "libc.so", 1, 0, 1, RX);
    let stack = a_stack(&vmar, 1);
    put_word(&vmar, stack, 0, code + 0x1000_0000);
    put_word(&vmar, stack, 1, stack + 0x80);
    put_word(&vmar, stack, 2, 7);
    assert!(
        user_stack_backtrace(&vmar, stack).is_empty(),
        "a hole, a pointer into the stack and a small integer are all data: {:?}",
        user_stack_backtrace(&vmar, stack)
    );
    put_word(&vmar, stack, 3, code);
    assert_eq!(
        user_stack_backtrace(&vmar, stack).len(),
        1,
        "and the one real return address among them is still found"
    );
}

/// A pointer into the page a partial `mprotect` took the execute bit off is
/// data too, even though its mapping began as code.
#[test]
fn a_word_pointing_into_the_unexecutable_half_of_a_mapping_is_not_a_frame() {
    let vmar = aspace();
    let code = map(&vmar, "libc.so", 2, 0, 2, RX);
    let stack = a_stack(&vmar, 1);
    vmar.protect(code, PAGE_SIZE, RW).expect("RXW permissions");
    put_word(&vmar, stack, 0, code + 0x10);
    put_word(&vmar, stack, 1, code + PAGE_SIZE + 0x10);
    let trace = user_stack_backtrace(&vmar, stack);
    assert_eq!(
        trace.len(),
        1,
        "only the word in the page that is still executable: {:?}",
        trace
    );
    assert!(
        trace[0].starts_with(alloc::format!("stack[+{:#x}]", WORD).as_str()),
        "the second word, not the first: {}",
        trace[0]
    );
}

/// `sub rsp, 3` before a fault is a legal state on x86-64, so an unaligned
/// stack pointer is aligned down rather than read as it stands: read as it
/// stands, every word in the scan comes out shifted and no frame matches.
#[test]
fn an_unaligned_stack_pointer_is_read_from_the_word_below_it() {
    let vmar = aspace();
    let code = map(&vmar, "libc.so", 1, 0, 1, RX);
    let stack = a_stack(&vmar, 1);
    put_word(&vmar, stack, 1, code + 0x10);
    let trace = user_stack_backtrace(&vmar, stack + WORD + 3);
    assert_eq!(trace.len(), 1, "the frame survives the odd sp: {:?}", trace);
    assert!(
        trace[0].starts_with("stack[+0x0]"),
        "and counts from the aligned word: {}",
        trace[0]
    );
}

/// The scan is bounded in bytes looked at, and the bound is a whole number
/// of words so the last one is not dropped. A word inside the budget is
/// found and the next one along is never read.
#[test]
fn the_scan_reads_the_last_word_of_its_budget_and_not_the_one_after() {
    let vmar = aspace();
    let code = map(&vmar, "libc.so", 1, 0, 1, RX);
    let stack = a_stack(&vmar, 2);
    put_word(&vmar, stack, BACKTRACE_SCAN_WORDS - 1, code + 0x10);
    let trace = user_stack_backtrace(&vmar, stack);
    assert_eq!(
        trace.len(),
        1,
        "the last budgeted word is read: {:?}",
        trace
    );
    assert!(
        trace[0].starts_with(
            alloc::format!("stack[+{:#x}]", (BACKTRACE_SCAN_WORDS - 1) * WORD).as_str()
        ),
        "at the position the budget ends on: {}",
        trace[0]
    );

    let vmar = aspace();
    let code = map(&vmar, "libc.so", 1, 0, 1, RX);
    let stack = a_stack(&vmar, 2);
    put_word(&vmar, stack, BACKTRACE_SCAN_WORDS, code + 0x10);
    assert!(
        user_stack_backtrace(&vmar, stack).is_empty(),
        "and the word after it is not: {:?}",
        user_stack_backtrace(&vmar, stack)
    );
}

/// The report is read on a serial console, so the frames are capped even
/// when the stack has more candidates than that.
#[test]
fn the_trace_stops_at_the_frames_a_console_can_show() {
    let vmar = aspace();
    let code = map(&vmar, "libc.so", 1, 0, 1, RX);
    let stack = a_stack(&vmar, 2);
    for i in 0..BACKTRACE_MAX_FRAMES + 6 {
        put_word(&vmar, stack, i, code + 0x10);
    }
    let trace = user_stack_backtrace(&vmar, stack);
    assert_eq!(trace.len(), BACKTRACE_MAX_FRAMES, "capped, not truncated");
    assert!(
        trace[BACKTRACE_MAX_FRAMES - 1].starts_with(
            alloc::format!("stack[+{:#x}]", (BACKTRACE_MAX_FRAMES - 1) * WORD).as_str()
        ),
        "and the last one kept is the last one it looked at: {}",
        trace[BACKTRACE_MAX_FRAMES - 1]
    );
}

/// The scan reads through the address space's VMOs, so it stops at the end
/// of the mapping `sp` is in instead of running off it. A stack overflow
/// that left `sp` one word below the top still gets its one word read.
#[test]
fn a_scan_that_starts_one_word_from_the_top_reads_that_word_and_stops() {
    let vmar = aspace();
    let code = map(&vmar, "libc.so", 1, 0, 1, RX);
    let stack = a_stack(&vmar, 1);
    let last = PAGE_SIZE / WORD - 1;
    put_word(&vmar, stack, last, code + 0x10);
    let trace = user_stack_backtrace(&vmar, stack + last * WORD);
    assert_eq!(
        trace.len(),
        1,
        "the one word left in the mapping is read: {:?}",
        trace
    );
    assert!(
        trace[0].starts_with("stack[+0x0]"),
        "and it is the first word of the scan: {}",
        trace[0]
    );
}

/// A stack pointer no mapping covers -- a stack overflow past the guard
/// page, a register the fault corrupted -- yields no trace instead of
/// faulting the kernel inside the report of a fault.
#[test]
fn a_stack_pointer_nobody_mapped_yields_no_trace() {
    let vmar = aspace();
    let code = map(&vmar, "libc.so", 1, 0, 1, RX);
    let stack = a_stack(&vmar, 1);
    put_word(&vmar, stack, 0, code + 0x10);
    assert_eq!(
        user_stack_backtrace(&vmar, stack).len(),
        1,
        "there is a frame to find from the real stack"
    );
    assert!(
        user_stack_backtrace(&vmar, stack + 0x1000_0000).is_empty(),
        "but none from an sp that points at no memory"
    );
    assert!(
        user_stack_backtrace(&vmar, 0).is_empty(),
        "nor from a null one"
    );
}
