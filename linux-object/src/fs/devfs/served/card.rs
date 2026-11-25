//! A DRM card whose driver is a process (`udrv-nvidia`): the DRM core
//! sees one more card, and each call it makes of it goes over the same
//! link a served device node's requests do.
//!
//! The format and both of its ends are in `zcore-drivers`
//! (`scheme::drm_remote`): this is only the link under it, and the card's
//! place in the DRM core's list and in `/dev/dri`.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use rcore_fs::vfs::INode;
use zcore_drivers::scheme::drm_remote::{self, Remote, Transport};
use zcore_drivers::scheme::DrmScheme;
use zircon_object::ipc::Fifo;
use zircon_object::object::KernelObject;
use zircon_object::vm::VmObject;

use super::link::{Fail, Kill, Link};
use crate::error::{LxError, LxResult};
use crate::fs::devfs::drm;
use crate::process::LinuxProcess;

/// The link, as the card's transport. Each answer carries the card's
/// flags in its `fh`, kept here so the DRM core can read them under its
/// lock without a call (a call waits, and must not wait holding a lock).
pub struct OverLink(Arc<Link>, AtomicU64);

/// No answer has carried flags yet.
const NO_FLAGS: u64 = u64::MAX;

impl OverLink {
    pub(super) fn new(link: Arc<Link>) -> Self {
        Self(link, AtomicU64::new(NO_FLAGS))
    }
}

impl Transport for OverLink {
    fn call(&self, op: u32, a: u64, b: u64, data: &[u8]) -> Result<(u64, Vec<u8>), i32> {
        // The message's `len` is the bytes it carries; the answer has the
        // whole slot.
        match self.0.call(op, 0, 0, a, b, data) {
            Ok(answer) => {
                self.1.store(answer.fh as u64, Ordering::Relaxed);
                let mut bytes = vec![0u8; answer.len];
                answer.data(&mut bytes).map_err(|_| drm_remote::EINVAL)?;
                Ok((answer.a, bytes))
            }
            Err(Fail::Said(errno)) => Err(errno as i32),
            Err(Fail::Gone) => Err(drm_remote::ENODEV),
        }
    }

    fn flags(&self) -> Option<drm_remote::Flags> {
        match self.1.load(Ordering::Relaxed) {
            NO_FLAGS => None,
            bits => Some(drm_remote::Flags(bits)),
        }
    }
}

/// What [`serve_card`] made: the card, for whoever watches its server.
pub struct ServedCard {
    link: Arc<Link>,
    card: Arc<dyn DrmScheme>,
}

impl ServedCard {
    /// Read what the server wrote. Whether it is still there.
    pub fn look(&self) -> bool {
        self.link.pump();
        !self.link.is_dead()
    }

    /// How the server is ended when it hangs.
    pub fn killed_with(&self, kill: Kill) {
        self.link.killed_with(kill);
    }

    /// The server is gone: the card leaves the DRM core, and its node
    /// answers no more.
    pub fn gone(&self) {
        self.link.close();
        drm::unregister_driver(&self.card);
        drm::build_gpu_nodes();
        info!("served drm: the card {} is gone", self.card.name());
    }
}

/// Make the card the process at the other end of `fifo` drives, and put it
/// beside the others: in the DRM core's list and as a node in `/dev/dri`.
/// `hello` is what the driver says of itself (`drm_remote::op::HELLO`'s
/// answer, sent with the call: the server cannot answer while it makes it).
/// `EPERM` for a process that may not; `EINVAL` for a window that holds no
/// slot, a FIFO of other messages or a `hello` that does not parse.
pub fn serve_card(
    proc: &LinuxProcess,
    fifo: Arc<Fifo>,
    window: Arc<VmObject>,
    hello: &[u8],
) -> LxResult<ServedCard> {
    if !proc.capable(crate::process::CAP_SYS_ADMIN) {
        return Err(LxError::EPERM);
    }
    let koid = fifo.id();
    let link = Arc::new(Link::new(fifo, window).map_err(|_| LxError::EINVAL)?);
    let remote =
        Remote::with_hello(OverLink::new(link.clone()), hello).map_err(|_| LxError::EINVAL)?;
    super::remember(koid, &link);
    let card: Arc<dyn DrmScheme> = Arc::new(remote);
    drm::register_driver(card.clone());
    drm::build_gpu_nodes();
    add_nodes();
    info!("served drm: the card {} is served", card.name());
    Ok(ServedCard { link, card })
}

/// A `/dev/dri` node for every card that has none: one served after the
/// boot made the nodes, and `/dev/dri` itself on a machine whose boot found
/// no card to make it for.
fn add_nodes() {
    use rcore_fs_devfs::DevINode;
    let Some(root) = crate::fs::DEVFS_ROOT.lock().clone() else {
        return;
    };
    let found = (root.clone() as Arc<dyn INode>).find("dri");
    let dri: Arc<dyn INode> = match found {
        Ok(dri) => dri,
        Err(_) => match root.add_dir("dri") {
            Ok(dir) => dir,
            // Made by a racing serve between the look and the add.
            Err(_) => match (root as Arc<dyn INode>).find("dri") {
                Ok(dri) => dri,
                Err(_) => return,
            },
        },
    };
    let Some(dir) = dri.downcast_ref::<DevINode>() else {
        return;
    };
    for node in drm::gpu_nodes() {
        for minor in [node.card_minor(), node.render_minor()] {
            let name = drm::node_name(minor);
            if dri.find(&name).is_err() {
                let _ = dir.add(&name, Arc::new(crate::fs::devfs::DrmDev::new(minor)));
            }
        }
    }
}

#[cfg(test)]
mod tests;
