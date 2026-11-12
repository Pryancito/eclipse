//! Switch context by function call within the same privilege level.
//!
//! # Assumption
//!
//! This module suppose you are running kernel on Linux or macOS with glibc,
//! and your user program is based on musl libc.
//!
//! Because we will store values in their pthread structure.

use super::UserContext;
use core::arch::global_asm;

extern "sysv64" {
    /// The syscall entry of function call.
    ///
    /// # Usage
    ///
    /// Replace `syscall` instruction by a `call` instruction.
    ///
    /// ```asm
    /// syscall
    /// call syscall_fn_entry
    /// ```
    pub fn syscall_fn_entry();

    fn syscall_fn_return(regs: &mut UserContext);

    /// The way back into the kernel for a guest that faulted.
    ///
    /// Not callable from Rust: it is the address a fault catcher points the
    /// guest's instruction pointer at, with the stack pointer at a frame of
    /// [`FAULT_FRAME_WORDS`] words it laid somewhere of its own. See the asm
    /// for the layout.
    pub fn fault_fn_entry();
}

/// How many words a fault catcher lays before jumping to [`fault_fn_entry`],
/// with the stack pointer at the first. In order: the error code, the x86
/// trap vector, the faulting `rip`, the guest's `r11`, the guest's own
/// `rsp`, and the guest's `rflags`.
///
/// The frame is NOT on the guest's stack. It used to be laid just below the
/// guest's stack pointer, which is memory the guest owns twice over: the
/// SysV ABI gives a leaf function the 128 bytes under `rsp` as a red zone, so
/// a fault in one lost whatever lived there; and a Zircon thread that exits
/// detached unmaps its own stack first and makes the exit syscall with `rsp`
/// still pointing into the hole (`C11ThreadTest.SelfDetachAndFree`), so the
/// catcher's write faulted inside the catcher and the whole process went.
/// Real hardware writes a trap frame on the KERNEL's stack and touches the
/// guest's not at all; the catcher now does the same with a stack of its own.
pub const FAULT_FRAME_WORDS: usize = 6;

impl UserContext {
    /// Go to user context by function return, within the same privilege level.
    ///
    /// User program should call `syscall_fn_entry()` to return back.
    /// Trap reason and error code will always be set to 0x100 and 0.
    pub fn run_fncall(&mut self) {
        // Set before the trip out, not after it, because the way back is no
        // longer only `syscall_fn_entry`: a guest that faults comes back
        // through `fault_fn_entry`, which writes the real cause into these
        // two fields itself. Overwriting them on return would turn every
        // fault into a syscall numbered 0x100.
        self.trap_num = 0x100;
        self.error_code = 0;
        // The vector registers go out and come back with the guest, as
        // `run` does on bare metal. A syscall may clobber them by contract,
        // but the way back in is no longer only a syscall: a page fault the
        // kernel serves, or the end of a time slice, lands on any instruction
        // at all, in the middle of a `memcpy` with the bytes still in `xmm`,
        // and the kernel code that handles it uses the same registers. The
        // host restores the guest's state on its way out of the signal
        // handler, so what `fault_fn_entry` arrives with is the guest's; the
        // asm paths touch no vector register, and nothing of this wrapper's
        // runs between them and the save. Whatever a slice ended on came
        // back with the kernel's `xmm` in it (`zx_vmar_unmap` called with a
        // host pointer, `PThreadBarrierTest` under `--gtest_repeat`).
        unsafe {
            let mask = hosted_xsave_mask();
            let fp = core::ptr::addr_of_mut!(self.fpstate) as *mut u8;
            if mask != 0 {
                core::arch::asm!(
                    "xrstor [{}]",
                    in(reg) fp,
                    in("eax") mask, in("edx") 0u32,
                    options(readonly, nostack, preserves_flags),
                );
                syscall_fn_return(self);
                let fp = core::ptr::addr_of_mut!(self.fpstate) as *mut u8;
                core::arch::asm!(
                    "xsave [{}]",
                    in(reg) fp,
                    in("eax") mask, in("edx") 0u32,
                    options(nostack, preserves_flags),
                );
            } else {
                core::arch::asm!("fxrstor [{}]", in(reg) fp, options(readonly, nostack, preserves_flags));
                syscall_fn_return(self);
                let fp = core::ptr::addr_of_mut!(self.fpstate) as *mut u8;
                core::arch::asm!("fxsave [{}]", in(reg) fp, options(nostack, preserves_flags));
            }
        }
    }
}

/// The `XSAVE` component mask for a hosted kernel: what the HOST enabled in
/// `XCR0`, which is what its user mode -- this kernel and its guests alike --
/// may use, kept to the register components (`x87`, `SSE`, `AVX`, and the
/// three of AVX-512) that [`super::FpState`] has room for. Zero when the
/// host has no `XSAVE` for user mode, which is the `FXSAVE` path.
///
/// Read once: `XCR0` is per CPU but the host keeps it the same on all of
/// them, as `init_fpu` does on bare metal.
pub(crate) fn hosted_xsave_mask() -> u32 {
    use core::sync::atomic::{AtomicU32, Ordering};
    const UNRESOLVED: u32 = u32::MAX;
    const REGISTER_COMPONENTS: u32 = 0b1110_0111;
    static MASK: AtomicU32 = AtomicU32::new(UNRESOLVED);
    let mask = MASK.load(Ordering::Relaxed);
    if mask != UNRESOLVED {
        return mask;
    }
    // CPUID leaf 1 exists on every x86_64 processor, and `xgetbv` is legal
    // once `OSXSAVE` says the host turned `XSAVE` on.
    let leaf1 = core::arch::x86_64::__cpuid(1);
    let osxsave = leaf1.ecx & (1 << 27) != 0;
    let mask = if osxsave {
        let xcr0_low: u32;
        unsafe {
            core::arch::asm!(
                "xgetbv",
                in("ecx") 0u32,
                out("eax") xcr0_low,
                out("edx") _,
                options(nomem, nostack, preserves_flags),
            );
        }
        xcr0_low & REGISTER_COMPONENTS
    } else {
        0
    };
    MASK.store(mask, Ordering::Relaxed);
    mask
}

