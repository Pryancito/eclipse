// Rust language features implementations

use core::alloc::Layout;
use core::panic::PanicInfo;

#[alloc_error_handler]
fn alloc_error(layout: Layout) -> ! {
    let heap_used = crate::memory::heap_used();
    let heap_total = crate::memory::heap_total();
    crate::klog_err!(
        "kernel OOM: alloc {} bytes failed (heap {} / {} KiB)\n",
        layout.size(),
        heap_used / 1024,
        heap_total / 1024
    );
    panic!("memory allocation of {} bytes failed", layout.size());
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Disable interrupts immediately. With panic-strategy=abort, local variables
    // in the panicking function (e.g. kernel-sync's RefMut borrow guard in
    // pop_off) are never dropped. If a timer IRQ fires while the panic handler
    // is running, push_off/pop_off will call borrow_mut() on an already-borrowed
    // RefCell → nested panic → abort() → ud2 → triple fault → QEMU reset.
    kernel_hal::interrupt::intr_off();

    // Use spin variant: interrupts are already off above, and try_lock silently
    // discards output if another CPU holds the lock — unacceptable in panic context.
    if let Some(loc) = info.location() {
        kernel_hal::console::serial_write_fmt_spin(format_args!(
            "\n\npanic cpu={} at {}:{}:{}\n",
            kernel_hal::cpu::cpu_id(),
            loc.file(),
            loc.line(),
            loc.column(),
        ));
    } else {
        kernel_hal::console::serial_write_fmt_spin(format_args!(
            "\n\npanic cpu={}\n",
            kernel_hal::cpu::cpu_id(),
        ));
    }
    // `as_str()` returns None for any panic! with format arguments — use
    // Display on the Arguments directly so the message is always printed.
    kernel_hal::console::serial_write_fmt_spin(format_args!("{}\n", info.message()));

    if cfg!(feature = "baremetal-test") {
        kernel_hal::cpu::reset();
    } else {
        loop {
            core::hint::spin_loop();
        }
    }
}
