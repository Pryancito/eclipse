//! A [`DrmScheme`] whose driver is in another process: what the kernel's
//! DRM core asks of a card, as messages, and the two ends that speak them.
//!
//! [`Remote`] is the kernel's end. It is a card like any other to the DRM
//! core, and each call it gets it sends over a [`Transport`] (in the
//! kernel, a served device's link) and waits for the answer. [`answer`] is
//! the other end, in the driver's process: it takes one message and
//! makes the call on the driver itself, unchanged (`udrv-nvidia`'s
//! `NvidiaGpu`). Both are here, in the crate both ends link, so the format
//! has one definition and is tested end to end on the host.
//!
//! What this covers is the calls whose arguments and answers are values:
//! the card's resources, connectors, CRTCs and planes, framebuffers made
//! from GEM handles, flips, the cursor, modes; and the driver's own
//! ioctls, whose arguments are copied both ways ([`ioctl`]). The GEM and
//! fence registries both ends share come in their own steps.
//!
//! A message is an op, two words `a` and `b`, and bytes; an answer is a
//! word, bytes, or an `errno`. Numbers in the bytes are little-endian.

use alloc::string::String;
use alloc::vec::Vec;

use super::drm::{
    DisplayMode, DrmCaps, DrmConnector, DrmCrtc, DrmPlane, DrmScheme, GemAperture, GemHandle,
};
use super::Scheme;

/// The ops, numbered past those of a served device node (`OP_OPEN` and
/// the rest, below 16), so one link can never mistake one for the other.
pub mod op {
    /// The card's name, PCI place and capabilities: asked once, when the
    /// card is made.
    pub const HELLO: u32 = 64;
    /// The answers that change as the GPU comes up ([`super::Flags`]).
    pub const FLAGS: u32 = 65;
    pub const RESOURCES: u32 = 66;
    pub const CONNECTOR: u32 = 67;
    pub const CRTC: u32 = 68;
    pub const PLANE: u32 = 69;
    pub const PLANES: u32 = 70;
    pub const CREATE_FB: u32 = 71;
    pub const PAGE_FLIP: u32 = 72;
    pub const SET_CURSOR: u32 = 73;
    pub const WAIT_VBLANK: u32 = 74;
    pub const SET_PLANE: u32 = 75;
    pub const MODES: u32 = 76;
    pub const SET_MODE: u32 = 77;
    pub const HW_CURSOR_SET: u32 = 78;
    pub const HW_CURSOR_MOVE: u32 = 79;
    pub const HW_CURSOR_HIDE: u32 = 80;
    pub const POLL: u32 = 81;
    pub const EDID: u32 = 82;
    /// A driver's own ioctl: [`ioctl`](super::ioctl) says what travels.
    pub const IOCTL: u32 = 83;
    /// A DRM file closed (`a`, as `ioctl_file` names it). Told, not asked.
    pub const FILE_CLOSED: u32 = 84;
    /// A process (`a`) is gone. Told, not asked.
    pub const RELEASE_PROCESS: u32 = 85;
    /// `nouveau_gem_close` of handle `a` by holder `b`: 1 if it was the
    /// driver's.
    pub const GEM_CLOSE: u32 = 86;
    /// `gem_aperture` of handle `a` for `b`: 0 unknown, 1 sysmem, 2 VRAM.
    pub const GEM_APERTURE: u32 = 87;
    /// `import_buffer` and `free_buffer` of handle `a`, `b` bytes, at the
    /// physical address in the bytes (u64).
    pub const IMPORT_BUFFER: u32 = 88;
    pub const FREE_BUFFER: u32 = 89;
    /// `exec_cpu_waits` of the EXEC in the bytes (an [`ioctl`](super::ioctl)
    /// message of it) by `a`: 1 and the waits (a list of handles, then a
    /// u64 point for each), or 0 for none.
    pub const EXEC_CPU_WAITS: u32 = 90;
    /// `cpu_prep_fences` of handle `a` for `b`: a [fence list](super::fences_out).
    pub const CPU_PREP_FENCES: u32 = 91;
    /// `render_fence_for_scanout` of handle `a` for `b`: a fence list of
    /// one, or none.
    pub const RENDER_FENCE: u32 = 92;
    /// `queued_fences` of handle `a`: a fence list.
    pub const QUEUED_FENCES: u32 = 93;
    /// `ce_present` of `b` bytes at physical address `a`: 1 if the GPU's
    /// copy engine copied them.
    pub const CE_PRESENT: u32 = 94;
    /// `ce_present_2d_pitched` from physical address `a` to scanout offset
    /// `b`; the bytes are the source pitch, the scanout pitch, the bytes of
    /// a row and the rows (u32 each).
    pub const CE_PRESENT_2D: u32 = 95;
    /// `deferred_console_bringup`, with the console-GSP gate opened first
    /// on the driver's side when `a` is 1 (the kernel opened its own): the
    /// line it says, in the bytes.
    pub const CONSOLE_BRINGUP: u32 = 96;
    /// `build_console_gr_ladder`: the line it says, in the bytes.
    pub const CONSOLE_GR_LADDER: u32 = 97;
    /// `nvkms_load`: the lines it says, each ended by a newline, in the
    /// bytes.
    pub const NVKMS_LOAD: u32 = 98;
    /// `scanout_liveness`: the phase's name in the bytes, and what the
    /// driver says of its heads in the answer's.
    pub const SCANOUT_LIVENESS: u32 = 99;
}