// The kernel state the guest has to find again on its way back in lives in the
// HOST thread's GS base, not in the guest's thread structure.
//
// It used to live at the guest's `fs:48`, on the grounds that a musl
// `struct pthread` keeps `canary2` there and nothing reads it. Fuchsia's libc
// does not have that field: its thread structure holds a live pointer at
// offset 48, so the first syscall a Zircon guest made after its libc
// initialised read that pointer as a kernel fsbase, loaded a zero out of
// guest memory sixty-four bytes further on, and died in `pop rsp` with a
// null stack pointer. That is the `-11` every `Zircon Core Test Libos` run
// exits with, right after userboot maps the test binary.
//
// GS is free on x86_64: neither glibc nor musl nor Fuchsia's libc uses it,
// and a guest cannot change it (the trap frame carries no gs base, so
// `arch_prctl(ARCH_SET_GS)` from the guest never reaches the register).
// Pointing it at the host thread's own fsbase makes both lookups direct and
// takes the guest's memory out of the path entirely.
//
// User: (musl, Fuchsia libc)
// - fs:0  (pthread.self)       = user fsbase
//
// Kernel: (glibc)
// - fs:0  (pthread.self)       = kernel fsbase, and so does gs:0
// - fs:64 (pthread.???)        = kernel stack, and so does gs:64
// - fs:72 (pthread.???)        = init user fsbase
//
#[cfg(target_os = "linux")]
global_asm!(
    r#"
.macro SWITCH_TO_KERNEL_STACK
    mov rsp, gs:64          # rsp = kernel stack (gs base == kernel fsbase)
.endm
.macro SAVE_KERNEL_STACK
    mov fs:64, rsp
.endm
.macro PUSH_USER_FSBASE
    push 0                  # a placeholder; FILL_USER_FSBASE writes the real
                            # one once every register has been saved
.endm
# Ask the HOST for the guest's FS base, straight into the frame's slot for it.
#
# What was here was `push fs:0`, which reads the base out of the guest's own
# thread structure: the base points at a `struct pthread` whose first field is
# a pointer to itself. That holds while a guest's FS base points at a thread
# structure, and a guest is under no obligation to keep it that way --
# `zx_thread_write_state(ZX_THREAD_STATE_GENERAL_REGS)` sets `fs_base` to
# whatever is asked, and `wrfsbase` is a user-mode instruction.
#
# Measured in a libos build in this container, from `Property.RegisterFs` of
# the Zircon core tests, which sets a thread's FS base to `0xdead2` and then
# expects the thread to run on:
#
#   fault_fn_entry+33:  push %fs:0x0
#   si_addr   = 0xdead2
#   fs_base   = 0xdead2
#
# The guest faulted, the catcher sent it to `fault_fn_entry`, and the way back
# in faulted on the same bad base. The catcher dressed THAT fault as a guest
# fault too and jumped to the entry again: one host thread spinning at 100% on
# one instruction for ever. The run did not crash, it hung, and that is the
# 1200-second timeout `Zircon Core Test Libos (x86_64)` died of.
#
# This costs one more `arch_prctl` per trip in, on a path that already issues
# one on the way in (SWITCH_TO_KERNEL_FSBASE) and one on the way out
# (POP_USER_FSBASE). It is also the only way to see a `wrfsbase` the guest did
# behind the kernel's back, which reading a remembered value would miss.
.macro FILL_USER_FSBASE
    mov eax, 158            # SYS_arch_prctl
    mov edi, 0x1003         # ARCH_GET_FS
    lea rsi, [rsp + 18*8]   # &frame.fsbase (rsp is at frame.rax)
    syscall
.endm
.macro SWITCH_TO_KERNEL_FSBASE
    mov eax, 158            # SYS_arch_prctl
    mov edi, 0x1002         # SET_FS
    mov rsi, gs:0           # rsi = kernel fsbase
    syscall
.endm
.macro POP_USER_FSBASE
    mov rdx, fs:0           # rdx = kernel fsbase (read before fs moves)
    mov rsi, rdx
    mov eax, 158            # SYS_arch_prctl
    mov edi, 0x1001         # SET_GS
    syscall                 # gs base = kernel fsbase, for the way back in
    mov rsi, [rsp + 18 * 8] # rsi = user fsbase
    test rsi, rsi
    jnz 1f                  # if not 0, goto set
0:  lea rsi, [rdx + 72]     # rsi = init user fsbase
    mov [rsi], rsi          # user_fs:0 = user fsbase
1:  mov eax, 158            # SYS_arch_prctl
    mov edi, 0x1002         # SET_FS
    syscall                 # set fsbase
.endm

.global syscall_fn_entry
.global syscall_fn_return
.global fault_fn_entry
"#
);

// User: (musl)
// - gs:0   (pthread.self)      = user gsbase
// - gs:48  (pthread.canary2)   = kernel gsbase
//
// Kernel: (darwin)
// - gs:0   (pthread.tsd[self]) = kernel gsbase - 224
// - gs:48  (pthread.tsd[6])    = kernel stack
// - gs:240 (pthread.tsd[30])   = init user fsbase
//
// Ref:
// - Set gsbase:
//   - https://gist.github.com/aras-p/5389747
// - Get gsbase:
//   - https://github.com/DynamoRIO/dynamorio/issues/1568#issuecomment-239819506
//   - https://github.com/apple/darwin-libpthread/blob/03c4628c8940cca6fd6a82957f683af804f62e7f/src/internal.h#L241
#[cfg(target_os = "macos")]
global_asm!(
    r#"
.macro SWITCH_TO_KERNEL_STACK
    mov rsp, gs:48          # rsp = kernel gsbase
    mov rsp, [rsp + 48]     # rsp = kernel stack
.endm
.macro SAVE_KERNEL_STACK
    mov gs:48, rsp
.endm
.macro PUSH_USER_FSBASE
    push gs:0
.endm
# Nothing to fill: the push above already read it. The Linux macro of the same
# name explains why that read is not good enough there; nobody has measured
# this path on darwin, so it keeps what it had.
.macro FILL_USER_FSBASE
.endm
.macro SWITCH_TO_KERNEL_FSBASE
    mov rdi, gs:48          # rdi = kernel gsbase
    mov eax, 0x3000003
    syscall                 # set gsbase
.endm
.macro POP_USER_FSBASE
    mov rdi, [rsp + 18 * 8] # rdi = user gsbase
    mov rsi, gs:0
    add rsi, 224            # rsi = kernel gsbase
    test rdi, rdi
    jnz 1f                  # if not 0, goto set
0:  lea rdi, [rsi + 30*8]   # rdi = init user gsbase
                            #     = pthread.tsd[30] (kernel gsbase + 30 * 8)
    mov [rdi], rdi          # user_gs:0 = user gsbase
1:  mov eax, 0x3000003
    syscall                 # set gsbase
    mov gs:48, rsi          # user_gs:48 = kernel gsbase
.endm

.global _syscall_fn_entry
.global syscall_fn_entry
.global _syscall_fn_return
.global fault_fn_entry
.global _fault_fn_entry
.set _fault_fn_entry, fault_fn_entry
.set _syscall_fn_entry, syscall_fn_entry
.set _syscall_fn_return, syscall_fn_return
"#
);

