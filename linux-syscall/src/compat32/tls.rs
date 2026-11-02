//! `set_thread_area(2)` and `get_thread_area(2)`: where a 32-bit thread keeps
//! its thread pointer.
//!
//! On x86_64 a thread pointer is a number in an MSR, and `arch_prctl`
//! (`ARCH_SET_FS`) writes it. On i386 there are no such MSRs, so the thread
//! pointer is the **base of a segment**, and reaching it means loading a
//! selector into `%gs` and letting the CPU read the base out of a descriptor
//! table. That is the whole reason these two calls exist, and the reason they
//! cannot be answered by translating a number to a native one: there is no
//! native call that does this.
//!
//! So the kernel keeps three descriptors per thread, as Linux does
//! (`GDT_ENTRY_TLS_MIN..=MAX`), and writes them into the running CPU's GDT on
//! the way out to user mode (`kernel_hal`'s `enter_uspace`, Linux's
//! `load_TLS`). The thread owns the descriptors; the GDT slots are scratch
//! space for whoever is running now.
//!
//! ## The selector, and the one thing the kernel cannot see
//!
//! `set_thread_area` hands back an `entry_number`, and ring 3 turns it into a
//! selector itself: musl's `__set_thread_area` does
//!
//! ```text
//!     movl $243, %eax         # set_thread_area
//!     int $0x80
//!     movw %dx, %gs           # selector = (entry_number << 3) | 3
//! ```
//!
//! and the kernel never sees that `movw`. It has to know the selector anyway,
//! because `iretq` does not restore `%gs` (it reloads `CS` and `SS` and
//! nothing else), so the return path has to put it back from the saved
//! context. The kernel therefore records the selector for the entry it just
//! handed out, which is the one ring 3 is about to load. A thread that loads
//! some other selector of its own accord is not followed -- that would take a
//! trap on every write to a segment register -- and Linux makes the same
//! bargain for the same reason.

use super::*;
use kernel_hal::context::CompatUserDesc;
use linux_object::error::LxError;

impl Syscall<'_> {
    /// `set_thread_area(2)`, i386 243.
    ///
    /// `-1` as the `entry_number` means "pick a free slot and tell me which",
    /// which is what every C library does: the slot numbers are not something
    /// a program can know. Any other value has to name a slot this thread has.
    pub fn sys_set_thread_area(&mut self, desc: usize) -> SysResult {
        let slots = kernel_hal::context::UserContext::compat_tls_slots();
        let user: UserInOutPtr<u8> = desc.into();
        let bytes = user.read_array(16)?;
        let mut info = CompatUserDesc::from_bytes(&to_array(&bytes));

        // `entry_number` is a GDT index, not a slot number of the kernel's
        // choosing: ring 3 turns it into a selector itself. So -1 is answered
        // with the index of a free slot, and any other value has to name one.
        let slot = if info.entry_number == u32::MAX {
            let free = self
                .thread
                .with_context(|ctx| (0..slots).find(|&slot| ctx.compat_tls(slot).is_none()))?;
            // Every slot taken. Linux answers `ESRCH` here, which reads oddly
            // for "no free thread area" and is nonetheless the number glibc
            // and musl expect, so it is the number to give.
            free.ok_or(LxError::ESRCH)?
        } else {
            kernel_hal::context::compat_tls_slot_of(info.entry_number).ok_or(LxError::EINVAL)?
        };

        let descriptor = info.to_descriptor();
        let selector = self
            .thread
            .with_context(|ctx| {
                let selector = ctx.set_compat_tls(slot, descriptor);
                // An empty descriptor is a thread giving a slot back, so the
                // selector it had must not stay in the saved `%gs`: the return
                // path would reload a selector whose descriptor is now zero,
                // and `iretq` would fault in the kernel rather than in the
                // thread that asked for it.
                match (descriptor, selector) {
                    (0, _) => ctx.set_compat_gs(0),
                    (_, Some(selector)) => ctx.set_compat_gs(selector),
                    (_, None) => {}
                }
                selector
            })?
            .ok_or(LxError::EINVAL)?;

        let entry_number = kernel_hal::context::compat_tls_entry_number(slot);
        info!(
            "sys_set_thread_area: entry {} base {:#x} limit {:#x} -> selector {:#x}",
            entry_number, info.base_addr, info.limit, selector
        );

        // The chosen entry goes back to the caller, which is the whole point
        // of passing `-1`. Written even when the caller named it itself: the
        // value is then the same one it passed, and writing it unconditionally
        // keeps the two paths from differing in what the structure holds
        // afterwards.
        info.entry_number = entry_number;
        let mut out: UserInOutPtr<u8> = desc.into();
        out.write_array(&info.to_bytes())?;
        Ok(0)
    }

    /// `get_thread_area(2)`, i386 244: the descriptor in one of this thread's
    /// slots, read back as the `user_desc` it was built from.
    ///
    /// A slot nobody claimed is not an error. It reads back as a not-present
    /// descriptor with nothing else set, which is Linux's answer and the
    /// `user_desc` that `set_thread_area` would take as "clear this slot".
    pub fn sys_get_thread_area(&mut self, desc: usize) -> SysResult {
        let user: UserInOutPtr<u8> = desc.into();
        let bytes = user.read_array(16)?;
        let entry_number = CompatUserDesc::from_bytes(&to_array(&bytes)).entry_number;
        let slot = kernel_hal::context::compat_tls_slot_of(entry_number).ok_or(LxError::EINVAL)?;

        let installed = self.thread.with_context(|ctx| ctx.compat_tls(slot))?;
        let info = match installed {
            Some(descriptor) => CompatUserDesc::from_descriptor(entry_number, descriptor),
            None => CompatUserDesc::empty(entry_number),
        };
        let mut out: UserInOutPtr<u8> = desc.into();
        out.write_array(&info.to_bytes())?;
        Ok(0)
    }
}

/// The sixteen bytes as an array, for [`CompatUserDesc::from_bytes`].
///
/// `read_array` hands back a `Vec`, and the length is already known to be 16
/// because that is what was asked for; this is the one place that has to say
/// so to the type system.
fn to_array(bytes: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes[..16]);
    out
}