pub mod ioctl;

/// A driver's fence as it travels: its landing zone by the physical
/// address under the driver's mapping of it (the kernel's is another), the
/// semaphore's GPU VA, the payload and the driver's channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fence {
    pub phys: u64,
    pub gpu_va: u64,
    pub payload: u32,
    pub ctx: u32,
}

/// A fence list: a count, then 24 bytes for each [`Fence`].
fn fences_out(o: &mut Out, fences: &[Fence]) {
    o.u32(fences.len() as u32);
    for f in fences {
        o.u64(f.phys).u64(f.gpu_va).u32(f.payload).u32(f.ctx);
    }
}

fn fences_in(i: &mut In) -> Option<Vec<Fence>> {
    let n = i.u32()? as usize;
    if n > i.0.len() / 24 {
        return None;
    }
    (0..n)
        .map(|_| {
            Some(Fence {
                phys: i.u64()?,
                gpu_va: i.u64()?,
                payload: i.u32()?,
                ctx: i.u32()?,
            })
        })
        .collect()
}

/// The driver's fence at its own mapping `va`, as it travels.
fn fence_of(va: usize, gpu_va: u64, payload: u32, ctx: u32) -> Fence {
    // Only a driver in a process answers for real; a kernel built without
    // one (its tests) has no mapping to undo, and the address is the one.
    #[cfg(feature = "process")]
    let phys = crate::bus::virt_to_phys(va) as u64;
    #[cfg(not(feature = "process"))]
    let phys = va as u64;
    Fence {
        phys,
        gpu_va,
        payload,
        ctx,
    }
}

/// The `errno` of a call the other end does not know, or whose answer did
/// not parse.
pub const EINVAL: i32 = 22;
pub const ENOENT: i32 = 2;
/// The card's driver is gone.
pub const ENODEV: i32 = 19;

/// The boolean answers of a card, one bit each, in [`op::FLAGS`]'s word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flags(pub u64);

impl Flags {
    pub const HARDWARE_KMS: u64 = 1 << 0;
    pub const DRIVES_OWN_DISPLAY: u64 = 1 << 1;
    pub const CONSOLE_GPU: u64 = 1 << 2;
    pub const COMPUTE_GPU: u64 = 1 << 3;
    pub const NOUVEAU_CAPABLE: u64 = 1 << 4;
    pub const NOUVEAU_READY: u64 = 1 << 5;
    pub const HW_VBLANK: u64 = 1 << 6;
    pub const BLOCK_LINEAR: u64 = 1 << 7;
    pub const CE_PRESENT_READY: u64 = 1 << 8;

    /// What `gpu` answers now. The driver's end sends it with every
    /// answer ([`Transport::flags`]).
    pub fn of(gpu: &dyn DrmScheme) -> Flags {
        let bits = [
            (gpu.has_hardware_kms(), Self::HARDWARE_KMS),
            (gpu.drives_own_display(), Self::DRIVES_OWN_DISPLAY),
            (gpu.is_console_gpu(), Self::CONSOLE_GPU),
            (gpu.is_compute_gpu(), Self::COMPUTE_GPU),
            (gpu.nouveau_uapi_capable(), Self::NOUVEAU_CAPABLE),
            (gpu.nouveau_uapi_ready(), Self::NOUVEAU_READY),
            (gpu.has_hw_vblank(), Self::HW_VBLANK),
            (gpu.scans_out_block_linear(), Self::BLOCK_LINEAR),
            (gpu.ce_present_ready(), Self::CE_PRESENT_READY),
        ];
        Flags(
            bits.iter()
                .filter(|(on, _)| *on)
                .fold(0, |all, (_, bit)| all | bit),
        )
    }

    fn has(self, bit: u64) -> bool {
        self.0 & bit != 0
    }
}

// --- the bytes ---

struct Out<'a>(&'a mut Vec<u8>);

impl Out<'_> {
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn i32(&mut self, v: i32) -> &mut Self {
        self.u32(v as u32)
    }
    fn bool(&mut self, v: bool) -> &mut Self {
        self.u32(v as u32)
    }
    fn list(&mut self, v: &[u32]) -> &mut Self {
        self.u32(v.len() as u32);
        for &x in v {
            self.u32(x);
        }
        self
    }
    fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u32(v.len() as u32);
        self.0.extend_from_slice(v);
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
}

struct In<'a>(&'a [u8]);

impl In<'_> {
    fn u32(&mut self) -> Option<u32> {
        let (head, rest) = self.0.split_first_chunk::<4>()?;
        self.0 = rest;
        Some(u32::from_le_bytes(*head))
    }
    fn i32(&mut self) -> Option<i32> {
        self.u32().map(|v| v as i32)
    }
    fn bool(&mut self) -> Option<bool> {
        self.u32().map(|v| v != 0)
    }
    fn list(&mut self) -> Option<Vec<u32>> {
        let n = self.u32()? as usize;
        if n > self.0.len() / 4 {
            return None;
        }
        (0..n).map(|_| self.u32()).collect()
    }
    fn u64(&mut self) -> Option<u64> {
        let (head, rest) = self.0.split_first_chunk::<8>()?;
        self.0 = rest;
        Some(u64::from_le_bytes(*head))
    }
    fn bytes(&mut self) -> Option<&[u8]> {
        let n = self.u32()? as usize;
        if n > self.0.len() {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }
}

