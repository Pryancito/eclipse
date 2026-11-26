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
//! An escape lends the process the caller's memory for as long as it
//! waits: the RM follows the pointers in its parameters as it goes, and
//! the kernel copies each range for it (`ECLIPSE_DEV_MEMORY`), in the
//! caller's address space only, as the caller's own `read` and `write`
//! would.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use lock::Mutex;
pub use zcore_drivers::display::nvctl::remote::Owns;
use zcore_drivers::display::nvctl::remote::{self, Link as RmLink, Refused};
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

    /// How the server is ended when it hangs.
    pub fn killed_with(&self, kill: Kill) {
        self.link.killed_with(kill);
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
/// `owns` says what of the memory the RM maps is the process's, and
/// `system` maps what it pinned.
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
    owns: Owns,
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
    let cards = remote::serve(rm.clone(), hello, owns).map_err(|e| match e {
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
