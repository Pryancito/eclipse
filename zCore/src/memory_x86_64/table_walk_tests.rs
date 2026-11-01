use super::table_child;

/// Bits 51:12 are the address; the low twelve bits and bit 63 are flags.
fn entry(child_pa: usize, flags: u64) -> u64 {
    child_pa as u64 | flags
}

#[test]
fn an_absent_entry_has_nothing_under_it() {
    for level in 2..=4u8 {
        assert_eq!(table_child(entry(0x1000, 0), level), None, "level {level}");
        assert_eq!(
            table_child(entry(0x1000, 1 << 1 | 1 << 2), level),
            None,
            "level {level}: writable and user, but not present"
        );
    }
}

#[test]
fn a_present_entry_names_the_table_under_it_with_its_flags_stripped() {
    // present | writable | user | accessed | dirty | NX
    let flags = 1 | 1 << 1 | 1 << 2 | 1 << 5 | 1 << 6 | 1 << 63;
    for level in 2..=4u8 {
        assert_eq!(
            table_child(entry(0x1234_5000, flags), level),
            Some(0x1234_5000),
            "level {level}"
        );
    }
}

/// Bit 7 is `PS` in a PDPTE (1 GiB) and a PDE (2 MiB): the entry maps memory
/// instead of naming a table.
#[test]
fn a_huge_leaf_has_no_table_under_it() {
    let huge = 1 | 1 << 7;
    assert_eq!(table_child(entry(0x4000_0000, huge), 3), None, "1 GiB page");
    assert_eq!(table_child(entry(0x20_0000, huge), 2), None, "2 MiB page");
}

/// ...but in the PML4 bit 7 is reserved, not `PS`. Reading it as `PS` there
/// would skip a whole 512 GiB quarter of the tree, live page tables and all,
/// and every one of those frames would go into the pool as ordinary RAM.
#[test]
fn bit_seven_in_the_top_table_is_not_a_huge_page() {
    assert_eq!(
        table_child(entry(0x1000, 1 | 1 << 7), 4),
        Some(0x1000),
        "the PML4 has no PS bit"
    );
}

/// The hole. The walk guarded its *reservation* with `table_pa != 0` and
/// then dereferenced `phys_to_virt(table_pa)` anyway, so a present entry
/// naming frame 0 -- the shape of a half-overwritten entry, and nothing
/// legitimate -- sent it reading frame 0 as a page table and recursing into
/// whatever it found there. In the one function whose whole job is to stop a
/// recycled page table from triple faulting the machine.
#[test]
fn a_present_entry_naming_frame_zero_is_not_followed() {
    for level in 2..=4u8 {
        assert_eq!(table_child(entry(0, 1), level), None, "level {level}");
        assert_eq!(
            table_child(entry(0, 1 | 1 << 1 | 1 << 5), level),
            None,
            "level {level} with flags"
        );
    }
}