fn caps_out(o: &mut Out, c: &DrmCaps) {
    o.bool(c.has_3d)
        .bool(c.has_cursor)
        .u32(c.max_width)
        .u32(c.max_height);
}

fn caps_in(i: &mut In) -> Option<DrmCaps> {
    Some(DrmCaps {
        has_3d: i.bool()?,
        has_cursor: i.bool()?,
        max_width: i.u32()?,
        max_height: i.u32()?,
    })
}

// --- the driver's end ---

/// Answer one message on `gpu`: the answer's word, with its bytes put in
/// `out`, or an `errno`.
pub fn answer(
    gpu: &dyn DrmScheme,
    op: u32,
    a: u64,
    b: u64,
    data: &[u8],
    out: &mut Vec<u8>,
) -> Result<u64, i32> {
    let mut o = Out(out);
    let mut i = In(data);
    let found = |yes: bool| if yes { Ok(1) } else { Ok(0) };
    match op {
        op::HELLO => {
            o.bytes(gpu.name().as_bytes());
            caps_out(&mut o, &gpu.get_caps());
            match gpu.pci_bdf() {
                Some((domain, bus, dev, func)) => {
                    o.bool(true)
                        .u32(domain)
                        .u32(bus as u32)
                        .u32(dev as u32)
                        .u32(func as u32);
                }
                None => {
                    o.bool(false);
                }
            }
            o.u32(Flags::of(gpu).0 as u32);
            Ok(0)
        }
        op::FLAGS => Ok(Flags::of(gpu).0),
        op::RESOURCES => {
            let (fbs, crtcs, connectors) = gpu.get_resources();
            o.list(&fbs).list(&crtcs).list(&connectors);
            Ok(0)
        }
        op::CONNECTOR => {
            let c = gpu.get_connector(a as u32).ok_or(ENOENT)?;
            o.u32(c.id)
                .bool(c.connected)
                .u32(c.mm_width)
                .u32(c.mm_height)
                .u32(c.connector_type);
            Ok(0)
        }
        op::CRTC => {
            let c = gpu.get_crtc(a as u32).ok_or(ENOENT)?;
            o.u32(c.id).u32(c.fb_id).u32(c.x).u32(c.y);
            Ok(0)
        }
        op::PLANE => {
            let p = gpu.get_plane(a as u32).ok_or(ENOENT)?;
            o.u32(p.id)
                .u32(p.crtc_id)
                .u32(p.fb_id)
                .u32(p.possible_crtcs)
                .u32(p.plane_type);
            Ok(0)
        }
        op::PLANES => {
            o.list(&gpu.get_planes());
            Ok(0)
        }
        op::CREATE_FB => {
            let (handle, w, h, pitch) = (i.u32(), i.u32(), i.u32(), i.u32());
            let tiled = i.u32();
            let (Some(handle), Some(w), Some(h), Some(pitch), Some(tiled)) =
                (handle, w, h, pitch, tiled)
            else {
                return Err(EINVAL);
            };
            // `tiled`: 0 for pitch-linear, else 1 + log2 of the GOBs a block is high.
            let gobs = tiled.checked_sub(1).map(|g| g as u8);
            gpu.create_fb_tiled(handle, w, h, pitch, gobs)
                .map(|fb| fb as u64)
                .ok_or(ENOENT)
        }
        op::PAGE_FLIP => found(gpu.page_flip(a as u32)),
        op::SET_CURSOR => {
            let v = (i.u32(), i.i32(), i.i32(), i.u32(), i.u32());
            let (Some(crtc), Some(x), Some(y), Some(handle), Some(flags)) = v else {
                return Err(EINVAL);
            };
            found(gpu.set_cursor(crtc, x, y, handle, flags))
        }
        op::WAIT_VBLANK => found(gpu.wait_vblank(a as u32)),
        op::SET_PLANE => {
            let mut v = [0u32; 11];
            for x in v.iter_mut() {
                *x = i.u32().ok_or(EINVAL)?;
            }
            found(gpu.set_plane(
                v[0],
                v[1],
                v[2],
                v[3] as i32,
                v[4] as i32,
                v[5],
                v[6],
                v[7],
                v[8],
                v[9],
                v[10],
            ))
        }
        op::MODES => {
            let modes = gpu.display_modes();
            o.u32(modes.len() as u32);
            for m in &modes {
                o.u32(m.width)
                    .u32(m.height)
                    .u32(m.refresh_hz)
                    .u32(m.pixel_clock_khz)
                    .bool(m.preferred);
            }
            Ok(0)
        }
        op::SET_MODE => {
            let v = (i.u32(), i.u32(), i.u32(), i.u32(), i.bool());
            let (Some(w), Some(h), Some(hz), Some(fb), Some(test)) = v else {
                return Err(EINVAL);
            };
            match gpu.set_display_mode(w, h, hz, fb, test) {
                None => Ok(0),
                Some(Ok(())) => Ok(1),
                Some(Err(why)) => {
                    o.bytes(why.as_bytes());
                    Ok(2)
                }
            }
        }
        op::HW_CURSOR_SET => {
            let (Some(w), Some(h)) = (i.u32(), i.u32()) else {
                return Err(EINVAL);
            };
            let argb: Vec<u32> = core::iter::from_fn(|| i.u32()).collect();
            found(gpu.hw_cursor_set(&argb, w, h))
        }
        op::HW_CURSOR_MOVE => found(gpu.hw_cursor_move(a as i32, b as i32)),
        op::CE_PRESENT => found(gpu.ce_present(a, b)),
        op::CE_PRESENT_2D => {
            let (Some(src_pitch), Some(dst_pitch), Some(row_bytes), Some(rows)) =
                (i.u32(), i.u32(), i.u32(), i.u32())
            else {
                return Err(EINVAL);
            };
            found(gpu.ce_present_2d_pitched(a, src_pitch, b, dst_pitch, row_bytes, rows))
        }
        op::CONSOLE_BRINGUP => {
            #[cfg(target_arch = "x86_64")]
            if a == 1 {
                crate::display::set_console_gsp_enabled(true);
            }
            o.0.extend_from_slice(gpu.deferred_console_bringup().as_bytes());
            Ok(0)
        }
        op::CONSOLE_GR_LADDER => {
            o.0.extend_from_slice(gpu.build_console_gr_ladder().as_bytes());
            Ok(0)
        }
        op::NVKMS_LOAD => {
            lines_out(o.0, &gpu.nvkms_load());
            Ok(0)
        }
        op::SCANOUT_LIVENESS => {
            let phase = core::str::from_utf8(data).map_err(|_| EINVAL)?;
            o.0.extend_from_slice(gpu.scanout_liveness(phase).as_bytes());
            Ok(0)
        }
        op::HW_CURSOR_HIDE => found(gpu.hw_cursor_hide()),
        op::POLL => found(gpu.poll_connectors()),
        op::EDID => {
            let edid = gpu.get_connector_edid(a as u32).ok_or(ENOENT)?;
            o.0.extend_from_slice(&edid);
            Ok(0)
        }
        op::IOCTL => {
            ioctl::answer(gpu, a as u32, b, data, o.0);
            Ok(0)
        }
        op::FILE_CLOSED => {
            gpu.nouveau_file_closed(a as usize);
            Ok(0)
        }
        op::RELEASE_PROCESS => {
            gpu.nouveau_release_process(a);
            Ok(0)
        }
        op::GEM_CLOSE => found(gpu.nouveau_gem_close(a as u32, b)),
        op::GEM_APERTURE => Ok(match gpu.gem_aperture(a as u32, b) {
            GemAperture::Unknown => 0,
            GemAperture::Sysmem => 1,
            GemAperture::Vram => 2,
        }),
        op::IMPORT_BUFFER | op::FREE_BUFFER => {
            let phys = data
                .get(..8)
                .map(|w| u64::from_le_bytes(core::convert::TryFrom::try_from(w).unwrap()))
                .ok_or(EINVAL)?;
            let handle = GemHandle {
                id: a as u32,
                size: b as usize,
                phys_addr: phys,
            };
            if op == op::IMPORT_BUFFER {
                found(gpu.import_buffer(handle))
            } else {
                gpu.free_buffer(handle);
                Ok(0)
            }
        }
        op::EXEC_CPU_WAITS => {
            let (waits, _) =
                ioctl::unpacked(ioctl::exec_cmd(), data, |at, _| gpu.exec_cpu_waits(at, a))?;
            match waits {
                Some((handles, points)) => {
                    o.list(&handles);
                    for p in points {
                        o.u64(p);
                    }
                    Ok(1)
                }
                None => Ok(0),
            }
        }
        op::CPU_PREP_FENCES => {
            let fences: Vec<Fence> = gpu
                .cpu_prep_fences(a as u32, b)
                .into_iter()
                .map(|(va, payload)| fence_of(va, 0, payload, 0))
                .collect();
            fences_out(&mut o, &fences);
            Ok(0)
        }
        op::RENDER_FENCE => {
            let fences: Vec<Fence> = gpu
                .render_fence_for_scanout(a as u32, b)
                .map(|(va, payload)| fence_of(va, 0, payload, 0))
                .into_iter()
                .collect();
            fences_out(&mut o, &fences);
            Ok(0)
        }
        op::QUEUED_FENCES => {
            let fences: Vec<Fence> = gpu
                .queued_fences(a as u32)
                .into_iter()
                .map(|f| fence_of(f.fence_va, f.fence_gpu_va, f.payload, f.ctx_idx))
                .collect();
            fences_out(&mut o, &fences);
            Ok(0)
        }
        _ => Err(EINVAL),
    }
}

