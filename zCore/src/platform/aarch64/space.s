.section .data
.align 12
sdata:
    .space 0x8000 // 32K

.section .bss.stack
.align 12
boot_stack:
    // 128 KiB, the same per-hart size riscv64 gives its boot stacks.
    //
    // It was 32 KiB, and `create_root_fs` ran off the bottom of it: the block
    // scan alone puts a 4 KiB sector window on the stack before it goes
    // looking for partitions and filesystems. The failure was unreadable —
    // AArch64 takes the resulting data abort to the vector table, the handler
    // pushes its trapframe onto the same dead stack, faults again, and the CPU
    // spins at `__vectors + 0x200` forever with nothing on the console. Every
    // case of `Linux Other Test Baremetal (aarch64)` timed out at
    // "[boot] create_root_fs: scanning 1 block device(s)" for this reason.
    .space 0x20000 // 128K
boot_stack_top:
