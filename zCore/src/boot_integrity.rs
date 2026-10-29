//! The kernel's side of hunter's boot integrity (`hunter::boot`): hands over
//! what rboot measured, measures the kernel's own code, and re-checks that code
//! while the machine runs.
//!
//! The comparison against `/etc/hunter/boot` is not here: it needs the root
//! filesystem and the operator's `policy.conf`, so `hunter_config::load` runs
//! it once both are read. This file only makes sure the measurements are in
//! place before that.
//!
//! ## The kernel's code
//!
//! `.text` and `.rodata` are hashed as mapped, right after hunter comes up,
//! and that digest is the `kernel-code` measurement. Nothing in this kernel
//! writes either range after boot -- no alternatives patching, no static keys,
//! no live patching; relocations are resolved at link time, and the one
//! post-link patch (`.ksyms`) is a section of its own, applied to the file --
//! so a digest that changes later is code or constants patched in memory,
//! which is how a kernel rootkit hooks a function. The watch re-hashes every
//! [`WATCH_PERIOD_S`] seconds and `hunterctl boot verify` does it on demand.
//!
//! A software debugger that plants `int3` in `.text` (gdb over KVM's gdbstub)
//! trips this too, and should: from inside the guest it is indistinguishable
//! from a hook.

#[cfg(all(target_os = "none", target_arch = "x86_64"))]
static LOADER: spin::Mutex<Option<rboot::BootMeasurements>> = spin::Mutex::new(None);

/// Called from `_start` with what rboot handed over. Kept until hunter is up:
/// the record lives in `BootInfo`, on memory the kernel reclaims later.
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
pub fn set_loader_measurements(m: rboot::BootMeasurements) {
    *LOADER.lock() = Some(m);
}

/// Seconds between re-checks of the kernel's code. Hashing `.text` and
/// `.rodata` is tens of milliseconds of one CPU on real hardware; every ten
/// minutes is noise, and it bounds how long a patched kernel goes unnoticed
/// by someone not asking.
#[cfg(feature = "linux")]
const WATCH_PERIOD_S: u64 = 600;

/// Hands hunter this boot's measurements. Call right after `hunter::init`.
pub fn init() {
    #[cfg(all(target_os = "none", target_arch = "x86_64"))]
    {
        if let Some(m) = LOADER.lock().take().filter(|m| m.is_valid()) {
            hunter::boot::set_loader("rboot");
            for (name, item) in [
                ("kernel", m.kernel),
                ("initramfs", m.initramfs),
                ("rboot.conf", m.config),
                ("cmdline", m.cmdline),
            ] {
                if item.measured {
                    hunter::boot::record_measurement(name, item.digest, item.size);
                }
            }
        }
        let (text, rodata) = code_ranges();
        let size = (text.len() + rodata.len()) as u64;
        hunter::boot::set_code_measurer(measure_code, size);
        if let Some(m) = hunter::boot::measured(hunter::boot::KERNEL_CODE) {
            kernel_hal::klog_info!(
                "hunter: kernel code {} ({} bytes of .text + .rodata)",
                hunter::integrity::to_hex(&m.digest),
                m.size
            );
        }
    }
}

/// Starts the periodic re-check of the kernel's code.
#[cfg(feature = "linux")]
pub fn start_watch() {
    use core::time::Duration;
    use kernel_hal::timer::timer_now;
    kernel_hal::thread::spawn(async {
        loop {
            kernel_hal::thread::sleep_until(timer_now() + Duration::from_secs(WATCH_PERIOD_S))
                .await;
            hunter::boot::verify_code();
        }
    });
}

/// The kernel image's `.text` and `.rodata`, as mapped (see `linker.ld`).
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
fn code_ranges() -> (&'static [u8], &'static [u8]) {
    extern "C" {
        fn stext();
        fn etext();
        fn srodata();
        fn erodata();
    }
    let span = |lo: unsafe extern "C" fn(), hi: unsafe extern "C" fn()| {
        let (lo, hi) = (lo as *const () as usize, hi as *const () as usize);
        // SAFETY: the linker places both symbols around one loaded section,
        // which rboot maps for the kernel's whole life and nothing unmaps.
        unsafe { core::slice::from_raw_parts(lo as *const u8, hi.saturating_sub(lo)) }
    };
    (span(stext, etext), span(srodata, erodata))
}

/// SHA-256 of `.text` followed by `.rodata`.
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
fn measure_code() -> Option<hunter::integrity::Digest> {
    let (text, rodata) = code_ranges();
    hunter::integrity::sha256_image(text.len() + rodata.len(), &mut |off, buf| {
        // The two ranges as one stream: a chunk may straddle the seam.
        let (t, end) = (text.len(), off + buf.len());
        if end <= t {
            buf.copy_from_slice(&text[off..end]);
        } else if off >= t {
            buf.copy_from_slice(&rodata[off - t..end - t]);
        } else {
            let (head, tail) = buf.split_at_mut(t - off);
            head.copy_from_slice(&text[off..]);
            tail.copy_from_slice(&rodata[..end - t]);
        }
        true
    })
}
