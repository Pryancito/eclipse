mod drivers;
#[cfg(feature = "graphic")]
mod early_fb_console;
mod smp;
mod trap;

pub mod config;
pub mod cpu;
pub mod interrupt;
pub mod mem;
pub mod timer;
pub mod vm;

#[doc(cfg(target_arch = "x86_64"))]
pub mod special;

hal_fn_impl! {
    impl mod crate::hal_fn::console {
        fn console_write_early(_s: &str) {
            #[cfg(feature = "graphic")]
            {
                // Note: this is within the kernel-hal crate, so we can reference the private
                // `imp::arch` module.
                crate::imp::arch::early_fb_console::write_str(_s);
            }
        }

        fn console_progress_early(_progress: u32) {
            #[cfg(feature = "graphic")]
            {
                crate::imp::arch::early_fb_console::draw_progress_bar(_progress);
            }
        }
    }
}

use crate::{mem::phys_to_virt, KCONFIG};
use x86_64::registers::control::{Cr4, Cr4Flags};

pub const fn timer_interrupt_vector() -> usize {
    trap::X86_INT_APIC_TIMER
}

pub fn cmdline() -> alloc::string::String {
    // `boot_info.cmdline` carries a *physical* pointer (rboot runs identity-mapped,
    // like `initrd_start`). Reading the `&str` directly only works while rboot's
    // identity map is still live; once the kernel switches to its own page tables
    // that low address is unmapped and the read faults (observed as a kernel page
    // fault on a ~physical address during boot). Read it through the physmap,
    // which always maps all RAM, so it is valid regardless of the identity map.
    let s = KCONFIG.cmdline;
    let virt = phys_to_virt(s.as_ptr() as usize);
    let bytes = unsafe { core::slice::from_raw_parts(virt as *const u8, s.len()) };
    alloc::string::String::from_utf8_lossy(bytes).into_owned()
}

pub fn init_ram_disk() -> Option<&'static mut [u8]> {
    if KCONFIG.initrd_start == 0 || KCONFIG.initrd_size == 0 {
        return None;
    }
    let start = phys_to_virt(KCONFIG.initrd_start as usize);
    Some(unsafe { core::slice::from_raw_parts_mut(start as *mut u8, KCONFIG.initrd_size as usize) })
}

pub fn primary_init_early() {
    // init serial output first
    drivers::init_early().unwrap();
}

pub fn primary_init() {
    drivers::init().unwrap();
    warn!("[boot] drivers init complete");
    unsafe {
        // enable global page
        Cr4::update(|f| f.insert(Cr4Flags::PAGE_GLOBAL));
    }
    smp::start_application_processors();
    warn!("[boot] smp init complete");
}

pub fn timer_init() {
    timer::init();
}

pub fn secondary_init() {
    zcore_drivers::irq::x86::Apic::init_local_apic_ap();
    smp::ap_signal_online();
}

/// Dense logical CPU id for the AP currently starting (trampoline slot).
pub fn ap_trampoline_logical_id() -> u8 {
    smp::ap_trampoline_logical_id()
}

/// Release the BSP to reuse the shared trampoline slots: called by the starting
/// AP the instant it has latched its logical id out of the slot.
pub fn ap_signal_slot_consumed() {
    smp::ap_signal_slot_consumed();
}
