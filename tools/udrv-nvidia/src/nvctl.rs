//! The RM this process runs, served to the kernel's `/dev/nvidiactl` and
//! `/dev/nvidia<N>` (`ECLIPSE_NVCTL_SERVE`): every escape the device files
//! pass to the RM is a request here, answered by
//! `nvctl::remote::answer` on the driver's RM, which does not know the
//! difference. What the RM reads and writes of the caller's memory, the
//! kernel copies for it while the request waits; pages of the caller's it
//! is lent (an OS descriptor) go back to the kernel when it lets them go,
//! and what it posts to an OS event goes to the kernel's open.
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use udrivers::served::{caller_read, caller_write, Msg, Node, SLOT_LEN};
use udrivers::zx::Status;
use zcore_drivers::display::nvctl::remote::{self, Rm, EINVAL};

/// The `reqid` of a message the kernel waits on no answer for.
const NO_REPLY: u32 = u32::MAX;

/// Serve `rm`, saying `hello` of it, until the kernel lets go. Only
/// returns then, with why; `Err` when it could not be served at all, with
/// the kernel's status.
pub fn serve(rm: &dyn Rm, hello: &[u8]) -> Result<String, (Status, String)> {
    nvidia_rm_sys::frontend::set_user_copy(copy);
    remote::set_give_back(give_back);
    zcore_drivers::display::nvctl::set_event_sink(post_event);
    let node = Node::serve_nvctl(hello)?;
    let why = node.run_with(&Mutex::new(()), |_, node, req| {
        let answer = reply(rm, node, req);
        (req.reqid != NO_REPLY).then_some(answer)
    });
    Ok(format!("{why}"))
}

/// The RM's copy of the caller's memory (`os_memcpy_from_user` and
/// `os_memcpy_to_user`): the kernel does it, on the request being answered.
pub fn copy(to_user: bool, user: usize, ours: *mut u8, len: usize) -> bool {
    // SAFETY: `ours` is the RM's buffer of `len` bytes.
    let ours = unsafe { core::slice::from_raw_parts_mut(ours, len) };
    let done = if to_user {
        caller_write(user as u64, ours)
    } else {
        caller_read(user as u64, ours)
    };
    done == Ok(len)
}

/// The last loan given back, and how many the kernel refused.
pub static GIVEN_BACK: AtomicU64 = AtomicU64::new(0);
pub static GIVE_BACK_REFUSED: AtomicU32 = AtomicU32::new(0);

/// The RM is done with the pages of `loan` (it unlocked them, or never
/// took them): they are the kernel's again.
fn give_back(loan: u64) {
    match Node::give_back_nvctl(loan) {
        Ok(()) => GIVEN_BACK.store(loan, Ordering::Relaxed),
        Err(e) => {
            GIVE_BACK_REFUSED.fetch_add(1, Ordering::Relaxed);
            eprintln!("udrv-nvidia: the kernel took no loan {loan} back: {e}");
        }
    }
}

/// How many OS events were posted to the kernel, and how many it refused.
pub static POSTED: AtomicU32 = AtomicU32::new(0);
pub static POST_REFUSED: AtomicU32 = AtomicU32::new(0);