global_asm!(
    r#"
syscall_fn_entry:
    # save rsp
    lea r11, [rsp + 8]      # save rsp to r11 (clobber)

    SWITCH_TO_KERNEL_STACK
    pop rsp
    lea rsp, [rsp + 20*8]   # rsp = top of trap frame

    # push trap frame (struct GeneralRegs)
    push 0                  # ignore gs_base
    PUSH_USER_FSBASE
    pushfq                  # push rflags
    push [r11 - 8]          # push rip
    push r15
    push r14
    push r13
    push r12
    push r11
    push r10
    push r9
    push r8
    push r11                # push rsp
    push rbp
    push rdi
    push rsi
    push rdx
    push rcx
    push rbx
    push rax

    FILL_USER_FSBASE

    # restore callee-saved registers
    SWITCH_TO_KERNEL_STACK
    pop rbx
    pop rbx
    pop rbp
    pop r12
    pop r13
    pop r14
    pop r15

    SWITCH_TO_KERNEL_FSBASE

    # go back to Rust
    ret

    # The way back for a guest that FAULTED rather than made a syscall.
    #
    # Jumped to (not called) with the guest's registers as they were at the
    # fault, except rsp, which points at five words laid by whoever caught the
    # fault -- on a libos host, the signal handler, from the `ucontext` the
    # kernel handed it -- on memory of the catcher's own, never the guest's
    # (see `FAULT_FRAME_WORDS` for why):
    #
    #   [rsp]        error_code
    #   [rsp + 8]    trap_num       (the x86 vector: 6 for #UD, 13 for #GP,
    #                                and so on)
    #   [rsp + 16]   guest rip      (where the fault happened)
    #   [rsp + 24]   guest r11      (this code needs r11 as scratch, and the
    #                                frame must still carry the real one: a
    #                                Zircon exception report hands the
    #                                thread's registers to a debugger)
    #   [rsp + 32]   guest rsp      (as it was at the fault)
    #   [rsp + 40]   guest rflags   (as they were at the fault)
    #
    # Everything after that is `syscall_fn_entry` with the rip, r11, rsp and
    # rflags taken from those words instead of from the call, and with
    # trap_num/error_code written into the frame on the way past them.
    #
    # The flags the kernel then runs with are NOT the guest's. A trap on real
    # hardware clears TF and (through an interrupt gate or `syscall`'s mask)
    # the kernel entry does `cld` and runs with AC off; a hosted catcher
    # arrives here with the guest's own rflags live, because the host put
    # them back on the way out of the signal handler. A guest spinning with
    # DF and AC set (`Threads.ReadingGeneralRegisterState` fills every
    # register with a pattern, those bits included) then had the kernel's
    # first misaligned store answered with #AC -- SIGBUS, the whole process
    # gone. So the frame carries the guest's flags for the record and the
    # live ones are scrubbed before any Rust runs.
fault_fn_entry:
    mov r11, rsp            # r11 = the catcher's frame

    SWITCH_TO_KERNEL_STACK

    # The kernel's flags: the guest's TF (0x100), DF (0x400), NT (0x4000)
    # and AC (0x40000) off, as a trap on hardware leaves them. NT matters
    # for the way back: `iretq` raises #GP when the CURRENT flags have it.
    #
    # Done HERE, with rsp on the kernel stack, and nowhere else. This scrub
    # first sat below the trap frame, after the pushes, when rsp was the
    # address of the `UserContext` itself: `pushfq` then wrote the flags
    # word 8 bytes below the context, which is the header of the heap
    # chunk the context lives in (`Box<UserContext>`, 64-byte aligned).
    # Every tick stamped a chunk size of 0x246 over it, and glibc noticed
    # the next time a neighbour was resized: `corrupted size vs.
    # prev_size` out of a `zx_vmar_allocate`, the whole process gone
    # (`Threads.ReadingGeneralRegisterState`).
    pushfq
    and qword ptr [rsp], -0x44501
    popfq

    pop rsp                 # rsp = &UserContext
    lea rsp, [rsp + 22*8]   # one past error_code
    push [r11]              # error_code
    push [r11 + 8]          # trap_num
                            # rsp = top of trap frame (struct GeneralRegs)

    # push trap frame (struct GeneralRegs)
    push 0                  # ignore gs_base
    PUSH_USER_FSBASE
    push [r11 + 40]         # push the guest's own rflags
    push [r11 + 16]         # push rip
    push r15
    push r14
    push r13
    push r12
    push [r11 + 24]         # push the guest's own r11
    push r10
    push r9
    push r8
    push [r11 + 32]         # push the guest's own rsp
    push rbp
    push rdi
    push rsi
    push rdx
    push rcx
    push rbx
    push rax

    FILL_USER_FSBASE

    # restore callee-saved registers
    SWITCH_TO_KERNEL_STACK
    pop rbx
    pop rbx
    pop rbp
    pop r12
    pop r13
    pop r14
    pop r15

    SWITCH_TO_KERNEL_FSBASE

    # go back to Rust, out of `syscall_fn_return` as if it had returned
    ret

    # extern "sysv64" fn syscall_fn_return(&mut UserContext)
syscall_fn_return:
    # save callee-saved registers
    push r15
    push r14
    push r13
    push r12
    push rbp
    push rbx

    push rdi
    SAVE_KERNEL_STACK
    mov rsp, rdi

    POP_USER_FSBASE

    # pop trap frame (struct GeneralRegs)
    pop rax
    pop rbx
    pop rcx
    pop rdx
    pop rsi
    pop rdi
    pop rbp
    pop r8                  # skip rsp
    pop r8
    pop r9
    pop r10
    pop r11
    pop r12
    pop r13
    pop r14
    pop r15
                            # rsp = &frame.rip; the slots from here on are
                            # rip, rflags, fsbase, gsbase, trap_num
    # The last three -- rip, rflags and the guest's own rsp -- go back in
    # one `iretq`, which a ring-3 `iretq` to ring 3 is allowed to do and
    # which touches no register. What was here popped rip into r11 and
    # `jmp r11`, so every trip out handed the guest the entry address in
    # r11. A syscall clobbers r11 by contract and a fault rarely cares, but
    # the end of a time slice (`PREEMPT_VECTOR` in the libos fault bridge)
    # lands on any instruction at all, and userboot died of a #GP on its
    # first slice. The iret frame is pushed over the five slots just below
    # rip (r11 to r15), which the pops above have already consumed; nothing
    # above rip is touched, so trap_num and error_code stay what the kernel
    # set. A push addresses through rsp BEFORE decrementing it.
    push 0
    mov word ptr [rsp], ss          # [R-8]  ss
    push qword ptr [rsp - 8*8]      # [R-16] the guest's rsp (frame slot 7,
                                    #        9 below rip = R-72)
    push qword ptr [rsp + 8*3]      # [R-24] rflags (frame slot 17 = R+8)
    or qword ptr [rsp], 2           #        bit 1 of rflags always reads 1
    push 0
    mov word ptr [rsp], cs          # [R-32] the ring-3 cs this thread runs with
    push qword ptr [rsp + 8*4]      # [R-40] rip (R)
    iretq
"#
);

