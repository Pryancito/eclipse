//! `/dev/nvidiactl` and `/dev/nvidia<N>` with the RM in a process
//! (`udrv-nvidia`): the device files stay the kernel's
//! (`devfs::nvidia`), and what they ask of the RM goes over the same link a
//! served node's requests do. The format and both of its ends are in
//! `zcore-drivers` (`display::nvctl::remote`): this is only the link
//! under it, and the GPU nodes the process's GPUs get.
//!
//! A mapping of system memory the RM grants is mapped as the memory
//! itself, a slice of the VMO the process pinned ([`SystemVmo`]): the pages
//! stay the mapping's for as long as it lives, whatever the process frees
//! meanwhile or however it ends. An aperture is a window of a BAR, as with
//! the kernel's RM.
//!
//! Pages of the caller's that an escape lends the RM (an OS descriptor)
//! the kernel pins as it does for its own RM, and the loader makes
//! reachable to the process's GPUs ([`Lender`]) before the process is
//! told where they are. They are unpinned when it gives them back
//! ([`give_back`]).
//!
//! An escape lends the process the caller's memory for as long as it
//! waits: the RM follows the pointers in its parameters as it goes, and
//! the kernel copies each range for it (`ECLIPSE_DEV_MEMORY`), in the
//! caller's address space only, as the caller's own `read` and `write`
//! would.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use lock::Mutex;
use zcore_drivers::display::nvctl::remote::{self, Link as RmLink, Refused};
pub use zcore_drivers::display::nvctl::remote::{Lender, Loan, Owns, Server};
pub use zcore_drivers::display::nvctl::UnixEvent;
use zircon_object::ipc::Fifo;
use zircon_object::object::KernelObject;
use zircon_object::vm::VmObject;

use super::link::{Fail, Kill, Link};
use crate::error::{LxError, LxResult};
use crate::process::LinuxProcess;

/// The memory behind `len` bytes of the RM process's pinned memory at a
/// physical address, as a VMO of its own, `None` for memory it did not
/// pin. The loader gives it, knowing the process.
pub type SystemVmo = Arc<dyn Fn(u64, usize) -> Option<Arc<VmObject>> + Send + Sync>;

/// The served RM's [`SystemVmo`], while there is one.
static SYSTEM: Mutex<Option<SystemVmo>> = Mutex::new(None);

/// How a system-memory mapping is mapped: `None` with the RM in the
/// kernel (the pages are the kernel's, mapped where they are); with the
/// RM in a process, the memory it pinned there, or `Some(None)` for
/// memory it does not have any more.
pub fn system_vmo(start: u64, len: usize) -> Option<Option<Arc<VmObject>>> {
    let system = SYSTEM.lock().clone()?;
    Some(system(start, len))
}

/// `ENODEV`: the server is gone.
const ENODEV: i32 = 19;

/// The link, as the RM's.
struct OverLink(Arc<Link>);

impl RmLink for OverLink {
    fn call(&self, op: u32, a: u64, data: &[u8]) -> Result<(u64, Vec<u8>), i32> {
        // The caller's address space goes with the call: the RM reaches
        // into it while the call waits, and only then.
        match self.0.call_for(super::current_vmar(), op, 0, 0, a, 0, data) {
            Ok(answer) => {
                let mut bytes = vec![0u8; answer.len];
                answer.data(&mut bytes).map_err(|_| remote::EINVAL)?;
                Ok((answer.a, bytes))
            }
            Err(Fail::Said(errno)) => Err(errno as i32),
            Err(Fail::Gone) => Err(ENODEV),
        }
    }

    fn tell(&self, op: u32, a: u64) {
        self.0.tell_with(op, 0, a, 0);
    }
}

/// What [`serve_rm`] made: the RM, for whoever watches its server.
pub struct ServedRm {
    link: Arc<Link>,
    rm: Arc<dyn RmLink>,
}

impl ServedRm {
    /// Read what the server wrote. Whether it is still there.
    pub fn look(&self) -> bool {
        self.link.pump();
        !self.link.is_dead()
    }

    /// The process that serves the RM (`server`, its id), and how it is
    /// ended when it hangs.
    pub fn killed_with(&self, server: u64, kill: Kill) {
        self.link.killed_with(server, kill);
    }

    /// The server is gone, and its RM: the device files find none again.
    /// Its GPU nodes stay, as a GPU the RM never brought up keeps its
    /// node, and open as no GPU (`ENODEV`) until a server is back.
    pub fn gone(&self) {
        self.link.close();
        remote::gone(&self.rm);
        *SYSTEM.lock() = None;
        info!("[nvidiactl] the RM's process is gone");
    }
}

/// Take the RM the process at the other end of `fifo` serves, which says
/// what it has in `hello`, as the one `/dev/nvidiactl` reaches.
///
/// `server` says what of the memory the RM maps is the process's and lends
/// it pages, and `system` maps what it pinned.
///
/// `EPERM` for a process that may not; `ENODEV` with no `/dev/nvidiactl`
/// (`nvidia.nvidiactl` is off); `EEXIST` while the kernel's own RM is up or
/// another process serves one; `EINVAL` for a window that holds no slot, a
/// FIFO of other messages or a `hello` that does not parse.
pub fn serve_rm(
    proc: &LinuxProcess,
    fifo: Arc<Fifo>,
    window: Arc<VmObject>,
    hello: &[u8],
    server: Server,
    system: SystemVmo,
) -> LxResult<ServedRm> {
    if !proc.capable(crate::process::CAP_SYS_ADMIN) {
        return Err(LxError::EPERM);
    }
    if !zcore_drivers::display::nvctl::enabled() {
        return Err(LxError::ENODEV);
    }
    let koid = fifo.id();
    let link = Arc::new(Link::new(fifo, window).map_err(|_| LxError::EINVAL)?);
    let rm: Arc<dyn RmLink> = Arc::new(OverLink(link.clone()));
    // Before the RM is the device files': a mapping can be made at once.
    let mut slot = SYSTEM.lock();
    let cards = remote::serve(rm.clone(), hello, server).map_err(|e| match e {
        Refused::Taken => LxError::EEXIST,
        Refused::Invalid => LxError::EINVAL,
    })?;
    *slot = Some(system);
    drop(slot);
    super::remember(koid, &link);
    crate::fs::devfs::nvidia::add_gpu_nodes();
    info!(
        "[nvidiactl] the RM is a process's now, with {} GPU(s)",
        cards.len()
    );
    Ok(ServedRm { link, rm })
}

