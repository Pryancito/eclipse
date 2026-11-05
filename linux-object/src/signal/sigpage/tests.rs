//! What the trampoline must be, and where the page must go.
//!
//! The encodings are checked here, on the host, because the architectures
//! they are for are not the one the CI runs unit tests on: a hand-assembled
//! instruction that is wrong is otherwise invisible until a guest returns
//! from its first signal handler, forty minutes later, inside QEMU.

use core::convert::TryInto;

use super::*;

/// The RV64 trampoline, instruction by instruction.
///
/// Decoded from the fields rather than compared against one 64-bit constant,
/// so a failure says which instruction is wrong and in which field.
#[test]
fn the_riscv64_trampoline_is_li_a7_139_then_ecall() {
    let code = riscv64_trampoline();
    let li = u32::from_le_bytes(code[..4].try_into().unwrap());
    let ecall = u32::from_le_bytes(code[4..].try_into().unwrap());

    // addi a7, zero, 139: I-type, opcode OP-IMM, funct3 ADDI, rs1 x0.
    assert_eq!(li & 0x7f, 0b001_0011, "opcode is not OP-IMM");
    assert_eq!((li >> 12) & 0x7, 0, "funct3 is not ADDI");
    assert_eq!((li >> 15) & 0x1f, 0, "rs1 is not zero");
    assert_eq!((li >> 7) & 0x1f, 17, "rd is not a7 (x17)");
    assert_eq!(li >> 20, 139, "the immediate is not NR_rt_sigreturn");

    assert_eq!(ecall, 0x0000_0073, "the second instruction is not ecall");
}

/// The A64 trampoline, likewise.
#[test]
fn the_aarch64_trampoline_is_mov_x8_139_then_svc() {
    let code = aarch64_trampoline();
    let movz = u32::from_le_bytes(code[..4].try_into().unwrap());
    let svc = u32::from_le_bytes(code[4..].try_into().unwrap());

    // movz x8, #139: sf=1, opc=10, 100101, hw=00.
    assert_eq!(movz >> 23, 0b1_10_1001_01, "not a 64-bit MOVZ");
    assert_eq!((movz >> 21) & 0x3, 0, "hw is not 0, so the shift is not 0");
    assert_eq!(
        (movz >> 5) & 0xffff,
        139,
        "the immediate is not NR_rt_sigreturn"
    );
    assert_eq!(movz & 0x1f, 8, "Rd is not x8");

    assert_eq!(svc, 0xd400_0001, "the second instruction is not svc #0");
}

/// Both are eight bytes: two instructions, and no padding that a `ret` could
/// fall into.
#[test]
fn a_trampoline_is_exactly_two_instructions() {
    assert_eq!(riscv64_trampoline().len(), 8);
    assert_eq!(aarch64_trampoline().len(), 8);
}

/// The page sits a guard page below what it is placed under, and the offset it
/// reports is that address measured from the VMAR's own base.
#[test]
fn the_page_goes_a_guard_below_with_the_offset_measured_from_the_vmar() {
    let (base, offset) = placement(0x8000_0000, 0x1000).unwrap();
    assert_eq!(base, 0x8000_0000 - GUARD - PAGE_SIZE);
    assert_eq!(offset, base - 0x1000);
}

/// Nothing is placed when the arithmetic would wrap: neither under an address
/// too low to hold a guard and a page, nor inside a VMAR that starts above
/// where the page would go.
#[test]
fn no_placement_is_reported_instead_of_a_wrapped_one() {
    assert_eq!(placement(GUARD + PAGE_SIZE - 1, 0), None);
    assert_eq!(placement(0x8000_0000, 0x8000_0000), None);
}

/// Both trampolines, byte for byte, against an assembler.
///
/// The field checks above say the instructions are built the way the manual
/// describes them. This says they are the bytes a real assembler emits, which
/// is a different claim and the one a guest actually executes:
///
/// ```text
/// $ printf 'li a7, 139\necall\n' | llvm-mc -triple=riscv64 -show-encoding
///         li a7, 139      # encoding: [0x93,0x08,0xb0,0x08]
///         ecall           # encoding: [0x73,0x00,0x00,0x00]
/// $ printf 'mov x8, #139\nsvc #0\n' | llvm-mc -triple=aarch64 -show-encoding
///         mov x8, #139    // encoding: [0x68,0x11,0x80,0xd2]
///         svc #0          // encoding: [0x01,0x00,0x00,0xd4]
/// ```
#[test]
fn the_trampolines_are_the_bytes_an_assembler_emits() {
    assert_eq!(
        riscv64_trampoline(),
        [0x93, 0x08, 0xb0, 0x08, 0x73, 0x00, 0x00, 0x00]
    );
    assert_eq!(
        aarch64_trampoline(),
        [0x68, 0x11, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4]
    );
}