#[cfg(test)]
mod tests {
    use crate::*;
    use core::arch::global_asm;

    #[cfg(target_os = "macos")]
    global_asm!(".set _dump_registers, dump_registers");

    // Mock user program to dump registers at stack.
    global_asm!(
        r#"
dump_registers:
    push r15
    push r14
    push r13
    push r12
    push r11
    push r10
    push r9
    push r8
    push rsp
    push rbp
    push rdi
    push rsi
    push rdx
    push rcx
    push rbx
    push rax

    add rax, 10
    add rbx, 10
    add rcx, 10
    add rdx, 10
    add rsi, 10
    add rdi, 10
    add rbp, 10
    add r8, 10
    add r9, 10
    add r10, 10
    add r11, 10
    add r12, 10
    add r13, 10
    add r14, 10
    add r15, 10

    call syscall_fn_entry
"#
    );

    #[test]
    fn run_fncall() {
        extern "sysv64" {
            fn dump_registers();
        }
        let mut stack = [0u8; 0x1000];
        let mut cx = UserContext {
            general: GeneralRegs {
                rax: 0,
                rbx: 1,
                rcx: 2,
                rdx: 3,
                rsi: 4,
                rdi: 5,
                rbp: 6,
                rsp: stack.as_mut_ptr() as usize + 0x1000,
                r8: 8,
                r9: 9,
                r10: 10,
                r11: 11,
                r12: 12,
                r13: 13,
                r14: 14,
                r15: 15,
                rip: dump_registers as *const () as usize,
                rflags: 0,
                fsbase: 0, // don't set to non-zero garbage value
                gsbase: 0,
            },
            trap_num: 0,
            error_code: 0,
            // `fncall` is the libos path, where the "user" code is a function
            // in this very process: it is 64-bit, so never a compat thread,
            // and it has no 32-bit thread area either.
            compat32: 0,
            compat_gs: 0,
            compat_tls: [0; 3],
            fpstate: Default::default(),
        };
        cx.run_fncall();
        // check restored registers
        let general = unsafe { *(cx.general.rsp as *const GeneralRegs) };
        assert_eq!(
            general,
            GeneralRegs {
                rax: 0,
                rbx: 1,
                rcx: 2,
                rdx: 3,
                rsi: 4,
                rdi: 5,
                rbp: 6,
                // skip rsp
                r8: 8,
                r9: 9,
                r10: 10,
                // Delivered: the way out used to carry the entry address
                // here instead (see the `iretq` in `syscall_fn_return`).
                r11: 11,
                r12: 12,
                r13: 13,
                r14: 14,
                r15: 15,
                ..general
            }
        );
        // check saved registers
        assert_eq!(
            cx.general,
            GeneralRegs {
                rax: 10,
                rbx: 11,
                rcx: 12,
                rdx: 13,
                rsi: 14,
                rdi: 15,
                rbp: 16,
                // skip rsp
                r8: 18,
                r9: 19,
                r10: 20,
                // skip r11: a syscall clobbers it by contract, and so does
                // `syscall_fn_entry`
                r12: 22,
                r13: 23,
                r14: 24,
                r15: 25,
                ..cx.general
            }
        );
        assert_eq!(cx.trap_num, 0x100);
        assert_eq!(cx.error_code, 0);
    }

