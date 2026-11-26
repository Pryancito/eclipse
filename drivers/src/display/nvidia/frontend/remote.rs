//! `/dev/nvidiactl` and `/dev/nvidia<N>` with the RM in a process
//! (`udrv-nvidia`): the kernel keeps the device files, as it keeps the
//! nv.c half of them (`linux_object::fs::devfs::nvidia`), and what it asks
//! of the RM goes over a link to the process, whose RM answers it as the
//! kernel's would ([`answer`]).
//!
//! What the RM reads and writes of the caller's memory (the parameters an
//! escape points to, as deep as they go) the kernel copies for it while
//! the call waits: `nvidia_rm_sys::frontend::set_user_copy` in the
//! process, over the request the call is.
//!
//! The process says what it has in [`hello`] when it starts serving: whether
//! its RM is up and its GPUs, which the kernel keeps for the device files
//! to read without a call. One RM only: there is no serving while the
//! kernel's own is up, and no second server.
//!
//! A mapping the RM grants (`RM_MAP_MEMORY`) is the kernel's to check
//! before it is recorded: an aperture has to lie in a BAR of a GPU the
//! process holds, system memory in what it pinned (the loader says which,
//! [`Owns`]), or the RM is told it was not recorded.
//!
//! This carries the plain escapes, the mappings and `release`. The others
//! (lent pages, OS events, UVM) are refused with `NV_ERR_NOT_SUPPORTED`
//! until they are carried too.

use super::{Caller, Card, MmapContext, MmapMemory, RawMmapContext, CALLER_NAME_LEN, GPU_UUID_LEN};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::convert::TryFrom;
use lock::Mutex;
use nvidia_rm_sys::types::{NV_ERR_INVALID_STATE, NV_ERR_OPERATING_SYSTEM, NV_STATUS};

/// The link's operations, numbered apart from a served node's (`1..=16`)
/// and a served DRM card's (`64..`).
pub mod op {
    /// `a` = the escape's number; the message is the [`Caller`]
    /// ([`super::CALLER_LEN`] bytes) and then the payload. The answer's
    /// `a` is the RM's status, its bytes the payload as the RM left it.
    pub const RM_ESCAPE: u32 = 128;
    /// `a` = the `os_info` of an open that is gone. Told, not answered.
    pub const RELEASE: u32 = 129;
    /// `RM_MAP_MEMORY`: as [`RM_ESCAPE`], and the answer's bytes are the
    /// payload and then the mapping the RM granted
    /// ([`super::MAPPING_LEN`] bytes, `valid` 0 for none).
    pub const RM_MAP_MEMORY: u32 = 130;
    /// The kernel did not record the mapping: `a` = why; the message and
    /// the answer as [`RM_ESCAPE`]'s.
    pub const MAP_UNRECORDED: u32 = 131;
}

/// `EINVAL`, `ENOSYS`: what a message that is not one is answered with.
pub const EINVAL: i32 = 22;
pub const ENOSYS: i32 = 38;

/// The kernel's end of the link.
pub trait Link: Send + Sync {
    /// Ask, and wait for the answer: its `a` and its bytes, or an errno
    /// (the server's, or `ENODEV` for one that is gone).
    fn call(&self, op: u32, a: u64, data: &[u8]) -> Result<(u64, Vec<u8>), i32>;
    /// Say, and wait for nothing.
    fn tell(&self, op: u32, a: u64);
}

/// A GPU as the process's RM has it: the card, and what its RM calls it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServedCard {
    pub card: Card,
    pub gpu_id: u32,
    pub uuid: Option<[u8; GPU_UUID_LEN]>,
}

/// Whose `len` bytes of physical memory at `start` are, as the loader
/// knows of the process: `Some(true)` for a BAR of a device it holds,
/// `Some(false)` for memory it pinned, `None` for neither.
pub type Owns = Arc<dyn Fn(u64, u64) -> Option<bool> + Send + Sync>;

pub(super) struct Served {
    link: Arc<dyn Link>,
    owns: Owns,
    rm_up: bool,
    cards: Vec<ServedCard>,
}

static SERVED: Mutex<Option<Arc<Served>>> = Mutex::new(None);

/// The process serving the RM, if one is. Cloned out: nothing is called
/// under the lock.
pub(super) fn served() -> Option<Arc<Served>> {
    SERVED.lock().clone()
}

/// Why [`serve`] said no.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The kernel's own RM is up, or another process serves one.
    Taken,
    /// A `hello` that does not parse.
    Invalid,
}