/// The most text an answer of lines carries: half a slot of the link.
const LINES_MAX: usize = 32 * 1024;

/// Lines of text as an answer carries them, each ended by a newline. What
/// does not fit in [`LINES_MAX`] bytes is left out, and a last line says so.
fn lines_out(out: &mut Vec<u8>, lines: &[String]) {
    for line in lines {
        if out.len() + line.len() + 1 > LINES_MAX {
            out.extend_from_slice(b"...\n");
            break;
        }
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
}

/// What `gpu` says of itself, [`op::HELLO`]'s answer.
pub fn hello(gpu: &dyn DrmScheme) -> Vec<u8> {
    let mut out = Vec::new();
    let _ = answer(gpu, op::HELLO, 0, 0, &[], &mut out);
    out
}

// --- the kernel's end ---

/// How [`Remote`] reaches the driver: send `op` with `a`, `b` and `data`,
/// and wait for its answer, the word and the bytes.
pub trait Transport: Send + Sync {
    fn call(&self, op: u32, a: u64, b: u64, data: &[u8]) -> Result<(u64, Vec<u8>), i32>;

    /// The card's [`Flags`] as the last answer carried them, if answers
    /// carry them.
    fn flags(&self) -> Option<Flags> {
        None
    }

    /// Send `op` and wait for no answer: what a close or an exit says,
    /// which must not wait on the driver (the one exiting may be it).
    fn tell(&self, op: u32, a: u64, b: u64) {
        let _ = self.call(op, a, b, &[]);
    }

    /// `len` bytes of the caller's memory at `addr`, or `EFAULT`. None by
    /// default: a transport that cannot reach the caller carries no ioctl.
    fn read_user(&self, _addr: usize, _len: usize) -> Result<Vec<u8>, i32> {
        Err(ioctl::EFAULT)
    }

    /// Write `bytes` into the caller's memory at `addr`, or `EFAULT`.
    fn write_user(&self, _addr: usize, _bytes: &[u8]) -> Result<(), i32> {
        Err(ioctl::EFAULT)
    }

    /// The kernel's mapping of the driver's fence `f`, and the ctx the
    /// kernel's syncobj table knows its channel by: `None` when its landing
    /// zone is not memory the driver may name, and a fence the kernel
    /// cannot read is not waited for. Default: none.
    fn fence(&self, _f: &Fence) -> Option<(usize, u32)> {
        None
    }
}

/// A card whose driver answers through `T`.
pub struct Remote<T: Transport> {
    to: T,
    name: String,
    caps: DrmCaps,
    bdf: Option<(u32, u8, u8, u8)>,
    /// The flags as last heard: from the hello, then from each answer.
    /// Never asked for on their own: the DRM core asks them with its own
    /// lock held (`get_display_driver`), where waiting for another process
    /// would stop every other caller, and the driver itself if it needs
    /// that lock.
    flags: core::sync::atomic::AtomicU64,
}

impl<T: Transport> Remote<T> {
    /// Ask the driver who it is ([`op::HELLO`]): the card, or the errno.
    pub fn new(to: T) -> Result<Self, i32> {
        let (_, hello) = to.call(op::HELLO, 0, 0, &[])?;
        Self::with_hello(to, &hello)
    }

    /// The card the driver described with `hello`, its answer to
    /// [`op::HELLO`] ([`hello`]), sent some other way: with the call that
    /// serves the card, which the driver cannot answer while it makes it.
    pub fn with_hello(to: T, hello: &[u8]) -> Result<Self, i32> {
        let mut i = In(hello);
        let name = i.bytes().ok_or(EINVAL)?;
        let name = String::from_utf8_lossy(name).into_owned();
        let caps = caps_in(&mut i).ok_or(EINVAL)?;
        let bdf = if i.bool().ok_or(EINVAL)? {
            let v = (i.u32(), i.u32(), i.u32(), i.u32());
            let (Some(d), Some(b), Some(dv), Some(f)) = v else {
                return Err(EINVAL);
            };
            Some((d, b as u8, dv as u8, f as u8))
        } else {
            None
        };
        let flags = i.u32().ok_or(EINVAL)? as u64;
        Ok(Remote {
            to,
            name,
            caps,
            bdf,
            flags: core::sync::atomic::AtomicU64::new(flags),
        })
    }

    pub fn transport(&self) -> &T {
        &self.to
    }

    fn call(&self, op: u32, a: u64, b: u64, data: &[u8]) -> Result<(u64, Vec<u8>), i32> {
        let r = self.to.call(op, a, b, data);
        if let Some(f) = self.to.flags() {
            self.flags.store(f.0, core::sync::atomic::Ordering::Relaxed);
        }
        r
    }

    fn word(&self, op: u32, a: u64, b: u64, data: &[u8]) -> Option<u64> {
        self.call(op, a, b, data).ok().map(|(w, _)| w)
    }

    fn bytes(&self, op: u32, a: u64) -> Option<Vec<u8>> {
        self.call(op, a, 0, &[]).ok().map(|(_, b)| b)
    }

    /// A line of text the driver answers `op` with; empty when it cannot.
    fn line(&self, op: u32, a: u64) -> String {
        self.bytes(op, a)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    }

    fn yes(&self, op: u32, a: u64, b: u64, data: &[u8]) -> bool {
        self.word(op, a, b, data) == Some(1)
    }

    /// The fences op `op` names for handle `a` and `b`, each with the
    /// kernel's mapping and ctx of it ([`Transport::fence`]); one the
    /// kernel cannot read is left out, and said so.
    fn fences(&self, op: u32, a: u32, b: u64) -> Vec<(usize, u32, Fence)> {
        let Ok((_, back)) = self.call(op, a as u64, b, &[]) else {
            return Vec::new();
        };
        let Some(fences) = fences_in(&mut In(&back)) else {
            return Vec::new();
        };
        fences
            .into_iter()
            .filter_map(|f| match self.to.fence(&f) {
                Some((va, ctx)) => Some((va, ctx, f)),
                None => {
                    log::warn!(
                        "[drm-remote] {}: a fence at {:#x} that is not the driver's",
                        self.name,
                        f.phys
                    );
                    None
                }
            })
            .collect()
    }

    fn flag(&self, bit: u64) -> bool {
        Flags(self.flags.load(core::sync::atomic::Ordering::Relaxed)).has(bit)
    }

    /// Ask the driver for its flags now: outside every lock, as the DRM
    /// core's connector poll calls [`DrmScheme::poll_connectors`].
    pub fn refresh(&self) {
        let _ = self.call(op::FLAGS, 0, 0, &[]).map(|(f, _)| {
            self.flags.store(f, core::sync::atomic::Ordering::Relaxed);
        });
    }
}

impl<T: Transport> Scheme for Remote<T> {
    fn name(&self) -> &str {
        &self.name
    }
}

impl<T: Transport> DrmScheme for Remote<T> {
    fn get_caps(&self) -> DrmCaps {
        self.caps
    }

    fn pci_bdf(&self) -> Option<(u32, u8, u8, u8)> {
        self.bdf
    }

    fn nouveau_uapi_capable(&self) -> bool {
        self.flag(Flags::NOUVEAU_CAPABLE)
    }

    fn nouveau_uapi_ready(&self) -> bool {
        self.flag(Flags::NOUVEAU_READY)
    }

    fn is_console_gpu(&self) -> bool {
        self.flag(Flags::CONSOLE_GPU)
    }

    fn is_compute_gpu(&self) -> bool {
        self.flag(Flags::COMPUTE_GPU)
    }

    fn has_hardware_kms(&self) -> bool {
        self.flag(Flags::HARDWARE_KMS)
    }

    fn has_hw_vblank(&self) -> bool {
        self.flag(Flags::HW_VBLANK)
    }

    fn drives_own_display(&self) -> bool {
        self.flag(Flags::DRIVES_OWN_DISPLAY)
    }

    fn scans_out_block_linear(&self) -> bool {
        self.flag(Flags::BLOCK_LINEAR)
    }

    /// What the driver last said: a GPU of the process's whose RM is up
    /// copies the frame with its copy engine ([`op::CE_PRESENT`]).
    fn ce_present_ready(&self) -> bool {
        self.flag(Flags::CE_PRESENT_READY)
    }

    /// The copy is the process's GPU's: one call a frame, for the copy the
    /// CPU would otherwise make of the whole frame.
    fn ce_present(&self, src_sysmem_pa: u64, size: u64) -> bool {
        // A card that is not ready declines without a call: the kernel
        // offers every frame to every card.
        self.ce_present_ready() && self.yes(op::CE_PRESENT, src_sysmem_pa, size, &[])
    }

    /// The console GPU of the process brought up as the kernel's would
    /// be, in the window the kernel's task holds the scanout for. The
    /// console-GSP gate the kernel opened for it is opened there too.
    fn deferred_console_bringup(&self) -> String {
        #[cfg(target_arch = "x86_64")]
        let gate = crate::display::console_gsp_enabled() as u64;
        #[cfg(not(target_arch = "x86_64"))]
        let gate = 0;
        self.line(op::CONSOLE_BRINGUP, gate)
    }

    fn build_console_gr_ladder(&self) -> String {
        self.line(op::CONSOLE_GR_LADDER, 0)
    }

    /// The heads of the driver's console GPU, read where the driver is:
    /// whether its raster still runs after the bring-up, and after
    /// nvidia-modeset's modeset, is what the log of a boot whose monitor
    /// went black has to say.
    fn scanout_liveness(&self, phase: &str) -> String {
        self.call(op::SCANOUT_LIVENESS, 0, 0, phase.as_bytes())
            .map(|(_, text)| String::from_utf8_lossy(&text).into_owned())
            .unwrap_or_default()
    }

    /// `nvidia.nvkms`: nvidia-modeset is loaded where the driver is, and
    /// takes this card there; what it says of it comes back. On the card
    /// that scans out the console this is the modeset that puts the
    /// console back on the monitor after the GPU's bring-up, which nothing
    /// else does. Its timers run in the driver's process, as Linux runs
    /// them from its own thread, so [`DrmScheme::nvkms_service`] stays
    /// `None` here.
    fn nvkms_load(&self) -> Vec<String> {
        match self.call(op::NVKMS_LOAD, 0, 0, &[]) {
            Ok((_, text)) => String::from_utf8_lossy(&text)
                .lines()
                .map(String::from)
                .collect(),
            Err(errno) => alloc::vec![alloc::format!(
                "{}: its driver's process did not load nvkms (errno {})",
                self.name,
                errno
            )],
        }
    }

    fn ce_present_2d_pitched(
        &self,
        src_sysmem_pa: u64,
        src_pitch: u32,
        dst_byte_offset: u64,
        dst_pitch: u32,
        row_bytes: u32,
        line_count: u32,
    ) -> bool {
        if !self.ce_present_ready() {
            return false;
        }
        let mut data = Vec::new();
        Out(&mut data)
            .u32(src_pitch)
            .u32(dst_pitch)
            .u32(row_bytes)
            .u32(line_count);
        self.yes(op::CE_PRESENT_2D, src_sysmem_pa, dst_byte_offset, &data)
    }

    fn get_connector_edid(&self, id: u32) -> Option<[u8; 128]> {
        core::convert::TryFrom::try_from(self.bytes(op::EDID, id as u64)?).ok()
    }

    fn ioctl(&self, request: u32, arg: usize) -> Result<usize, i32> {
        self.ioctl_file(request, arg, 0, 0)
    }

    fn ioctl_owned(&self, request: u32, arg: usize, owner_pid: u64) -> Result<usize, i32> {
        self.ioctl_file(request, arg, owner_pid, 0)
    }

    fn ioctl_file(
        &self,
        request: u32,
        arg: usize,
        owner_pid: u64,
        file: usize,
    ) -> Result<usize, i32> {
        let send = |msg: &[u8]| {
            self.call(op::IOCTL, request as u64, owner_pid, msg)
                .map(|(_, back)| back)
        };
        ioctl::call(&self.to, send, request, arg, file)
    }

    fn nouveau_gem_close(&self, handle: u32, owner_pid: u64) -> bool {
        self.yes(op::GEM_CLOSE, handle as u64, owner_pid, &[])
    }

    fn gem_aperture(&self, handle_id: u32, pid: u64) -> GemAperture {
        match self.word(op::GEM_APERTURE, handle_id as u64, pid, &[]) {
            Some(1) => GemAperture::Sysmem,
            Some(2) => GemAperture::Vram,
            _ => GemAperture::Unknown,
        }
    }

    fn import_buffer(&self, handle: GemHandle) -> bool {
        let phys = handle.phys_addr.to_le_bytes();
        self.yes(
            op::IMPORT_BUFFER,
            handle.id as u64,
            handle.size as u64,
            &phys,
        )
    }

    fn free_buffer(&self, handle: GemHandle) {
        let phys = handle.phys_addr.to_le_bytes();
        let _ = self.call(op::FREE_BUFFER, handle.id as u64, handle.size as u64, &phys);
    }

    fn exec_cpu_waits(&self, arg: usize, owner_pid: u64) -> Option<(Vec<u32>, Vec<u64>)> {
        let (msg, _) = ioctl::message(&self.to, ioctl::exec_cmd(), arg, 0).ok()?;
        let (w, back) = self.call(op::EXEC_CPU_WAITS, owner_pid, 0, &msg).ok()?;
        if w != 1 {
            return None;
        }
        let mut i = In(&back);
        let handles = i.list()?;
        let points = (0..handles.len())
            .map(|_| i.u64())
            .collect::<Option<Vec<u64>>>()?;
        Some((handles, points))
    }

    fn cpu_prep_fences(&self, gem_handle: u32, owner_pid: u64) -> Vec<(usize, u32)> {
        self.fences(op::CPU_PREP_FENCES, gem_handle, owner_pid)
            .into_iter()
            .map(|(va, _, f)| (va, f.payload))
            .collect()
    }

    fn render_fence_for_scanout(&self, gem_handle: u32, owner_pid: u64) -> Option<(usize, u32)> {
        self.fences(op::RENDER_FENCE, gem_handle, owner_pid)
            .into_iter()
            .next()
            .map(|(va, _, f)| (va, f.payload))
    }

    fn queued_fences(&self, gem_handle: u32) -> Vec<crate::scheme::syncobj::HwFence> {
        self.fences(op::QUEUED_FENCES, gem_handle, 0)
            .into_iter()
            .map(|(va, ctx, f)| crate::scheme::syncobj::HwFence {
                fence_va: va,
                fence_gpu_va: f.gpu_va,
                payload: f.payload,
                ctx_idx: ctx,
            })
            .collect()
    }

    fn nouveau_file_closed(&self, file: usize) {
        self.to.tell(op::FILE_CLOSED, file as u64, 0);
    }

    fn nouveau_release_process(&self, pid: u64) {
        self.to.tell(op::RELEASE_PROCESS, pid, 0);
    }

    fn display_modes(&self) -> Vec<DisplayMode> {
        let Some(bytes) = self.bytes(op::MODES, 0) else {
            return Vec::new();
        };
        let mut i = In(&bytes);
        let n = i.u32().unwrap_or(0);
        (0..n)
            .map_while(|_| {
                Some(DisplayMode {
                    width: i.u32()?,
                    height: i.u32()?,
                    refresh_hz: i.u32()?,
                    pixel_clock_khz: i.u32()?,
                    preferred: i.bool()?,
                })
            })
            .collect()
    }

    fn poll_connectors(&self) -> bool {
        // Called every few seconds with no DRM lock held: when the flags
        // that change as the GPU comes up are asked again, if no answer
        // has brought them meanwhile.
        self.refresh();
        self.yes(op::POLL, 0, 0, &[])
    }

    fn create_fb_tiled(
        &self,
        handle_id: u32,
        width: u32,
        height: u32,
        pitch: u32,
        log2_gobs_per_block_y: Option<u8>,
    ) -> Option<u32> {
        let mut data = Vec::new();
        let tiled = log2_gobs_per_block_y.map_or(0, |g| g as u32 + 1);
        Out(&mut data)
            .u32(handle_id)
            .u32(width)
            .u32(height)
            .u32(pitch)
            .u32(tiled);
        self.word(op::CREATE_FB, 0, 0, &data).map(|fb| fb as u32)
    }

    fn set_display_mode(
        &self,
        width: u32,
        height: u32,
        refresh_hz: u32,
        fb_id: u32,
        test_only: bool,
    ) -> Option<Result<(), String>> {
        let mut data = Vec::new();
        Out(&mut data)
            .u32(width)
            .u32(height)
            .u32(refresh_hz)
            .u32(fb_id)
            .bool(test_only);
        match self.call(op::SET_MODE, 0, 0, &data) {
            Ok((1, _)) => Some(Ok(())),
            Ok((2, why)) => {
                let why = In(&why)
                    .bytes()
                    .map(|w| String::from_utf8_lossy(w).into_owned())
                    .unwrap_or_default();
                Some(Err(why))
            }
            Ok(_) => None,
            Err(e) => Some(Err(alloc::format!("the driver's process: errno {e}"))),
        }
    }

    fn create_fb(&self, handle_id: u32, width: u32, height: u32, pitch: u32) -> Option<u32> {
        self.create_fb_tiled(handle_id, width, height, pitch, None)
    }

    fn page_flip(&self, fb_id: u32) -> bool {
        self.yes(op::PAGE_FLIP, fb_id as u64, 0, &[])
    }

    fn set_cursor(&self, crtc_id: u32, x: i32, y: i32, handle: u32, flags: u32) -> bool {
        let mut data = Vec::new();
        Out(&mut data)
            .u32(crtc_id)
            .i32(x)
            .i32(y)
            .u32(handle)
            .u32(flags);
        self.yes(op::SET_CURSOR, 0, 0, &data)
    }

    fn hw_cursor_set(&self, argb: &[u32], w: u32, h: u32) -> bool {
        let mut data = Vec::new();
        let mut o = Out(&mut data);
        o.u32(w).u32(h);
        for &px in argb {
            o.u32(px);
        }
        self.yes(op::HW_CURSOR_SET, 0, 0, &data)
    }

    fn hw_cursor_move(&self, x: i32, y: i32) -> bool {
        self.yes(op::HW_CURSOR_MOVE, x as u32 as u64, y as u32 as u64, &[])
    }

    fn hw_cursor_hide(&self) -> bool {
        self.yes(op::HW_CURSOR_HIDE, 0, 0, &[])
    }

    fn wait_vblank(&self, crtc_id: u32) -> bool {
        self.yes(op::WAIT_VBLANK, crtc_id as u64, 0, &[])
    }

    fn get_resources(&self) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
        let Some(bytes) = self.bytes(op::RESOURCES, 0) else {
            return (Vec::new(), Vec::new(), Vec::new());
        };
        let mut i = In(&bytes);
        match (i.list(), i.list(), i.list()) {
            (Some(f), Some(c), Some(n)) => (f, c, n),
            _ => (Vec::new(), Vec::new(), Vec::new()),
        }
    }

    fn get_connector(&self, id: u32) -> Option<DrmConnector> {
        let bytes = self.bytes(op::CONNECTOR, id as u64)?;
        let mut i = In(&bytes);
        Some(DrmConnector {
            id: i.u32()?,
            connected: i.bool()?,
            mm_width: i.u32()?,
            mm_height: i.u32()?,
            connector_type: i.u32()?,
        })
    }

    fn get_crtc(&self, id: u32) -> Option<DrmCrtc> {
        let bytes = self.bytes(op::CRTC, id as u64)?;
        let mut i = In(&bytes);
        Some(DrmCrtc {
            id: i.u32()?,
            fb_id: i.u32()?,
            x: i.u32()?,
            y: i.u32()?,
        })
    }

    fn get_plane(&self, id: u32) -> Option<DrmPlane> {
        let bytes = self.bytes(op::PLANE, id as u64)?;
        let mut i = In(&bytes);
        Some(DrmPlane {
            id: i.u32()?,
            crtc_id: i.u32()?,
            fb_id: i.u32()?,
            possible_crtcs: i.u32()?,
            plane_type: i.u32()?,
        })
    }

    fn get_planes(&self) -> Vec<u32> {
        self.bytes(op::PLANES, 0)
            .and_then(|b| In(&b).list())
            .unwrap_or_default()
    }

    fn set_plane(
        &self,
        plane_id: u32,
        crtc_id: u32,
        fb_id: u32,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        src_x: u32,
        src_y: u32,
        src_w: u32,
        src_h: u32,
    ) -> bool {
        let mut data = Vec::new();
        let mut o = Out(&mut data);
        for v in [
            plane_id, crtc_id, fb_id, x as u32, y as u32, w, h, src_x, src_y, src_w, src_h,
        ] {
            o.u32(v);
        }
        self.yes(op::SET_PLANE, 0, 0, &data)
    }
}

#[cfg(test)]
mod tests;