    #[cfg(target_os = "macos")]
    global_asm!(".set _fault_from_guest, fault_from_guest");

    // Mock guest that does what a fault catcher does on its behalf: lay the
    // six words on a stack of its own (`fault_frame`, which is not the
    // guest's) and jump with rsp at them. `r11` is stored BEFORE it is
    // clobbered, so the frame still carries the guest's own value.
    global_asm!(
        r#"
fault_from_guest:
    pushfq
    pop qword ptr [rip + fault_frame + 40]   # the guest's own rflags
    mov [rip + fault_frame + 32], rsp    # the guest's own rsp
    mov [rip + fault_frame + 24], r11    # the guest's own r11
    lea r11, [rip + 2f]
    mov [rip + fault_frame + 16], r11    # the faulting rip
    mov qword ptr [rip + fault_frame + 8], 6   # trap_num: #UD
    mov qword ptr [rip + fault_frame], 0       # error_code
    lea rsp, [rip + fault_frame]
    jmp fault_fn_entry
2:
"#
    );

    #[no_mangle]
    #[allow(non_upper_case_globals)]
    static mut fault_frame: [u64; FAULT_FRAME_WORDS] = [0; FAULT_FRAME_WORDS];

    #[cfg(target_os = "macos")]
    global_asm!(
        ".set _load_xmm7_then_fault, load_xmm7_then_fault",
        ".set _store_xmm7_then_syscall, store_xmm7_then_syscall"
    );

