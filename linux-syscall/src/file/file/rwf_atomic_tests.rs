use super::*;

#[test]
fn atomic_writes_need_a_power_of_two_in_the_unit_aligned_to_itself() {
    let (min, max) = (4096, 1 << 20);
    assert!(atomic_write_valid(4096, 0, min, max));
    assert!(atomic_write_valid(65536, 65536 * 3, min, max));
    assert!(atomic_write_valid(1 << 20, 1 << 20, min, max));
    assert!(!atomic_write_valid(4096 * 3, 0, min, max));
    assert!(!atomic_write_valid(2048, 0, min, max));
    assert!(!atomic_write_valid(2 << 20, 0, min, max));
    assert!(!atomic_write_valid(8192, 4096, min, max));
    assert!(!atomic_write_valid(0, 0, min, max));
}

#[test]
fn the_rwf_numbers_are_linuxs() {
    assert_eq!((RWF_DSYNC, RWF_SYNC, RWF_ATOMIC), (0x2, 0x4, 0x40));
}