/// Take the RM of the process at the other end of `link`, which says what
/// it has in `hello`. Its GPUs are the cards from then on.
pub fn serve(link: Arc<dyn Link>, hello: &[u8], owns: Owns) -> Result<Vec<ServedCard>, Refused> {
    let (rm_up, cards) = parse_hello(hello).ok_or(Refused::Invalid)?;
    if super::local_rm_up() {
        return Err(Refused::Taken);
    }
    let mut served = SERVED.lock();
    if served.is_some() {
        return Err(Refused::Taken);
    }
    *served = Some(Arc::new(Served {
        link,
        owns,
        rm_up,
        cards: cards.clone(),
    }));
    Ok(cards)
}

/// The process behind `link` is gone, and its RM with it: the device files
/// find no RM and no GPUs again.
pub fn gone(link: &Arc<dyn Link>) {
    let mut served = SERVED.lock();
    if served.as_ref().is_some_and(|s| Arc::ptr_eq(&s.link, link)) {
        *served = None;
    }
}

impl Served {
    pub(super) fn rm_up(&self) -> bool {
        self.rm_up
    }

    pub(super) fn cards(&self) -> Vec<Card> {
        self.cards.iter().map(|c| c.card).collect()
    }

    /// The card whose RM instance is `instance`.
    pub(super) fn by_instance(&self, instance: u32) -> Option<&ServedCard> {
        self.cards
            .iter()
            .find(|c| c.card.instance == Some(instance))
    }

    pub(super) fn rm_escape(&self, cmd: u32, payload: &mut [u8], caller: &Caller) -> NV_STATUS {
        let msg = with_caller(caller, payload);
        match self.link.call(op::RM_ESCAPE, cmd as u64, &msg) {
            Ok((status, back)) if back.len() == payload.len() => {
                payload.copy_from_slice(&back);
                status as NV_STATUS
            }
            Ok(_) => {
                warn!("[nvidiactl] escape {cmd:#x}: the RM's process answered another size");
                NV_ERR_OPERATING_SYSTEM
            }
            Err(errno) => {
                warn!("[nvidiactl] escape {cmd:#x}: the RM's process did not answer ({errno})");
                NV_ERR_OPERATING_SYSTEM
            }
        }
    }

    pub(super) fn release(&self, os_info: usize) {
        self.link.tell(op::RELEASE, os_info as u64);
    }

    pub(super) fn rm_map_memory(
        &self,
        payload: &mut [u8],
        caller: &Caller,
    ) -> (NV_STATUS, Option<MmapContext>) {
        let msg = with_caller(caller, payload);
        let (status, back) = match self.link.call(op::RM_MAP_MEMORY, 0, &msg) {
            Ok(answer) => answer,
            Err(errno) => {
                warn!("[nvidiactl] RM_MAP_MEMORY: the RM's process did not answer ({errno})");
                return (NV_ERR_OPERATING_SYSTEM, None);
            }
        };
        if back.len() != payload.len() + MAPPING_LEN {
            warn!("[nvidiactl] RM_MAP_MEMORY: the RM's process answered another size");
            return (NV_ERR_OPERATING_SYSTEM, None);
        }
        payload.copy_from_slice(&back[..payload.len()]);
        let raw = decode_mapping(&back[payload.len()..]);
        let mapping = MmapContext::from_raw(&raw);
        match mapping {
            None if raw.valid != 0 => {
                warn!("[nvidiactl] RM_MAP_MEMORY: a mapping that is not one ({raw:x?})");
                self.map_unrecorded(payload, caller, NV_ERR_INVALID_STATE);
                (NV_ERR_INVALID_STATE, None)
            }
            Some(m) if !self.may_map(&m) => {
                warn!(
                    "[nvidiactl] RM_MAP_MEMORY: {:?} {:#x}+{:#x} is not the RM's process's",
                    m.memory, m.start, m.size
                );
                self.map_unrecorded(payload, caller, NV_ERR_INVALID_STATE);
                (NV_ERR_INVALID_STATE, None)
            }
            mapping => (status as NV_STATUS, mapping),
        }
    }

    /// An aperture in a BAR of a GPU the process holds, system memory in
    /// what it pinned.
    fn may_map(&self, m: &MmapContext) -> bool {
        let device = m.memory != MmapMemory::System;
        (self.owns)(m.start, m.size) == Some(device)
    }

    pub(super) fn map_unrecorded(&self, payload: &mut [u8], caller: &Caller, why: NV_STATUS) {
        let msg = with_caller(caller, payload);
        match self.link.call(op::MAP_UNRECORDED, why as u64, &msg) {
            Ok((_, back)) if back.len() == payload.len() => payload.copy_from_slice(&back),
            Ok(_) => warn!("[nvidiactl] MAP_UNRECORDED: the RM's process answered another size"),
            Err(errno) => {
                warn!("[nvidiactl] MAP_UNRECORDED: the RM's process did not answer ({errno})")
            }
        }
    }
}

fn with_caller(caller: &Caller, payload: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(CALLER_LEN + payload.len());
    msg.extend_from_slice(&encode_caller(caller));
    msg.extend_from_slice(payload);
    msg
}

/// A mapping on the link: `RawMmapContext`'s words, in its order.
pub const MAPPING_LEN: usize = 40;

fn encode_mapping(m: Option<&MmapContext>) -> [u8; MAPPING_LEN] {
    let raw = m.map(MmapContext::to_raw).unwrap_or_default();
    let mut b = [0u8; MAPPING_LEN];
    let words = [
        raw.valid,
        raw.ctl,
        raw.gpu_instance,
        raw.memory_type,
        raw.caching,
        raw.prot,
    ];
    for (i, w) in words.iter().enumerate() {
        b[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    b[24..32].copy_from_slice(&raw.start.to_le_bytes());
    b[32..40].copy_from_slice(&raw.size.to_le_bytes());
    b
}

fn decode_mapping(b: &[u8]) -> RawMmapContext {
    RawMmapContext {
        valid: u32_at(b, 0),
        ctl: u32_at(b, 4),
        gpu_instance: u32_at(b, 8),
        memory_type: u32_at(b, 12),
        caching: u32_at(b, 16),
        prot: u32_at(b, 20),
        start: u64_at(b, 24),
        size: u64_at(b, 32),
    }
}

/// The RM a process serves, as [`answer`] reaches it.
pub trait Rm {
    fn escape(&self, cmd: u32, payload: &mut [u8], caller: &Caller) -> NV_STATUS;
    fn release(&self, os_info: usize);
    fn map_memory(&self, payload: &mut [u8], caller: &Caller) -> (NV_STATUS, Option<MmapContext>);
    fn map_unrecorded(&self, payload: &mut [u8], caller: &Caller, why: NV_STATUS);
}

/// This address space's RM: the driver's own.
pub struct ThisRm;

impl Rm for ThisRm {
    fn escape(&self, cmd: u32, payload: &mut [u8], caller: &Caller) -> NV_STATUS {
        super::rm_escape(cmd, payload, caller)
    }

    fn release(&self, os_info: usize) {
        super::release(os_info)
    }

    fn map_memory(&self, payload: &mut [u8], caller: &Caller) -> (NV_STATUS, Option<MmapContext>) {
        super::rm_map_memory(payload, caller)
    }

    fn map_unrecorded(&self, payload: &mut [u8], caller: &Caller, why: NV_STATUS) {
        super::map_unrecorded(payload, caller, why)
    }
}

/// The server's end: the answer to message `op` with `a` and `data`, its
/// bytes in `out`. `Err` is the errno the kernel is told.
pub fn answer(rm: &dyn Rm, op: u32, a: u64, data: &[u8], out: &mut Vec<u8>) -> Result<u64, i32> {
    let with_payload = |out: &mut Vec<u8>| -> Result<Caller, i32> {
        let caller = data
            .get(..CALLER_LEN)
            .and_then(decode_caller)
            .ok_or(EINVAL)?;
        out.clear();
        out.extend_from_slice(&data[CALLER_LEN..]);
        Ok(caller)
    };
    match op {
        op::RM_ESCAPE => {
            let cmd = u32::try_from(a).map_err(|_| EINVAL)?;
            let caller = with_payload(out)?;
            Ok(rm.escape(cmd, out, &caller) as u64)
        }
        op::RM_MAP_MEMORY => {
            let caller = with_payload(out)?;
            let (status, mapping) = rm.map_memory(out, &caller);
            out.extend_from_slice(&encode_mapping(mapping.as_ref()));
            Ok(status as u64)
        }
        op::MAP_UNRECORDED => {
            let why = u32::try_from(a).map_err(|_| EINVAL)?;
            let caller = with_payload(out)?;
            rm.map_unrecorded(out, &caller, why);
            Ok(0)
        }
        op::RELEASE => {
            rm.release(a as usize);
            Ok(0)
        }
        _ => Err(ENOSYS),
    }
}

/// What this process's RM says of itself, for `ECLIPSE_NVCTL_SERVE`.
pub fn hello() -> Vec<u8> {
    let cards: Vec<ServedCard> = super::cards()
        .into_iter()
        .map(|card| ServedCard {
            card,
            gpu_id: card.gpu_id(),
            uuid: card.uuid(),
        })
        .collect();
    hello_of(super::rm_up(), &cards)
}

/// `"NVCT"`, then the format's version.
const MAGIC: u32 = 0x5443_564e;
const VERSION: u32 = 1;
const HEAD_LEN: usize = 16;
const CARD_LEN: usize = 80;
/// The most GPUs a `hello` lists.
const CARDS_MAX: usize = 32;

pub fn hello_of(rm_up: bool, cards: &[ServedCard]) -> Vec<u8> {
    let mut b = Vec::with_capacity(HEAD_LEN + cards.len() * CARD_LEN);
    for word in [MAGIC, VERSION, rm_up as u32, cards.len() as u32] {
        b.extend_from_slice(&word.to_le_bytes());
    }
    for c in cards {
        let card = &c.card;
        let at = b.len();
        b.resize(at + CARD_LEN, 0);
        let e = &mut b[at..];
        e[0..4].copy_from_slice(&card.minor.to_le_bytes());
        e[4..8].copy_from_slice(&card.domain.to_le_bytes());
        e[8] = card.bus;
        e[9] = card.slot;
        e[10..12].copy_from_slice(&card.device_id.to_le_bytes());
        e[12..16].copy_from_slice(&(card.instance.is_some() as u32).to_le_bytes());
        e[16..20].copy_from_slice(&card.instance.unwrap_or(0).to_le_bytes());
        e[20..24].copy_from_slice(&c.gpu_id.to_le_bytes());
        e[24..28].copy_from_slice(&(c.uuid.is_some() as u32).to_le_bytes());
        e[32..40].copy_from_slice(&card.reg_address.to_le_bytes());
        e[40..48].copy_from_slice(&card.reg_size.to_le_bytes());
        e[48..56].copy_from_slice(&card.fb_address.to_le_bytes());
        e[56..64].copy_from_slice(&card.fb_size.to_le_bytes());
        e[64..80].copy_from_slice(&c.uuid.unwrap_or([0; GPU_UUID_LEN]));
    }
    b
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from(u32_at(b, at)) | u64::from(u32_at(b, at + 4)) << 32
}

pub fn parse_hello(b: &[u8]) -> Option<(bool, Vec<ServedCard>)> {
    if b.len() < HEAD_LEN || u32_at(b, 0) != MAGIC || u32_at(b, 4) != VERSION {
        return None;
    }
    let rm_up = match u32_at(b, 8) {
        0 => false,
        1 => true,
        _ => return None,
    };
    let count = u32_at(b, 12) as usize;
    if count > CARDS_MAX || b.len() != HEAD_LEN + count * CARD_LEN {
        return None;
    }
    let mut cards = Vec::with_capacity(count);
    for e in b[HEAD_LEN..].chunks(CARD_LEN) {
        let flag = |at| match u32_at(e, at) {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        };
        let mut uuid = [0u8; GPU_UUID_LEN];
        uuid.copy_from_slice(&e[64..80]);
        let card = Card {
            minor: u32_at(e, 0),
            domain: u32_at(e, 4),
            bus: e[8],
            slot: e[9],
            device_id: u16::from_le_bytes([e[10], e[11]]),
            reg_address: u64_at(e, 32),
            reg_size: u64_at(e, 40),
            fb_address: u64_at(e, 48),
            fb_size: u64_at(e, 56),
            instance: flag(12)?.then(|| u32_at(e, 16)),
        };
        // Each card's minor is its place, as `cards()` lists them.
        if card.minor as usize != cards.len() {
            return None;
        }
        cards.push(ServedCard {
            card,
            gpu_id: u32_at(e, 20),
            uuid: flag(24)?.then_some(uuid),
        });
    }
    Some((rm_up, cards))
}

/// A [`Caller`] on the link.
pub const CALLER_LEN: usize = 24 + CALLER_NAME_LEN;

fn encode_caller(c: &Caller) -> [u8; CALLER_LEN] {
    let mut b = [0u8; CALLER_LEN];
    b[0..8].copy_from_slice(&(c.os_info as u64).to_le_bytes());
    b[8] = c.ctl as u8;
    b[9] = c.admin as u8;
    b[12..16].copy_from_slice(&c.gpu_mask.to_le_bytes());
    b[16..20].copy_from_slice(&c.pid.to_le_bytes());
    b[24..].copy_from_slice(&c.name);
    b
}

fn decode_caller(b: &[u8]) -> Option<Caller> {
    let flag = |v: u8| match v {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    };
    let mut name = [0u8; CALLER_NAME_LEN];
    name.copy_from_slice(&b[24..CALLER_LEN]);
    // The RM prints it with `%s`: it always ends.
    name[CALLER_NAME_LEN - 1] = 0;
    Some(Caller {
        os_info: usize::try_from(u64_at(b, 0)).ok()?,
        ctl: flag(b[8])?,
        admin: flag(b[9])?,
        gpu_mask: u32_at(b, 12),
        pid: u32_at(b, 16),
        name,
    })
}

#[cfg(test)]
mod tests;
