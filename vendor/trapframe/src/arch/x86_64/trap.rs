use core::arch::global_asm;
use core::mem::{offset_of, size_of};

global_asm!(include_str!("trap.S"));
global_asm!(include_str!(concat!(env!("OUT_DIR"), "/vector.S")));

/// Trap frame of kernel interrupt
///
/// # Trap handler
///
/// You need to define a handler function like this:
///
/// ```
/// use trapframe::TrapFrame;
///
/// #[no_mangle]
/// extern "sysv64" fn trap_handler(tf: &mut TrapFrame) {
///     match tf.trap_num {
///         3 => {
///             println!("TRAP: BreakPoint");
///             tf.rip += 1;
///         }
///         _ => panic!("TRAP: {:#x?}", tf),
///     }
/// }
/// ```
#[derive(Debug, Default, Clone, Copy)]
#[repr(C)]
pub struct TrapFrame {
    // Pushed by 'trap.S'
    pub rax: usize,
    pub rbx: usize,
    pub rcx: usize,
    pub rdx: usize,
    pub rsi: usize,
    pub rdi: usize,
    pub rbp: usize,
    pub rsp: usize,
    pub r8: usize,
    pub r9: usize,
    pub r10: usize,
    pub r11: usize,
    pub r12: usize,
    pub r13: usize,
    pub r14: usize,
    pub r15: usize,
    pub _pad: usize,

    // Pushed by 'vector.S'
    pub trap_num: usize,
    pub error_code: usize,

    // Pushed by CPU
    pub rip: usize,
    pub cs: usize,
    pub rflags: usize,
}

/// The contract between [`TrapFrame`] and the assembly that builds it.
///
/// `trap.S` fills this struct by *pushing registers in a fixed order* and
/// unwinds it by popping them back in the mirror order, so every field's byte
/// offset is hardcoded in assembly that the type system cannot see. Reorder
/// two fields here and the stubs keep assembling: `trap_return` then restores
/// each register from the wrong slot, on every interrupt and exception.
///
/// These are compile-time assertions rather than unit tests on purpose. This
/// module only exists under `target_os = "none"` / `"uefi"` (see the `cfg` on
/// `mod trap;`), so no host `cargo test` compiles it at all -- a unit test
/// here would never run, and a mutation of this struct comes out "survived"
/// without anything having looked at it. A `const` assertion is checked in the
/// one build where this code exists, on every architecture, and cannot come
/// out empty.
///
/// The slot numbers come from `__from_kernel`, which pushes (last push lands
/// at the lowest address) `0`, `r15` .. `r8`, the CPU-saved `rsp`, `rbp`,
/// `rdi`, `rsi`, `rdx`, `rcx`, `rbx`, `rax`, on top of what `vector.S`
/// (`error_code`, `trap_num`) and the CPU (`rflags`, `cs`, `rip`) already
/// pushed.
const _: () = {
    // Slot n of the frame is byte n * 8. `trap_return` pops these sixteen in
    // this exact order, so a swap moves two registers' values into each
    // other's slot.
    assert!(
        offset_of!(TrapFrame, rax) == 0 * 8,
        "trap_return pops rax first"
    );
    assert!(offset_of!(TrapFrame, rbx) == 1 * 8, "rbx is slot 1");
    assert!(offset_of!(TrapFrame, rcx) == 2 * 8, "rcx is slot 2");
    assert!(offset_of!(TrapFrame, rdx) == 3 * 8, "rdx is slot 3");
    assert!(offset_of!(TrapFrame, rsi) == 4 * 8, "rsi is slot 4");
    assert!(offset_of!(TrapFrame, rdi) == 5 * 8, "rdi is slot 5");
    assert!(offset_of!(TrapFrame, rbp) == 6 * 8, "rbp is slot 6");
    // The slot `trap_return` deliberately throws away: `pop r8` over `rsp`,
    // because returning to a stack pointer the handler edited would strand the
    // frame. `__from_kernel` fills it from `[rsp + 14*8]`, the CPU-saved RSP.
    assert!(
        offset_of!(TrapFrame, rsp) == 7 * 8,
        "slot 7 is the rsp trap_return discards"
    );
    assert!(offset_of!(TrapFrame, r8) == 8 * 8, "r8 is slot 8");
    assert!(offset_of!(TrapFrame, r9) == 9 * 8, "r9 is slot 9");
    assert!(offset_of!(TrapFrame, r10) == 10 * 8, "r10 is slot 10");
    assert!(offset_of!(TrapFrame, r11) == 11 * 8, "r11 is slot 11");
    assert!(offset_of!(TrapFrame, r12) == 12 * 8, "r12 is slot 12");
    assert!(offset_of!(TrapFrame, r13) == 13 * 8, "r13 is slot 13");
    assert!(offset_of!(TrapFrame, r14) == 14 * 8, "r14 is slot 14");
    assert!(offset_of!(TrapFrame, r15) == 15 * 8, "r15 is slot 15");

    // `__from_kernel`'s first push is the `push 0` that lands here, and its
    // comment block counts the hardware frame from this slot outwards:
    // `[rsp+9]=trap_num` after the nine pushes above it.
    assert!(
        offset_of!(TrapFrame, _pad) == 16 * 8,
        "the `push 0` of __from_kernel is slot 16"
    );

    // Pushed by the `vector{i}` stub: `push 0` (error_code, skipped for the
    // vectors the CPU pushes one for) then `push i`, so trap_num ends up
    // BELOW error_code.
    assert!(
        offset_of!(TrapFrame, trap_num) == 17 * 8,
        "vector.S puts trap_num below error_code"
    );
    assert!(
        offset_of!(TrapFrame, error_code) == 18 * 8,
        "vector.S puts error_code above trap_num"
    );

    // Pushed by the CPU, in the order `iretq` expects to pop them back.
    assert!(
        offset_of!(TrapFrame, rip) == 19 * 8,
        "iretq pops rip from slot 19"
    );
    assert!(
        offset_of!(TrapFrame, cs) == 20 * 8,
        "iretq pops cs from slot 20"
    );
    assert!(
        offset_of!(TrapFrame, rflags) == 21 * 8,
        "iretq pops rflags from slot 21"
    );

    // `trap_return` does a literal `add rsp, 24` to step over the padding,
    // trap_num and error_code and leave rsp on `rip` for `iretq`. Any field
    // added or moved between `_pad` and `rip` leaves `iretq` reading a
    // register value as the return address.
    assert!(
        offset_of!(TrapFrame, rip) == offset_of!(TrapFrame, _pad) + 24,
        "the `add rsp, 24` of trap_return no longer lands on rip"
    );

    // `__alltraps` pushes rax and then reads the hardware frame at fixed
    // offsets from it: `[rsp + 4*8]` for cs (to test the CPL) and
    // `[rsp + 6*8]` for the user rsp. Counting from trap_num, which sits one
    // slot above the pushed rax, that puts cs three slots up.
    assert!(
        offset_of!(TrapFrame, cs) == offset_of!(TrapFrame, trap_num) + 3 * 8,
        "the `[rsp + 4*8]` of __alltraps no longer reads cs"
    );

    // A gap or an extra field changes every slot above it, and `syscall.S`
    // and `trap.S` both step a whole frame with `add rsp, 22*8`.
    assert!(
        size_of::<TrapFrame>() == 22 * 8,
        "the frame is no longer the 22 slots the entry stubs step over"
    );
};
