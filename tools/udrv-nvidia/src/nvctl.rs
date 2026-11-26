//! The RM this process runs, served to the kernel's `/dev/nvidiactl` and
//! `/dev/nvidia<N>` (`ECLIPSE_NVCTL_SERVE`): every escape the device files
//! pass to the RM is a request here, answered by
//! `nvctl::remote::answer` on the driver's RM, which does not know the
//! difference. What the RM reads and writes of the caller's memory, the
//! kernel copies for it while the request waits.
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
/// `RM_MAP_MEMORY` grants the mapping its `hMemory` names ([`Grant`]).
pub mod check {
    use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
    use zcore_drivers::display::nvctl::remote::{hello_of, Rm, ServedCard};
    use zcore_drivers::display::nvctl::{Caller, Card, MmapContext, RawMmapContext};

    /// `NV_ESC_RM_CONTROL`, and `NVOS54_PARAMETERS`' size.
    pub const RM_CONTROL: u32 = 0x2a;
    pub const NVOS54_LEN: usize = 32;
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
    }

    #[derive(Default)]
    pub struct CheckRm {
        pub escapes: AtomicU32,
        pub released: AtomicUsize,
        /// Where its system memory and its aperture are.
        pub system: AtomicU64,
        pub bar: AtomicU64,
        pub unrecorded: AtomicU32,
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
                _ => None,
            };
            payload[40..44].copy_from_slice(&NV_OK.to_le_bytes());
            (NV_OK, granted)
        }

        fn map_unrecorded(&self, _payload: &mut [u8], _caller: &Caller, why: u32) {
            self.unrecorded.store(why, Ordering::Relaxed);
        }
    }
}