/// The RM's registrations post `event` to the open `os_info`
/// (`nv_post_event`): it is the kernel's to queue there.
fn post_event(os_info: usize, event: Option<zcore_drivers::display::nvctl::UnixEvent>) {
    let data = event.map(|e| (e.h_object, e.notify_index, e.info32, e.info16));
    match Node::post_nvctl_event(os_info, data) {
        Ok(()) => {
            POSTED.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => {
            POST_REFUSED.fetch_add(1, Ordering::Relaxed);
            eprintln!("udrv-nvidia: the kernel took no OS event for {os_info:#x}: {e}");
        }
    }
}

/// The answer to one request, its bytes put in its slot.
fn reply(rm: &dyn Rm, node: &Node, req: &Msg) -> Msg {
    let slot = node.slot_of(req);
    let carried = (req.len as usize).min(SLOT_LEN);
    let mut out = Vec::new();
    let result = {
        let _reaching = node.reaching(req);
        remote::answer(rm, req.op, req.a, &slot[..carried], &mut out)
    };
    let mut reply = Msg {
        op: req.op,
        reqid: req.reqid,
        ..Msg::default()
    };
    match result {
        Ok(word) if out.len() <= SLOT_LEN => {
            slot[..out.len()].copy_from_slice(&out);
            reply.a = word;
            reply.len = out.len() as u32;
        }
        Ok(_) => reply.b = (-(EINVAL as i64)) as u64,
        Err(errno) => reply.b = (-(errno as i64)) as u64,
    }
    reply
}

/// An RM with nothing behind it, for `udrv-nvidia nvctl-check`: an
/// `RM_CONTROL` adds one to each word of its parameters, read and written
/// back where the caller's pointer says, as the RM follows one; an
/// `RM_MAP_MEMORY` grants the mapping its `hMemory` names ([`grant`]); an
/// `RM_ALLOC_MEMORY` of an OS descriptor keeps the pages it is lent until
/// `RM_FREE` of its handle or the close, or refuses them for handle
/// [`REFUSED`]. Its OS events are the RM's own registrations, and its
/// nonstall interrupt is pending once after each: a look at it posts
/// [`EVENT`] to the last one registered, as an engine's would. UVM's
/// calls each make a handle; a semaphore pool's pages are held from the
/// memory made of them until it is freed.
pub mod check {
    use nvidia_rm_sys::frontend::events;
    use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::sync::Mutex;
    use zcore_drivers::display::nvctl::remote::{hello_of, Rm, ServedCard};
    use zcore_drivers::display::nvctl::{
        Caller, Card, ExternalMapping, MmapContext, NonstallPoll, Pages, RawMmapContext, UserPages,
    };

    /// `NV_ESC_RM_CONTROL`, and `NVOS54_PARAMETERS`' size.
    pub const RM_CONTROL: u32 = 0x2a;
    pub const NVOS54_LEN: usize = 32;
    /// `NV_ESC_RM_ALLOC_MEMORY` (`NVOS02_PARAMETERS`, 56 bytes) and
    /// `NV_ESC_RM_FREE` (`NVOS00_PARAMETERS`, 16).
    pub const RM_ALLOC_MEMORY: u32 = 0x27;
    pub const RM_FREE: u32 = 0x29;
    /// The `hObjectNew` whose pages it refuses.
    pub const REFUSED: u32 = 0xbad;
    /// What its nonstall interrupt posts: `hObject`, notifier index,
    /// `info32`, `info16`.
    pub const EVENT: (u32, u32, u32, u16) = (0xe7e7, 3, 0x1234_5678, 0x9abc);
    /// `NV_OK`, and the status of an escape it does not know.
    const NV_OK: u32 = 0;
    const NV_ERR_NOT_SUPPORTED: u32 = 0x56;
    /// The most parameter bytes it takes.
    const PARAMS_MAX: usize = 4096;

    /// What an `RM_MAP_MEMORY`'s `hMemory` asks for.
    pub mod grant {
        /// A page of system memory, at [`super::CheckRm::system`].
        pub const SYSTEM: u32 = 1;
        /// A page of the framebuffer aperture, at [`super::CheckRm::bar`].
        pub const APERTURE: u32 = 2;
        /// A page of system memory this process never pinned.
        pub const NOT_OURS: u32 = 3;
        /// The first page it was last lent, at [`super::CheckRm::lent`].
        pub const LENT: u32 = 4;
    }

    #[derive(Default)]
    pub struct CheckRm {
        pub escapes: AtomicU32,
        pub released: AtomicUsize,
        /// Where its system memory and its aperture are.
        pub system: AtomicU64,
        pub bar: AtomicU64,
        pub unrecorded: AtomicU32,
        /// The first page it was last lent, and how many.
        pub lent: AtomicU64,
        pub lent_pages: AtomicUsize,
        /// What it keeps, by handle.
        pub kept: Mutex<Vec<(u32, UserPages)>>,
        /// The last OS event registered, as (client, fd), while its
        /// nonstall interrupt is pending; and how many looks it has had.
        pub pending: Mutex<Option<(u32, u32)>>,
        pub looks: AtomicU32,
        /// UVM: handles made, the pool's pages it holds by the memory's
        /// handle, and the size of the last block.
        pub uvm_handles: AtomicU32,
        pub pools: Mutex<Vec<(u32, Arc<dyn std::any::Any + Send + Sync>)>>,
        pub pool_size: AtomicU64,
        pub mapped: AtomicU32,
    }

    impl CheckRm {
        fn handle(&self) -> u32 {
            0xd000 + self.uvm_handles.fetch_add(1, Ordering::Relaxed)
        }
    }

    fn mapping(ctl: bool, memory_type: u32, caching: u32, start: u64) -> Option<MmapContext> {
        MmapContext::from_raw(&RawMmapContext {
            valid: 1,
            ctl: ctl as u32,
            gpu_instance: 0,
            memory_type,
            caching,
            prot: 2,
            start,
            size: 4096,
        })
    }

    /// One GPU, up, with an RM id and UUID only this check uses.
    pub fn hello() -> Vec<u8> {
        let card = Card {
            minor: 0,
            domain: 0,
            bus: 0x5a,
            slot: 0,
            device_id: 0x5a5a,
            reg_address: 0,
            reg_size: 0,
            fb_address: 0,
            fb_size: 0,
            instance: Some(0),
        };
        hello_of(
            true,
            &[ServedCard {
                card,
                gpu_id: 0x5a00,
                uuid: Some([0x5a; 16]),
            }],
        )
    }

    impl Rm for CheckRm {
        fn escape(&self, cmd: u32, payload: &mut [u8], _caller: &Caller) -> u32 {
            self.escapes.fetch_add(1, Ordering::Relaxed);
            if cmd == RM_FREE && payload.len() == 16 {
                let h = u32::from_le_bytes(payload[8..12].try_into().unwrap());
                let mut kept = self.kept.lock().unwrap();
                let before = kept.len();
                // Freed: its pages go back as they are dropped.
                kept.retain(|(of, _)| *of != h);
                let status = if kept.len() < before { NV_OK } else { 0x57 };
                payload[12..16].copy_from_slice(&status.to_le_bytes());
                return NV_OK;
            }
            if cmd != RM_CONTROL || payload.len() != NVOS54_LEN {
                return NV_ERR_NOT_SUPPORTED;
            }
            let params = u64::from_le_bytes(payload[16..24].try_into().unwrap()) as usize;
            let size = u32::from_le_bytes(payload[24..28].try_into().unwrap()) as usize;
            if size > PARAMS_MAX || !size.is_multiple_of(4) {
                return NV_ERR_NOT_SUPPORTED;
            }
            let mut buf = vec![0u8; size];
            let status = if !super::copy(false, params, buf.as_mut_ptr(), size) {
                0x1d // NV_ERR_INVALID_ADDRESS
            } else {
                for w in buf.as_chunks_mut::<4>().0 {
                    *w = u32::from_le_bytes(*w).wrapping_add(1).to_le_bytes();
                }
                if super::copy(true, params, buf.as_mut_ptr(), size) {
                    NV_OK
                } else {
                    0x1d
                }
            };
            payload[28..32].copy_from_slice(&status.to_le_bytes());
            status
        }

        fn release(&self, os_info: usize) {
            self.released.store(os_info, Ordering::Relaxed);
            self.kept.lock().unwrap().clear();
        }

        fn escape_lending(
            &self,
            cmd: u32,
            payload: &mut [u8],
            _caller: &Caller,
            pages: UserPages,
        ) -> u32 {
            self.escapes.fetch_add(1, Ordering::Relaxed);
            if cmd != RM_ALLOC_MEMORY || payload.len() != 56 {
                return NV_ERR_NOT_SUPPORTED;
            }
            let h = u32::from_le_bytes(payload[8..12].try_into().unwrap());
            self.lent
                .store(pages.phys.first().copied().unwrap_or(0), Ordering::Relaxed);
            self.lent_pages.store(pages.phys.len(), Ordering::Relaxed);
            let status = if h == REFUSED {
                0x1d // NV_ERR_INVALID_ADDRESS, the pages dropped untaken
            } else {
                self.kept.lock().unwrap().push((h, pages));
                NV_OK
            };
            payload[40..44].copy_from_slice(&status.to_le_bytes());
            NV_OK
        }

        fn map_memory(&self, payload: &mut [u8], _caller: &Caller) -> (u32, Option<MmapContext>) {
            self.escapes.fetch_add(1, Ordering::Relaxed);
            if payload.len() != 56 {
                return (NV_ERR_NOT_SUPPORTED, None);
            }
            let h_memory = u32::from_le_bytes(payload[8..12].try_into().unwrap());
            // `NV_MEMORY_CACHED` and `_DEFAULT`, the framebuffer's.
            let granted = match h_memory {
                grant::SYSTEM => mapping(true, 0, 0, self.system.load(Ordering::Relaxed)),
                grant::APERTURE => mapping(false, 2, 6, self.bar.load(Ordering::Relaxed)),
                grant::NOT_OURS => mapping(true, 0, 0, 0x1000),
                grant::LENT => mapping(true, 0, 0, self.lent.load(Ordering::Relaxed)),
                _ => None,
            };
            payload[40..44].copy_from_slice(&NV_OK.to_le_bytes());
            (NV_OK, granted)
        }

        fn map_unrecorded(&self, _payload: &mut [u8], _caller: &Caller, why: u32) {
            self.unrecorded.store(why, Ordering::Relaxed);
        }

        fn alloc_os_event(&self, client: u32, os_info: usize, fd: u32) -> u32 {
            let status = events::alloc_os_event(client, os_info, fd);
            if status == NV_OK {
                *self.pending.lock().unwrap() = Some((client, fd));
            }
            status
        }

        fn free_os_event(&self, client: u32, fd: u32) -> u32 {
            events::free_os_event(client, fd)
        }

        fn service_nonstall(&self) -> NonstallPoll {
            self.looks.fetch_add(1, Ordering::Relaxed);
            let Some((client, fd)) = self.pending.lock().unwrap().take() else {
                return NonstallPoll::Serviced(0);
            };
            // As the RM does: the event object holds the registration it
            // names by fd, and posts to it.
            let mut event = core::ptr::null_mut();
            let status = events::osUserHandleToKernelPtr(client, fd as usize as _, &mut event);
            if status != NV_OK {
                return NonstallPoll::Failed(status);
            }
            let (h, index, info32, info16) = EVENT;
            events::eclipse_rm_frontend_post_event(event, h, index, info32, info16, 1);
            events::osDereferenceObjectCount(event);
            NonstallPoll::Serviced(1)
        }

        fn uvm_va_space_device(
            &self,
            _caller: &Caller,
            _h_client: u32,
            _h_va_space: u32,
            _gpu_instance: u32,
        ) -> Result<u32, u32> {
            Ok(self.handle())
        }

        fn uvm_map_external(&self, _caller: &Caller, _m: &ExternalMapping) -> Result<u32, u32> {
            self.mapped.fetch_add(1, Ordering::Relaxed);
            Ok(self.handle())
        }

        fn uvm_free(&self, _caller: &Caller, _h_client: u32, h_object: u32) -> u32 {
            // The memory made of a pool goes, and the RM's hold on its pages.
            self.pools.lock().unwrap().retain(|(h, _)| *h != h_object);
            NV_OK
        }

        fn uvm_pages(
            &self,
            _caller: &Caller,
            _h_client: u32,
            _h_device: u32,
            pages: &Pages,
            _gpu_cached: bool,
        ) -> Result<u32, u32> {
            let h = self.handle();
            self.pool_size.store(pages.size, Ordering::Relaxed);
            self.pools.lock().unwrap().push((h, pages.keep.clone()));
            Ok(h)
        }
    }
}