/// `ECLIPSE_NVCTL_RETURN`: the process `server` (its koid) gives back the
/// pages of loan `loan`. `false` for a loan it does not have, or a process
/// that does not serve the RM.
pub fn give_back(server: u64, loan: u64) -> bool {
    remote::give_back(server, loan)
}

/// `ECLIPSE_NVCTL_EVENT`: the process `server` (its koid) posts `event`
/// (none for one with no data, `osSetEvent`'s) to the open `os_info`, as
/// its RM's registrations say: queued there as the kernel's own RM's posts
/// are, and nothing for an open that has closed. `false` to a process that
/// does not serve the RM.
pub fn post_event(server: u64, os_info: usize, event: Option<UnixEvent>) -> bool {
    if !remote::is_server(server) {
        return false;
    }
    crate::fs::devfs::nvidia::post_event(os_info, event);
    true
}

/// `ECLIPSE_BOOT_DISPLAY`'s bytes: what the boot knows of the display, as
/// the kernel's NVIDIA driver was told, for the same driver in a process.
pub use zcore_drivers::display::boot_display::LEN as BOOT_DISPLAY_LEN;

/// [`BOOT_DISPLAY_LEN`] bytes of what this boot knows of the display.
pub fn boot_display() -> [u8; BOOT_DISPLAY_LEN] {
    zcore_drivers::display::boot_display::BootDisplay::this_boot().encode()
}

/// The most bytes of [`driver_flags`].
pub use zcore_drivers::display::driver_flags::MAX as DRIVER_FLAGS_MAX;
use zcore_drivers::display::driver_flags::{DriverFlags, CONSOLE_GSP, HWFLIP};

/// `ECLIPSE_DRIVER_FLAGS`'s bytes: the boot line's `nvidia.*` switches as
/// the kernel's NVIDIA driver was told them, for the same driver in a
/// process ([`for_a_process`]).
pub fn driver_flags() -> alloc::vec::Vec<u8> {
    for_a_process(
        DriverFlags::this_boot(),
        !super::card::ce_presents_allowed(),
        super::card::console_gate_is_the_windows(),
    )
    .encode()
}

/// Of the switches the kernel's driver has now, those a driver in a
/// process is told when it starts: all of them, but two.
///
/// `nvidia.hwflip`, when its GPUs are each behind an IOMMU domain of their
/// own (`behind_an_iommu`). With that switch a flip is the GPU's copy
/// engine copying the frame out of the kernel's memory, which is not in
/// such a GPU's domain: the copy reads nothing and is said done, the
/// kernel then leaves its own copy out, and no frame reaches the monitor.
/// Without it the driver declines those flips and the kernel copies the
/// frame, as it does for a card whose RM is not up.
///
/// The console-GSP gate, when the boot's deferred bring-up opened it
/// (`gate_is_the_windows`). That one is not the boot line's: it is open
/// for a window, and the bring-up's own request carries it to the driver
/// that is there then. A driver that starts afterwards and read it open
/// would bring the console GPU up for its first client, with the scanout
/// live and nothing held: what the window is there to prevent.
fn for_a_process(
    mut flags: DriverFlags,
    behind_an_iommu: bool,
    gate_is_the_windows: bool,
) -> DriverFlags {
    if behind_an_iommu {
        flags.bits &= !HWFLIP;
    }
    if gate_is_the_windows {
        flags.bits &= !CONSOLE_GSP;
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcore_drivers::display::driver_flags::NOUVEAU_UAPI;

    fn told() -> DriverFlags {
        DriverFlags {
            bits: NOUVEAU_UAPI | CONSOLE_GSP | HWFLIP | 1 << 5 | 1 << 12,
            registry: alloc::string::String::from("RMKey=0x1;"),
        }
    }

    /// A GPU behind an IOMMU cannot read the kernel's frames, so its
    /// driver is not told to flip by copying them; every other switch is
    /// told as it is, and so is that one to a GPU that reaches all memory.
    #[test]
    fn a_driver_behind_an_iommu_is_not_told_to_flip_by_copying_the_kernels_frames() {
        let isolated = for_a_process(told(), true, false);
        assert_eq!(isolated.bits, told().bits & !HWFLIP);
        assert_eq!(isolated.registry, told().registry);
        assert_eq!(for_a_process(told(), false, false), told());
    }

    /// The gate the boot line opened (`nvidia.console_gsp`) is told as it
    /// is. The one the deferred bring-up opened for its window is not: a
    /// driver that starts after the window must not bring the console GPU
    /// up by itself, at its first client.
    #[test]
    fn a_driver_is_not_told_the_gate_that_was_opened_for_the_bring_ups_window() {
        let late = for_a_process(told(), false, true);
        assert_eq!(late.bits, told().bits & !CONSOLE_GSP);
        assert_eq!(late.registry, told().registry);
        let both = for_a_process(told(), true, true);
        assert_eq!(both.bits, told().bits & !(HWFLIP | CONSOLE_GSP));
    }
}
