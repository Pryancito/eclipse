// Rust language features implementations

use core::alloc::Layout;
use core::panic::PanicInfo;

#[alloc_error_handler]
fn alloc_error(layout: Layout) -> ! {
    // The heap is exhausted here, so we must NOT allocate: klog_*! use
    // `alloc::format!` and would recursively fail. Use the spin serial writer
    // (the same no-alloc path the panic handler uses) so the used/total numbers
    // actually reach the console — they pinpoint whether this is a leak.
    let heap_used = crate::memory::heap_used();
    let heap_total = crate::memory::heap_total();
    kernel_hal::console::serial_write_fmt_spin(format_args!(
        "\nkernel OOM: alloc {} bytes failed (used {} / total {} MiB)\n",
        layout.size(),
        heap_used / 1024 / 1024,
        heap_total / 1024 / 1024,
    ));
    panic!("memory allocation of {} bytes failed", layout.size());
}

/// Fixed-size, no-alloc formatter for the panic banner. The panic handler must
/// not allocate (the panic may BE an OOM) and must not depend on any lock.
struct StackBuf {
    buf: [u8; 512],
    len: usize,
}

impl core::fmt::Write for StackBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let room = self.buf.len() - self.len;
        let n = s.len().min(room);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Disable interrupts immediately. With panic-strategy=abort, local variables
    // in the panicking function (e.g. kernel-sync's RefMut borrow guard in
    // pop_off) are never dropped. If a timer IRQ fires while the panic handler
    // is running, push_off/pop_off will call borrow_mut() on an already-borrowed
    // RefCell → nested panic → abort() → ud2 → triple fault → QEMU reset.
    kernel_hal::interrupt::intr_off();

    // FIRST, before anything that touches a lock: rasterize the panic straight
    // onto the framebuffer (red band, raw pixel writes, no locks, no alloc).
    // Everything below can be silently dropped or deadlock when another CPU —
    // or THIS one — holds the console/serial locks (a panic inside an IRQ
    // handler mid-print left the screen frozen half-line with the real panic
    // visible only on serial). This banner cannot.
    {
        use core::fmt::Write;
        let mut b = StackBuf {
            buf: [0u8; 512],
            len: 0,
        };
        if let Some(loc) = info.location() {
            let _ = write!(
                b,
                "KERNEL PANIC cpu={} {}:{}\n{}",
                kernel_hal::cpu::cpu_id(),
                loc.file(),
                loc.line(),
                info.message()
            );
        } else {
            let _ = write!(
                b,
                "KERNEL PANIC cpu={}\n{}",
                kernel_hal::cpu::cpu_id(),
                info.message()
            );
        }
        let valid = match core::str::from_utf8(&b.buf[..b.len]) {
            Ok(s) => s,
            // Truncation can split a multi-byte char; keep the valid prefix.
            Err(e) => core::str::from_utf8(&b.buf[..e.valid_up_to()]).unwrap_or(""),
        };
        kernel_hal::console::panic_banner(valid);
    }

    // Make the panic VISIBLE after a compositor took the screen. Once labwc
    // sets KD_GRAPHICS the kernel stops PRESENTING the text console (writes
    // still land in the shadow buffer but are never pushed to the display), so
    // a panic here would only reach serial — the monitor stays black on the
    // compositor's last frame and the crash reads as a silent freeze. Forcing
    // the active VT back to KD_TEXT repaints the text console and makes every
    // graphic_console_write_fmt below actually appear on the monitor. It is
    // panic-safe: the repaint is best-effort try_lock and allocates nothing.
    kernel_hal::console::set_kd_mode(kernel_hal::console::KD_TEXT);

    // Use spin variant: interrupts are already off above, and try_lock silently
    // discards output if another CPU holds the lock — unacceptable in panic context.
    //
    // Mirror to the graphic console too: on a real bring-up box with only a
    // monitor (no serial capture), a serial-only panic is invisible and reads
    // as a silent freeze. graphic_console_write_fmt is a best-effort try_lock
    // that no-ops if the VT lock is held, so it can't deadlock the panic path.
    if let Some(loc) = info.location() {
        kernel_hal::console::serial_write_fmt_spin(format_args!(
            "\n\npanic cpu={} at {}:{}:{}\n",
            kernel_hal::cpu::cpu_id(),
            loc.file(),
            loc.line(),
            loc.column(),
        ));
        kernel_hal::console::graphic_console_write_fmt_spin(format_args!(
            "\n\n[PANIC] cpu={} at {}:{}:{}\n",
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
        kernel_hal::console::graphic_console_write_fmt_spin(format_args!(
            "\n\n[PANIC] cpu={}\n",
            kernel_hal::cpu::cpu_id(),
        ));
    }
    // `as_str()` returns None for any panic! with format arguments — use
    // Display on the Arguments directly so the message is always printed.
    kernel_hal::console::serial_write_fmt_spin(format_args!("{}\n", info.message()));
    kernel_hal::console::graphic_console_write_fmt_spin(format_args!("{}\n", info.message()));

    if cfg!(feature = "baremetal-test") {
        kernel_hal::cpu::reset();
    } else {
        loop {
            core::hint::spin_loop();
        }
    }
}