    // Two halves of one mock guest: the first fills `xmm7` and faults, the
    // way `fault_from_guest` does; the second, resumed after the kernel had
    // its turn, stores `xmm7` where the test can read it and makes a
    // syscall.
    global_asm!(
        r#"
load_xmm7_then_fault:
    movdqu xmm7, [rip + xmm7_pattern]
    pushfq
    pop qword ptr [rip + fault_frame + 40]
    mov [rip + fault_frame + 32], rsp
    mov [rip + fault_frame + 24], r11
    lea r11, [rip + 2f]
    mov [rip + fault_frame + 16], r11
    mov qword ptr [rip + fault_frame + 8], 14   # trap_num: #PF
    mov qword ptr [rip + fault_frame], 0
    lea rsp, [rip + fault_frame]
    jmp fault_fn_entry
2:
store_xmm7_then_syscall:
    movdqu [rip + xmm7_seen], xmm7
    call syscall_fn_entry
"#
    );

    #[no_mangle]
    #[allow(non_upper_case_globals)]
    static mut xmm7_pattern: [u64; 2] = [0x1122_3344_5566_7788, 0x99aa_bbcc_ddee_ff00];
    #[no_mangle]
    #[allow(non_upper_case_globals)]
    static mut xmm7_seen: [u64; 2] = [0; 2];

    /// A guest interrupted between two instructions -- a page fault the
    /// kernel serves, the end of a time slice -- keeps its vector registers
    /// across the kernel's turn: `xmm7` is saved with the context on the way
    /// in and restored on the way out, whatever the kernel did with the
    /// register meanwhile.
    #[test]
    fn the_vector_registers_survive_the_kernel_s_turn() {
        extern "sysv64" {
            fn load_xmm7_then_fault();
            fn store_xmm7_then_syscall();
        }
        let mut stack = [0u8; 0x1000];
        let guest_rsp = stack.as_mut_ptr() as usize + 0x1000;
        let mut cx = UserContext {
            general: GeneralRegs {
                rsp: guest_rsp,
                rip: load_xmm7_then_fault as *const () as usize,
                ..Default::default()
            },
            ..Default::default()
        };
        cx.run_fncall();
        assert_eq!(cx.trap_num, 14);
        // The kernel's turn: `xmm7` means nothing to it.
        unsafe {
            core::arch::asm!(
                "pxor xmm7, xmm7",
                out("xmm7") _,
                options(nomem, nostack, preserves_flags)
            );
        }
        // The FXSAVE legacy area keeps `xmm7` at 160 + 7 * 16, with or
        // without `XSAVE`.
        let saved = &cx.fpstate.as_bytes()[272..288];
        let pattern = unsafe { core::ptr::addr_of!(xmm7_pattern).read() };
        assert_eq!(&saved[..8], &pattern[0].to_le_bytes());
        assert_eq!(&saved[8..], &pattern[1].to_le_bytes());

        cx.general.rip = store_xmm7_then_syscall as *const () as usize;
        cx.run_fncall();
        assert_eq!(cx.trap_num, 0x100);
        let seen = unsafe { core::ptr::addr_of!(xmm7_seen).read() };
        assert_eq!(seen, pattern, "the guest did not get its xmm7 back");
    }

    #[cfg(target_os = "macos")]
    global_asm!(".set _fault_with_df_and_ac_set, fault_with_df_and_ac_set");

    // Mock guest that faults with DF and AC set, as a thread filling every
    // register with a test pattern does.
    global_asm!(
        r#"
fault_with_df_and_ac_set:
    pushfq
    or qword ptr [rsp], 0x40400
    popfq
    pushfq
    pop qword ptr [rip + fault_frame + 40]
    mov [rip + fault_frame + 32], rsp
    mov [rip + fault_frame + 24], r11
    lea r11, [rip + 2f]
    mov [rip + fault_frame + 16], r11
    mov qword ptr [rip + fault_frame + 8], 13
    mov qword ptr [rip + fault_frame], 0
    lea rsp, [rip + fault_frame]
    jmp fault_fn_entry
2:
"#
    );

    /// The kernel runs with its own flags after a fault, not the guest's: DF,
    /// NT and AC off, as a trap on hardware leaves them, while the frame keeps
    /// the guest's for `zx_thread_read_state`. With the guest's AC live, the
    /// kernel's first misaligned store was a #AC and a SIGBUS for the whole
    /// process (`Threads.ReadingGeneralRegisterState`).
    #[test]
    fn the_kernel_does_not_inherit_the_guest_s_df_and_ac() {
        extern "sysv64" {
            fn fault_with_df_and_ac_set();
        }
        // The 64 bytes right below the context, which in the kernel are
        // the header of the heap chunk a `Box<UserContext>` lives in: the
        // scrub must not touch them. It once did, with a `pushfq` issued
        // while rsp was the address of the context itself.
        #[repr(C, align(64))]
        struct Guarded {
            below: [u64; 8],
            cx: UserContext,
        }
        const SENTINEL: u64 = 0xa5a5_5a5a_a5a5_5a5a;
        let mut stack = [0u8; 0x1000];
        let guest_rsp = stack.as_mut_ptr() as usize + 0x1000;
        let mut guarded = Guarded {
            below: [SENTINEL; 8],
            cx: UserContext {
                general: GeneralRegs {
                    rsp: guest_rsp,
                    rip: fault_with_df_and_ac_set as *const () as usize,
                    ..Default::default()
                },
                ..Default::default()
            },
        };
        assert_eq!(
            core::ptr::addr_of!(guarded.cx) as usize
                - core::ptr::addr_of!(guarded.below[7]) as usize,
            8
        );
        guarded.cx.run_fncall();
        let live: u64;
        unsafe {
            core::arch::asm!("pushfq", "pop {}", out(reg) live, options(nomem));
        }
        let cx = &guarded.cx;
        assert_eq!(
            guarded.below, [SENTINEL; 8],
            "the entry wrote below the context (over a heap chunk header, in the kernel)"
        );
        assert_eq!(cx.trap_num, 13);
        assert_eq!(
            cx.general.rflags & 0x40400,
            0x40400,
            "the frame must keep the guest's DF and AC"
        );
        assert_eq!(
            live & 0x40400,
            0,
            "the kernel is running with the guest's flags"
        );
    }

    /// The way back for a guest that faulted: the frame must come back with
    /// the fault's cause, the guest's registers as they were -- `r11`
    /// included, which the entry needs as scratch -- and the guest's own stack
    /// pointer, not the catcher's frame the entry was reached with.
    #[test]
    fn run_fncall_returning_from_a_fault() {
        extern "sysv64" {
            fn fault_from_guest();
        }
        let mut stack = [0u8; 0x1000];
        let guest_rsp = stack.as_mut_ptr() as usize + 0x1000;
        let mut cx = UserContext {
            general: GeneralRegs {
                rax: 0,
                rbx: 1,
                rcx: 2,
                rdx: 3,
                rsi: 4,
                rdi: 5,
                rbp: 6,
                rsp: guest_rsp,
                r8: 8,
                r9: 9,
                r10: 10,
                r11: 11,
                r12: 12,
                r13: 13,
                r14: 14,
                r15: 15,
                rip: fault_from_guest as *const () as usize,
                rflags: 0,
                fsbase: 0,
                gsbase: 0,
            },
            trap_num: 0,
            error_code: 0,
            compat32: 0,
            compat_gs: 0,
            compat_tls: [0; 3],
            fpstate: Default::default(),
        };
        cx.run_fncall();

        // The cause, which `run_fncall` must NOT have overwritten with the
        // syscall number it sets before the trip out.
        assert_eq!(cx.trap_num, 6);
        assert_eq!(cx.error_code, 0);
        // Every register the guest held, with nothing of the catcher's frame
        // left in it.
        assert_eq!(
            cx.general,
            GeneralRegs {
                rax: 0,
                rbx: 1,
                rcx: 2,
                rdx: 3,
                rsi: 4,
                rdi: 5,
                rbp: 6,
                rsp: guest_rsp,
                r8: 8,
                r9: 9,
                r10: 10,
                // The way BACK carries whatever the guest actually held --
                // the 11 the way out delivered -- rather than the scratch
                // value `fault_fn_entry` needs: the catcher saved it before
                // the entry clobbered it.
                r11: 11,
                r12: 12,
                r13: 13,
                r14: 14,
                r15: 15,
                ..cx.general
            }
        );
        // And where it was, which is inside the mock, past its jump.
        assert!(
            cx.general.rip > fault_from_guest as *const () as usize,
            "rip {:#x} is not inside the mock guest",
            cx.general.rip
        );
    }
}
